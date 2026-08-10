//! Abuse suite, run by hand against an already-running server — the
//! container built from the repository `Dockerfile`, in particular.
//!
//! ```bash
//! docker run -d --name ferrite -p 15432:5432 \
//!   -e FERRITE_TLS_DISABLE=1 -e FERRITE_PASSWORD=hunter2 \
//!   -v ferrite-data:/data ferrite-server
//! cargo test -p ferrite-server --test stress -- --ignored --nocapture
//! ```
//!
//! Ignored by default: these need a listener that the normal test run has
//! no way to provide, and they are about resilience under load rather than
//! about a specific behaviour, so they do not belong in the gate.
//!
//! Override the target with `FERRITE_STRESS_ADDR=host:port`.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

use tokio_postgres::{Client, NoTls};

fn addr() -> String {
    std::env::var("FERRITE_STRESS_ADDR").unwrap_or_else(|_| "127.0.0.1:15432".to_owned())
}

fn conn_str() -> String {
    let addr = addr();
    let (host, port) = addr.rsplit_once(':').expect("host:port");
    format!("host={host} port={port} user=ferrite password=hunter2 dbname=app sslmode=disable")
}

async fn connect() -> Client {
    let (client, connection) = tokio_postgres::connect(&conn_str(), NoTls)
        .await
        .expect("connect");
    tokio::spawn(async move {
        let _ = connection.await;
    });
    client
}

async fn reset(client: &Client, ddl: &str) {
    let _ = client.batch_execute("DROP TABLE IF EXISTS stress").await;
    client.batch_execute(ddl).await.expect("create the table");
}

/// Several clients hammering the same rows inside overlapping explicit
/// transactions. Every write either lands or is refused with SQLSTATE
/// 40001; nothing else is acceptable, and the listener has to be healthy
/// afterwards.
#[tokio::test]
#[ignore = "needs a running ferrite-server; see the module docs"]
async fn overlapping_transactions_conflict_instead_of_crashing() {
    const CLIENTS: usize = 8;
    const ROUNDS: usize = 25;

    let setup = connect().await;
    reset(
        &setup,
        "CREATE TABLE stress (id BIGINT NOT NULL, n BIGINT NOT NULL)",
    )
    .await;
    for id in 0..4i64 {
        setup
            .execute("INSERT INTO stress VALUES ($1, 0)", &[&id])
            .await
            .expect("seed");
    }

    let mut tasks = Vec::new();
    for worker in 0..CLIENTS {
        tasks.push(tokio::spawn(async move {
            let client = connect().await;
            let (mut committed, mut conflicts, mut other) = (0u32, 0u32, Vec::new());
            for round in 0..ROUNDS {
                let id = ((worker + round) % 4) as i64;
                client.batch_execute("BEGIN").await.expect("BEGIN");
                // A literal, not `n = n + 1`: v1 has no arithmetic in
                // expressions, and the point here is the conflict, not the
                // value.
                let write = client
                    .execute(
                        "UPDATE stress SET n = $1 WHERE id = $2",
                        &[&(round as i64), &id],
                    )
                    .await;
                match write {
                    Ok(_) => match client.batch_execute("COMMIT").await {
                        Ok(()) => committed += 1,
                        Err(err) => other.push(format!("COMMIT: {err}")),
                    },
                    Err(err) if err.code().map(|c| c.code()) == Some("40001") => {
                        conflicts += 1;
                        client.batch_execute("ROLLBACK").await.expect("ROLLBACK");
                    }
                    Err(err) => {
                        other.push(match err.as_db_error() {
                            Some(db) => format!("{}: {}", db.code().code(), db.message()),
                            None => err.to_string(),
                        });
                        let _ = client.batch_execute("ROLLBACK").await;
                    }
                }
            }
            (committed, conflicts, other)
        }));
    }

    let (mut committed, mut conflicts, mut unexpected) = (0u32, 0u32, Vec::new());
    for task in tasks {
        let (c, k, o) = task.await.expect("worker task");
        committed += c;
        conflicts += k;
        unexpected.extend(o);
    }

    println!("committed={committed} conflicts={conflicts} unexpected={unexpected:?}");
    assert!(
        unexpected.is_empty(),
        "every failure must be a 40001, got {unexpected:?}"
    );
    assert_eq!(committed + conflicts, (CLIENTS * ROUNDS) as u32);

    let rows = setup
        .query("SELECT id FROM stress", &[])
        .await
        .expect("the listener is still healthy");
    assert_eq!(rows.len(), 4, "no row was lost or duplicated");
}

