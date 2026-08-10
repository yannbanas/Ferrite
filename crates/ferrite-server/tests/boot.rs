//! Boots the real binary as a child process and talks to it with a real
//! PostgreSQL client.
//!
//! This is the only test that covers `main` itself — argument handling, TLS
//! defaults, listener wiring — and the only one that exercises all four
//! crate families at once: wire protocol, planner, executor, storage. Every
//! statement below crosses a socket and reaches real pages on disk.

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use tokio_postgres::{Client, NoTls};

/// Reserves a port by binding and immediately releasing it. A race is
/// possible in principle; in practice the child binds within milliseconds.
fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("bind")
        .local_addr()
        .expect("local addr")
        .port()
}

/// A data directory of this test's own, so tests never share a database.
fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ferrite-boot-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

struct ServerProcess(Child);

impl Drop for ServerProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn spawn(port: u16, data: &Path, extra: &[(&str, &str)]) -> ServerProcess {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ferrite-server"));
    command
        .env("FERRITE_LISTEN", format!("127.0.0.1:{port}"))
        .env("FERRITE_USER", "ferrite")
        .env("FERRITE_PASSWORD", "hunter2")
        .env("FERRITE_DATA", data)
        .env("FERRITE_LOG", "warn");
    for (key, value) in extra {
        command.env(key, value);
    }
    ServerProcess(command.spawn().expect("spawn ferrite-server"))
}

async fn wait_for_port(port: u16) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        if tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_ok()
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("ferrite-server never started listening on {port}");
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

#[tokio::test]
async fn the_binary_requires_tls_by_default() {
    let data = scratch("tls");
    let port = free_port();
    let _server = spawn(port, &data, &[]);
    wait_for_port(port).await;

    // No TLS flags at all: the server must still have generated a
    // certificate and must refuse a cleartext session.
    let conn_str = format!(
        "host=127.0.0.1 port={port} user=ferrite password=hunter2 dbname=app sslmode=disable"
    );
    assert!(
        tokio_postgres::connect(&conn_str, NoTls).await.is_err(),
        "a default listener must not accept a cleartext session"
    );
}

/// The full lifecycle, over the wire, against the real engine: DDL, three
/// kinds of write, a filtered read, and — after the process is killed and
/// restarted — the same data read back out of the files it left behind.
#[tokio::test]
async fn the_engine_serves_ddl_dml_and_survives_a_restart() {
    let data = scratch("lifecycle");
    let port = free_port();

    {
        let _server = spawn(port, &data, &[("FERRITE_TLS_DISABLE", "1")]);
        wait_for_port(port).await;
        let client = connect(port).await;

        client
            .batch_execute(
                "CREATE TABLE pets (
                     id BIGINT NOT NULL,
                     name TEXT NOT NULL,
                     adopted BOOLEAN,
                     weight_kg DOUBLE PRECISION
                 )",
            )
            .await
            .expect("CREATE TABLE");

        // An index the planner can actually choose an access path from.
        client
            .batch_execute("CREATE UNIQUE INDEX pets_pkey ON pets (id)")
            .await
            .expect("CREATE INDEX");

        let inserted = client
            .execute(
                "INSERT INTO pets VALUES \
                 (1, 'Rex', true, 12.5), \
                 (2, 'Moebius', false, 4.25), \
                 (3, 'Ada', true, 7.0)",
                &[],
            )
            .await
            .expect("INSERT");
        assert_eq!(inserted, 3);

        // Bound parameters, i.e. the extended query flow with a real
        // ParameterDescription behind it.
        let inserted = client
            .execute(
                "INSERT INTO pets (id, name, adopted, weight_kg) VALUES ($1, $2, $3, $4)",
                &[&4i64, &"Grace", &false, &3.5f64],
            )
            .await
            .expect("parameterised INSERT");
        assert_eq!(inserted, 1);

        let rows = client
            .query("SELECT id, name FROM pets WHERE adopted = true", &[])
            .await
            .expect("SELECT ... WHERE");
        let mut names: Vec<String> = rows.iter().map(|r| r.get::<_, String>("name")).collect();
        names.sort();
        assert_eq!(names, vec!["Ada".to_string(), "Rex".to_string()]);

        // Equality on the indexed column, which is the access path the
        // planner picks an IndexScan for.
        let row = client
            .query_one("SELECT name, weight_kg FROM pets WHERE id = 2", &[])
            .await
            .expect("indexed SELECT");
        assert_eq!(row.get::<_, String>(0), "Moebius");
        assert_eq!(row.get::<_, f64>(1), 4.25);

        let updated = client
            .execute("UPDATE pets SET adopted = true WHERE id = 2", &[])
            .await
            .expect("UPDATE");
        assert_eq!(updated, 1);

        let deleted = client
            .execute("DELETE FROM pets WHERE weight_kg < 4.0", &[])
            .await
            .expect("DELETE");
        assert_eq!(deleted, 1, "only Grace weighs under 4 kg");

        let rows = client
            .query("SELECT id FROM pets", &[])
            .await
            .expect("count");
        assert_eq!(rows.len(), 3);

        // An explicit transaction, rolled back: the row must not survive.
        client.batch_execute("BEGIN").await.expect("BEGIN");
        client
            .execute("INSERT INTO pets VALUES (9, 'Ghost', false, 1.0)", &[])
            .await
            .expect("INSERT in transaction");
        client.batch_execute("ROLLBACK").await.expect("ROLLBACK");
        let rows = client
            .query("SELECT id FROM pets WHERE id = 9", &[])
            .await
            .expect("post-rollback SELECT");
        assert!(rows.is_empty(), "a rolled-back INSERT must leave nothing");

        // The child is killed rather than shut down, so recovery has to come
        // out of the journal exactly as it would after a power cut.
    }

    let port = free_port();
    let _server = spawn(port, &data, &[("FERRITE_TLS_DISABLE", "1")]);
    wait_for_port(port).await;
    let client = connect(port).await;

    let rows = client
        .query("SELECT id, name, adopted FROM pets", &[])
        .await
        .expect("SELECT after restart");
    assert_eq!(rows.len(), 3, "the committed rows must survive a restart");

    let row = client
        .query_one("SELECT name, adopted FROM pets WHERE id = 2", &[])
        .await
        .expect("indexed SELECT after restart");
    assert_eq!(row.get::<_, String>(0), "Moebius");
    assert!(
        row.get::<_, bool>(1),
        "the committed UPDATE must survive a restart"
    );

    // The index metadata is catalog state, so it has to come back too.
    client
        .batch_execute("DROP INDEX pets_pkey")
        .await
        .expect("DROP INDEX after restart");
}

