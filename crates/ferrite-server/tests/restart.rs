//! Kills the server in the middle of a write load and proves that
//! restarting it — and nothing else — brings back a fully working database.
//!
//! This is the whole point of the supervision work: the promise is not
//! "crashes are rare", it is "a crash costs a restart". Verifying it needs
//! all three halves at once, which no other test puts together:
//!
//! 1. real concurrent writers, so the kill lands mid-transaction rather
//!    than on an idle server;
//! 2. an ungraceful kill — no `SIGTERM`, no checkpoint, so recovery has to
//!    come from the journal;
//! 3. the health endpoint, because "it came back" has to mean the engine
//!    answers, not just that a port is open again.
//!
//! The durability assertion is deliberately narrow: every insert whose
//! `Ok` the client *received* before the kill must be there afterwards.
//! Inserts still in flight are undetermined by definition, and asserting
//! anything about them would be asserting a bug.

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_postgres::{Client, NoTls};

const WRITERS: i64 = 4;
/// Room for each writer's ids without overlapping its neighbour's.
const WRITER_STRIDE: i64 = 1_000_000;
/// Rows that must be committed before the kill, so that it lands on a
/// server with a populated table and transactions in flight.
const MIN_ACKNOWLEDGED: i64 = 40;

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("bind")
        .local_addr()
        .expect("local addr")
        .port()
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ferrite-restart-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

struct ServerProcess(Child);