/// Raw hostile bytes on the socket, not a well-behaved client. Each case
/// must leave the server up and still serving a separate, healthy
/// connection.
#[tokio::test]
#[ignore = "needs a running ferrite-server; see the module docs"]
async fn hostile_frames_do_not_take_the_listener_down() {
    let healthy = connect().await;
    reset(&healthy, "CREATE TABLE stress (id BIGINT NOT NULL)").await;

    let cases: Vec<(&str, Vec<u8>)> = vec![
        // A startup packet claiming to be 2 GiB long.
        ("startup length 0x7fffffff", vec![0x7f, 0xff, 0xff, 0xff]),
        // Length shorter than the four bytes of the length field itself.
        ("startup length 0", vec![0, 0, 0, 0]),
        ("startup length 1", vec![0, 0, 0, 1]),
        // Protocol version 2, explicitly refused.
        ("protocol v2", vec![0, 0, 0, 8, 0, 2, 0, 0]),
        // A tagged message before any startup packet.
        ("tagged message first", b"Q\0\0\0\x0aSELECT 1\0".to_vec()),
        // A valid startup, then an unknown message tag.
        ("unknown tag", {
            let mut v = startup();
            v.extend_from_slice(b"\xff\0\0\0\x04");
            v
        }),
        // A valid startup, then a Query header promising far more bytes
        // than follow, then the connection dies mid-message.
        ("truncated query", {
            let mut v = startup();
            v.extend_from_slice(b"Q\x00\x00\x10\x00SELECT");
            v
        }),
        // A Query whose declared length is negative.
        ("negative length", {
            let mut v = startup();
            v.extend_from_slice(b"Q\xff\xff\xff\xff");
            v
        }),
        // Non-UTF-8 inside an otherwise well-framed Query.
        ("invalid utf-8", {
            let mut v = startup();
            v.extend_from_slice(b"Q\x00\x00\x00\x09\xc3\x28\xff\0");
            v
        }),
        ("random bytes", (0u8..=255).cycle().take(4096).collect()),
    ];

    for (name, bytes) in cases {
        let mut socket = TcpStream::connect(addr()).expect("connect raw");
        socket
            .set_read_timeout(Some(Duration::from_millis(500)))
            .expect("timeout");
        let _ = socket.write_all(&bytes);
        let _ = socket.flush();
        // Read whatever comes back, then drop the socket mid-conversation.
        let mut sink = [0u8; 1024];
        let _ = socket.read(&mut sink);
        drop(socket);

        let rows = healthy
            .query("SELECT id FROM stress", &[])
            .await
            .unwrap_or_else(|err| panic!("{name}: the server stopped serving: {err}"));
        assert!(rows.is_empty());
        println!("survived: {name}");
    }

    // And a brand new connection still works.
    let fresh = connect().await;
    fresh
        .execute("INSERT INTO stress VALUES (1)", &[])
        .await
        .expect("a new connection after the abuse");
}

/// A `StartupMessage` for user `ferrite`, database `app`: the prefix every
/// case that wants to get past negotiation needs.
fn startup() -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&196_608i32.to_be_bytes()); // protocol 3.0
    for field in ["user", "ferrite", "database", "app"] {
        body.extend_from_slice(field.as_bytes());
        body.push(0);
    }
    body.push(0);
    let mut out = ((body.len() + 4) as i32).to_be_bytes().to_vec();
    out.extend_from_slice(&body);
    out
}