/// Two connections writing the same row: MVCC has to answer
/// `SerializationFailure` (SQLSTATE 40001), not corrupt anything and not
/// take the server down.
#[tokio::test]
async fn a_write_conflict_is_reported_as_serialization_failure() {
    let data = scratch("conflict");
    let port = free_port();
    let _server = spawn(port, &data, &[("FERRITE_TLS_DISABLE", "1")]);
    wait_for_port(port).await;

    let setup = connect(port).await;
    setup
        .batch_execute("CREATE TABLE counters (id BIGINT NOT NULL, n BIGINT NOT NULL)")
        .await
        .expect("CREATE TABLE");
    setup
        .execute("INSERT INTO counters VALUES (1, 0)", &[])
        .await
        .expect("INSERT");

    let a = connect(port).await;
    let b = connect(port).await;
    a.batch_execute("BEGIN").await.expect("BEGIN a");
    b.batch_execute("BEGIN").await.expect("BEGIN b");

    a.execute("UPDATE counters SET n = 1 WHERE id = 1", &[])
        .await
        .expect("the first writer wins");

    let conflict = b
        .execute("UPDATE counters SET n = 2 WHERE id = 1", &[])
        .await
        .expect_err("the second writer must be refused");
    assert_eq!(
        conflict.code().map(|c| c.code()),
        Some("40001"),
        "expected a serialization failure, got {conflict}"
    );

    b.batch_execute("ROLLBACK").await.expect("ROLLBACK b");
    a.batch_execute("COMMIT").await.expect("COMMIT a");

    let row = setup
        .query_one("SELECT n FROM counters WHERE id = 1", &[])
        .await
        .expect("read the survivor");
    assert_eq!(row.get::<_, i64>(0), 1);

    // The server is still serving everyone else.
    assert_eq!(
        setup
            .query("SELECT id FROM counters", &[])
            .await
            .expect("the listener is still healthy")
            .len(),
        1
    );
}

/// Two things at once, through the real wire: the query shapes an
/// application actually writes run end to end, and everything `ferrite-sql`
/// parses but `ferrite-exec` still cannot run comes back as a clean error on
/// a connection that stays usable.
#[tokio::test]
async fn unsupported_sql_is_an_error_not_a_dropped_connection() {
    let data = scratch("unsupported");
    let port = free_port();
    let _server = spawn(port, &data, &[("FERRITE_TLS_DISABLE", "1")]);
    wait_for_port(port).await;

    let client = connect(port).await;
    client
        .batch_execute("CREATE TABLE t (a BIGINT NOT NULL, b TEXT)")
        .await
        .expect("CREATE TABLE");
    client
        .execute("INSERT INTO t VALUES (1, 'x')", &[])
        .await
        .expect("INSERT");
    client
        .batch_execute("CREATE TABLE u (a BIGINT NOT NULL, tag TEXT)")
        .await
        .expect("CREATE TABLE u");
    client
        .execute("INSERT INTO u VALUES (1, 'tag')", &[])
        .await
        .expect("INSERT u");

    for sql in [
        "SELECT count(*) FROM t",
        "SELECT * FROM t ORDER BY a DESC",
        "SELECT a, count(*) FROM t GROUP BY a HAVING count(*) > 0",
        "SELECT t.a, u.tag FROM t JOIN u ON u.a = t.a",
        "SELECT t.a, u.tag FROM t LEFT JOIN u ON u.a = t.a",
        "SELECT DISTINCT b FROM t WHERE b LIKE 'x%'",
        "SELECT b FROM t LIMIT 1 OFFSET 0",
    ] {
        client
            .query(sql, &[])
            .await
            .unwrap_or_else(|err| panic!("{sql:?} should run now: {err}"));
    }

    for sql in [
        // `*` has no meaning once the rows have been collapsed into groups.
        "SELECT * FROM t GROUP BY a",
        "SELECT (SELECT a FROM t) FROM t",
        "SELECT CAST(a AS TEXT) FROM t",
        "ALTER TABLE t ADD COLUMN c INT",
        "SELECT * FROM nope",
        "this is not sql",
    ] {
        assert!(
            client.query(sql, &[]).await.is_err(),
            "{sql:?} should have been refused"
        );
    }

    // Same connection, still fine.
    let row = client
        .query_one("SELECT a FROM t WHERE a = 1", &[])
        .await
        .expect("the connection survived every refusal");
    assert_eq!(row.get::<_, i64>(0), 1);
}