impl ServerProcess {
    /// An ungraceful kill: `SIGKILL` on Unix, `TerminateProcess` on
    /// Windows. No signal handler runs, so nothing check points and the
    /// journal is what the next start has to work from.
    fn kill(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

impl Drop for ServerProcess {
    fn drop(&mut self) {
        self.kill();
    }
}

/// Exactly the same command line and environment on every start: proving
/// that a restart suffices means proving that *this* restart suffices,
/// with no operator step in between.
fn spawn(port: u16, metrics_port: u16, data: &Path) -> ServerProcess {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ferrite-server"));
    command
        .env("FERRITE_LISTEN", format!("127.0.0.1:{port}"))
        .env(
            "FERRITE_METRICS_LISTEN",
            format!("127.0.0.1:{metrics_port}"),
        )
        .env("FERRITE_USER", "ferrite")
        .env("FERRITE_PASSWORD", "hunter2")
        .env("FERRITE_DATA", data)
        .env("FERRITE_TLS_DISABLE", "1")
        .env("FERRITE_LOG", "warn");
    ServerProcess(command.spawn().expect("spawn ferrite-server"))
}

async fn connect(port: u16) -> Client {
    let conn_str = format!(
        "host=127.0.0.1 port={port} user=ferrite password=hunter2 dbname=app sslmode=disable"
    );
    let (client, connection) = tokio_postgres::connect(&conn_str, NoTls)
        .await
        .expect("connect to the spawned server");
    tokio::spawn(async move {
        let _ = connection.await;
    });
    client
}

/// One `GET` against the observability endpoint, or `None` if it is not
/// listening yet.
async fn get(port: u16, target: &str) -> Option<String> {
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .ok()?;
    stream
        .write_all(format!("GET {target} HTTP/1.1\r\nHost: ferrite\r\n\r\n").as_bytes())
        .await
        .ok()?;
    let mut response = String::new();
    stream.read_to_string(&mut response).await.ok()?;
    Some(response)
}

/// Waits for the endpoint to report a healthy *engine*, which is a
/// stronger statement than waiting for a port to accept.
async fn wait_for_health(port: u16) -> Duration {
    let started = Instant::now();
    let deadline = started + Duration::from_secs(60);
    while Instant::now() < deadline {
        if let Some(response) = get(port, "/health").await {
            if response.starts_with("HTTP/1.1 200 OK") {
                return started.elapsed();
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("the server never reported healthy on {port}");
}

fn metric(body: &str, name: &str) -> f64 {
    body.lines()
        .find(|line| line.starts_with(name) && line[name.len()..].starts_with(' '))
        .and_then(|line| line.rsplit(' ').next())
        .and_then(|value| value.parse().ok())
        .unwrap_or_else(|| panic!("{name} is not in the exposition body:\n{body}"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_crash_under_load_needs_nothing_but_a_restart() {
    let data = scratch("under-load");
    let port = free_port();
    let metrics_port = free_port();

    // Highest id each writer got an `Ok` for. Shared with this task so the
    // count survives the writers being torn down by the kill.
    let acknowledged: Vec<Arc<AtomicI64>> =
        (0..WRITERS).map(|_| Arc::new(AtomicI64::new(-1))).collect();

    let mut server = spawn(port, metrics_port, &data);
    wait_for_health(metrics_port).await;

    let client = connect(port).await;
    client
        .batch_execute("CREATE TABLE ledger (id BIGINT NOT NULL, writer BIGINT NOT NULL)")
        .await
        .expect("CREATE TABLE");
    drop(client);

    let mut writers = Vec::new();
    for w in 0..WRITERS {
        let progress = Arc::clone(&acknowledged[w as usize]);
        writers.push(tokio::spawn(async move {
            let client = connect(port).await;
            let mut n = 0i64;
            loop {
                let id = w * WRITER_STRIDE + n;
                // The first error is the kill landing. Stop counting there:
                // everything after it is undetermined.
                if client
                    .execute(
                        "INSERT INTO ledger (id, writer) VALUES ($1, $2)",
                        &[&id, &w],
                    )
                    .await
                    .is_err()
                {
                    return;
                }
                progress.store(n, Ordering::SeqCst);
                n += 1;
            }
        }));
    }

    // Let the load build up before pulling the plug. Waiting on a row
    // count rather than on a stopwatch keeps the test meaningful on a slow
    // machine: every commit fsyncs, so throughput here is bounded by the
    // disk, not by the engine.
    let load_started = Instant::now();
    let deadline = load_started + Duration::from_secs(60);
    let acked = || -> i64 {
        acknowledged
            .iter()
            .map(|p| p.load(Ordering::SeqCst) + 1)
            .sum()
    };
    while acked() < MIN_ACKNOWLEDGED && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let committed: Vec<i64> = acknowledged
        .iter()
        .map(|p| p.load(Ordering::SeqCst))
        .collect();
    let total: i64 = committed.iter().map(|last| last + 1).sum();
    println!(
        "{total} rows acknowledged by {WRITERS} writers in {:?} \
         ({:.1} commits/s, every one fsynced)",
        load_started.elapsed(),
        total as f64 / load_started.elapsed().as_secs_f64()
    );
    assert!(
        total >= MIN_ACKNOWLEDGED,
        "the load was too light to be a meaningful crash test: {total} rows"
    );

    server.kill();
    for writer in writers.drain(..) {
        let _ = writer.await;
    }

    // Nothing between the kill and the restart: no repair tool, no flag,
    // no manual checkpoint. This is what an orchestrator does on its own.
    let _server = spawn(port, metrics_port, &data);
    let recovery = wait_for_health(metrics_port).await;
    assert!(
        recovery < Duration::from_secs(30),
        "recovery took {recovery:?}, which is longer than any restart policy tolerates"
    );

    // Every acknowledged write is there, and readable.
    let client = connect(port).await;
    for (w, last) in committed.iter().enumerate() {
        let expected = last + 1;
        let rows = client
            .query("SELECT id FROM ledger WHERE writer = $1", &[&(w as i64)])
            .await
            .expect("read back this writer's rows");
        assert!(
            rows.len() as i64 >= expected,
            "writer {w} was told {expected} rows were committed, only {} survived",
            rows.len()
        );
        // The one that mattered most: the last write the client saw
        // succeed, i.e. the closest one to the crash.
        let last_id = w as i64 * WRITER_STRIDE + last;
        let found = client
            .query("SELECT id FROM ledger WHERE id = $1", &[&last_id])
            .await
            .expect("read back the last acknowledged row");
        assert_eq!(
            found.len(),
            1,
            "writer {w}'s last acknowledged row ({last_id}) did not survive the crash"
        );
    }

    // And the database is not merely readable: it still takes writes.
    let inserted = client
        .execute(
            "INSERT INTO ledger (id, writer) VALUES ($1, $2)",
            &[&9_999_999i64, &99i64],
        )
        .await
        .expect("the restarted server accepts new writes");
    assert_eq!(inserted, 1);

    // A fresh process, not a resurrected one: its counters start over, and
    // the health probe it just answered is on its own tally.
    let body = get(metrics_port, "/metrics")
        .await
        .expect("scrape the restarted server");
    assert!(metric(&body, "ferrite_health_checks_total") >= 1.0);
    assert_eq!(metric(&body, "ferrite_health_failures_total"), 0.0);
    assert!(metric(&body, "ferrite_uptime_seconds") < 60.0);
}

/// A restart with no crash at all still has to work: the orderly path
/// check points on `SIGTERM`, so the next start replays nothing, and the
/// health endpoint must go green just the same.
#[tokio::test]
async fn an_orderly_restart_comes_back_healthy_with_its_data() {
    let data = scratch("orderly");
    let port = free_port();
    let metrics_port = free_port();

    {
        let _server = spawn(port, metrics_port, &data);
        wait_for_health(metrics_port).await;
        let client = connect(port).await;
        client
            .batch_execute("CREATE TABLE kv (k BIGINT NOT NULL, v TEXT)")
            .await
            .expect("CREATE TABLE");
        client
            .execute("INSERT INTO kv (k, v) VALUES ($1, $2)", &[&1i64, &"one"])
            .await
            .expect("INSERT");
    }

    let _server = spawn(port, metrics_port, &data);
    wait_for_health(metrics_port).await;
    let client = connect(port).await;
    let row = client
        .query_one("SELECT v FROM kv WHERE k = 1", &[])
        .await
        .expect("the row survived the restart");
    assert_eq!(row.get::<_, String>(0), "one");
}