/// Tens of thousands of rows, to see whether anything falls over a cliff.
/// Not a benchmark — the property under test is that nothing is
/// pathological, e.g. a scan that goes quadratic or memory that never
/// comes back.
#[tokio::test]
#[ignore = "needs a running ferrite-server; see the module docs"]
async fn a_realistic_row_count_does_not_fall_over() {
    const BATCH: i64 = 250;
    // Overridable so the same test can be run at two sizes to see whether
    // anything scales worse than linearly.
    let rows: i64 = std::env::var("FERRITE_STRESS_ROWS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(30_000);

    let client = connect().await;
    reset(
        &client,
        "CREATE TABLE stress (id BIGINT NOT NULL, name TEXT NOT NULL, n BIGINT NOT NULL)",
    )
    .await;

    let started = Instant::now();
    for batch in 0..(rows / BATCH) {
        let mut sql = String::from("INSERT INTO stress VALUES ");
        for i in 0..BATCH {
            let id = batch * BATCH + i;
            if i > 0 {
                sql.push(',');
            }
            sql.push_str(&format!("({id}, 'row-{id}', {})", id % 97));
        }
        client.batch_execute(&sql).await.expect("bulk INSERT");
    }
    let insert = started.elapsed();

    let started = Instant::now();
    let scanned = client
        .query("SELECT id FROM stress", &[])
        .await
        .expect("full scan");
    let scan = started.elapsed();
    assert_eq!(scanned.len() as i64, rows);

    let started = Instant::now();
    let matched = client
        .query("SELECT id, name FROM stress WHERE n = 42", &[])
        .await
        .expect("filtered scan");
    let filtered = started.elapsed();
    assert!(!matched.is_empty());

    let started = Instant::now();
    let hit = client
        .query_one("SELECT name FROM stress WHERE id = $1", &[&(rows - 1)])
        .await
        .expect("point lookup");
    let point = started.elapsed();
    assert_eq!(hit.get::<_, String>(0), format!("row-{}", rows - 1));

    let started = Instant::now();
    let updated = client
        .execute("UPDATE stress SET n = 0 WHERE n = 96", &[])
        .await
        .expect("bulk UPDATE");
    let update = started.elapsed();

    let started = Instant::now();
    let deleted = client
        .execute("DELETE FROM stress WHERE n = 0", &[])
        .await
        .expect("bulk DELETE");
    let delete = started.elapsed();

    println!(
        "rows={rows} insert={insert:?} scan={scan:?} filtered={filtered:?} \
         point={point:?} update={update:?}({updated}) delete={delete:?}({deleted})"
    );
}

/// Restart the container while writes are in flight, then check that every
/// row the client was *told* was committed is still there.
///
/// This is the Docker-level counterpart to `ferrite-storage`'s local kill
/// test: a real container stop (SIGTERM, then SIGKILL after the grace
/// period), a real volume, a real restart. Set `FERRITE_STRESS_DOCKER` to
/// the container name to enable it.
#[tokio::test]
#[ignore = "needs a running ferrite-server in Docker; see the module docs"]
async fn a_container_restart_keeps_everything_committed() {
    let Ok(container) = std::env::var("FERRITE_STRESS_DOCKER") else {
        println!("FERRITE_STRESS_DOCKER is unset: skipping");
        return;
    };

    let client = connect().await;
    reset(
        &client,
        "CREATE TABLE stress (id BIGINT NOT NULL, name TEXT NOT NULL)",
    )
    .await;

    // Write until the restart lands, keeping only the ids the server
    // acknowledged.
    let writer = tokio::spawn(async move {
        let mut committed: Vec<i64> = Vec::new();
        for id in 0..100_000i64 {
            match client
                .execute("INSERT INTO stress VALUES ($1, $2)", &[&id, &"x"])
                .await
            {
                Ok(_) => committed.push(id),
                // The restart shows up as a dead connection, which is the
                // signal to stop.
                Err(_) => break,
            }
        }
        committed
    });

    tokio::time::sleep(Duration::from_millis(1500)).await;
    let restarted = std::process::Command::new("docker")
        .args(["restart", "-t", "2", &container])
        .status()
        .expect("docker restart");
    assert!(restarted.success(), "docker restart failed");

    let committed = writer.await.expect("writer task");
    assert!(
        !committed.is_empty(),
        "the writer never committed anything before the restart"
    );

    // Wait for the listener to come back.
    let deadline = Instant::now() + Duration::from_secs(30);
    let client = loop {
        match tokio_postgres::connect(&conn_str(), NoTls).await {
            Ok((client, connection)) => {
                tokio::spawn(async move {
                    let _ = connection.await;
                });
                break client;
            }
            Err(_) if Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            Err(err) => panic!("the container never came back: {err}"),
        }
    };

    let rows = client
        .query("SELECT id FROM stress", &[])
        .await
        .expect("read back after the restart");
    let mut found: Vec<i64> = rows.iter().map(|r| r.get::<_, i64>(0)).collect();
    found.sort_unstable();

    println!(
        "acknowledged={} recovered={} (extra rows written but unacknowledged: {})",
        committed.len(),
        found.len(),
        found.len().saturating_sub(committed.len())
    );
    for id in &committed {
        assert!(
            found.binary_search(id).is_ok(),
            "row {id} was acknowledged as committed but is gone after the restart"
        );
    }
}
