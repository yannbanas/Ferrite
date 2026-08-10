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
    // The observability endpoint binds a fixed port by default, and these
    // tests run several servers at once; a test that wants it names its own
    // port through `extra`.
    if !extra
        .iter()
        .any(|(key, _)| key.starts_with("FERRITE_METRICS"))
    {
        command.env("FERRITE_METRICS_DISABLE", "1");
    }
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

/// `ALTER TABLE … ADD COLUMN` on a table that already holds rows, and
/// `DEFAULT` actually applied to a column an `INSERT` leaves out.
///
/// This is the shape a real application migration has: the table is
/// created, filled, then grown one column at a time, and the rows written
/// before each migration have to keep reading back — the storage layer
/// never rewrites them, so the read path is what reconciles the arity.
/// The last section replays PawChat's own `users` case, which is where the
/// missing `DEFAULT` was found.
#[tokio::test]
async fn alter_table_adds_columns_and_defaults_are_applied() {
    let data = scratch("alter");
    let port = free_port();

    {
        let _server = spawn(port, &data, &[("FERRITE_TLS_DISABLE", "1")]);
        wait_for_port(port).await;
        let client = connect(port).await;

        client
            .batch_execute("CREATE TABLE members (id BIGINT NOT NULL, name TEXT NOT NULL)")
            .await
            .expect("CREATE TABLE");
        client
            .execute("INSERT INTO members VALUES (1, 'Rex'), (2, 'Ada')", &[])
            .await
            .expect("INSERT");

        // A nullable column: rows written before it read back as NULL.
        client
            .batch_execute("ALTER TABLE members ADD COLUMN bio TEXT")
            .await
            .expect("ADD COLUMN bio");
        let row = client
            .query_one("SELECT id, name, bio FROM members WHERE id = 1", &[])
            .await
            .expect("read a row that predates the column");
        assert_eq!(row.get::<_, Option<String>>("bio"), None);

        // A NOT NULL column with a constant default: rows written before it
        // read back as that default, not as NULL.
        client
            .batch_execute("ALTER TABLE members ADD COLUMN banned BIGINT NOT NULL DEFAULT 0")
            .await
            .expect("ADD COLUMN banned");
        let rows = client
            .query("SELECT id, banned FROM members", &[])
            .await
            .expect("read the backfilled column");
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|r| r.get::<_, i64>("banned") == 0));

        // …and the same default lands on a row inserted without it.
        client
            .execute("INSERT INTO members (id, name) VALUES (3, 'Grace')", &[])
            .await
            .expect("INSERT omitting the defaulted column");
        let row = client
            .query_one("SELECT banned, bio FROM members WHERE id = 3", &[])
            .await
            .expect("read the new row");
        assert_eq!(row.get::<_, i64>("banned"), 0);
        assert_eq!(
            row.get::<_, Option<String>>("bio"),
            None,
            "a column with no DEFAULT still falls back to NULL"
        );

        // An explicit value still wins over the default.
        client
            .execute(
                "INSERT INTO members (id, name, banned) VALUES (4, 'Moebius', 1)",
                &[],
            )
            .await
            .expect("INSERT supplying the defaulted column");
        let row = client
            .query_one("SELECT banned FROM members WHERE id = 4", &[])
            .await
            .expect("read the explicit value");
        assert_eq!(row.get::<_, i64>("banned"), 1);

        // A short row still updates, and comes back full width afterwards.
        let updated = client
            .execute("UPDATE members SET bio = 'hello' WHERE id = 2", &[])
            .await
            .expect("UPDATE a row that predates two columns");
        assert_eq!(updated, 1);
        let row = client
            .query_one("SELECT bio, banned FROM members WHERE id = 2", &[])
            .await
            .expect("read the updated row");
        assert_eq!(
            row.get::<_, Option<String>>("bio").as_deref(),
            Some("hello")
        );
        assert_eq!(row.get::<_, i64>("banned"), 0);

        // `IF NOT EXISTS` makes the migration re-runnable, which is how an
        // application replays its whole list of migrations at every boot.
        client
            .batch_execute("ALTER TABLE members ADD COLUMN IF NOT EXISTS bio TEXT")
            .await
            .expect("ADD COLUMN IF NOT EXISTS on an existing column is a no-op");
        client
            .batch_execute("ALTER TABLE IF EXISTS nope ADD COLUMN x TEXT")
            .await
            .expect("ALTER TABLE IF EXISTS on a missing table is a no-op");

        for sql in [
            // Already there, and not guarded.
            "ALTER TABLE members ADD COLUMN bio TEXT",
            // NOT NULL with nothing to backfill the existing rows with.
            "ALTER TABLE members ADD COLUMN nickname TEXT NOT NULL",
            // Volatile default: no honest value for a row that predates it.
            "ALTER TABLE members ADD COLUMN seen_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP",
            // Missing table, unguarded.
            "ALTER TABLE nope ADD COLUMN x TEXT",
            // The catalog is not the application's to alter.
            "ALTER TABLE ferrite_tables ADD COLUMN x TEXT",
            // A default outside the stored subset is refused, not dropped.
            "ALTER TABLE members ADD COLUMN score BIGINT DEFAULT (1 + 1)",
            // …and one whose type cannot hold.
            "ALTER TABLE members ADD COLUMN flag BOOLEAN DEFAULT 'nope'",
        ] {
            assert!(
                client.batch_execute(sql).await.is_err(),
                "{sql:?} should have been refused"
            );
        }

        // The schema survives every refusal above.
        assert_eq!(
            client
                .query("SELECT id, name, bio, banned FROM members", &[])
                .await
                .expect("the table is intact")
                .len(),
            4
        );

        // PawChat's own case: a `users` table whose flags all carry a
        // `DEFAULT`, written by an `INSERT` that names four columns.
        client
            .batch_execute(
                "CREATE TABLE users (
                     id TEXT NOT NULL PRIMARY KEY,
                     username TEXT NOT NULL,
                     password TEXT NOT NULL,
                     created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
                     totp_enabled BIGINT NOT NULL DEFAULT 0,
                     age_verified BIGINT NOT NULL DEFAULT 0,
                     profile_visibility TEXT NOT NULL DEFAULT 'everyone',
                     profile_theme TEXT DEFAULT 'galaxy'
                 )",
            )
            .await
            .expect("CREATE TABLE users");
        client
            .execute(
                "INSERT INTO users (id, username, password, created_at) \
                 VALUES ('u1', 'yann', 'hash', '2026-08-10T12:00:00Z')",
                &[],
            )
            .await
            .expect("the INSERT that used to fail on `totp_enabled is not nullable`");
        let row = client
            .query_one(
                "SELECT totp_enabled, profile_visibility, profile_theme FROM users \
                 WHERE id = 'u1'",
                &[],
            )
            .await
            .expect("read the defaulted row");
        assert_eq!(row.get::<_, i64>("totp_enabled"), 0);
        assert_eq!(row.get::<_, String>("profile_visibility"), "everyone");
        assert_eq!(
            row.get::<_, Option<String>>("profile_theme").as_deref(),
            Some("galaxy")
        );

        // `CURRENT_TIMESTAMP` is evaluated per statement, not stored, so a
        // row that omits the column gets a real time rather than NULL.
        client
            .execute(
                "INSERT INTO users (id, username, password) VALUES ('u2', 'ada', 'hash')",
                &[],
            )
            .await
            .expect("INSERT omitting a CURRENT_TIMESTAMP default");
        let row = client
            .query_one("SELECT created_at FROM users WHERE id = 'u2'", &[])
            .await
            .expect("read the timestamp default");
        assert!(
            row.get::<_, Option<std::time::SystemTime>>("created_at")
                .is_some_and(|t| t > std::time::UNIX_EPOCH),
            "CURRENT_TIMESTAMP must resolve to a real time"
        );
    }

    // Column metadata and defaults are catalog rows, so they have to come
    // back out of the files after a hard kill.
    let port = free_port();
    let _server = spawn(port, &data, &[("FERRITE_TLS_DISABLE", "1")]);
    wait_for_port(port).await;
    let client = connect(port).await;

    let rows = client
        .query("SELECT id, name, bio, banned FROM members", &[])
        .await
        .expect("SELECT after restart");
    assert_eq!(rows.len(), 4, "the added columns survive a restart");

    client
        .execute("INSERT INTO members (id, name) VALUES (5, 'Turing')", &[])
        .await
        .expect("the stored DEFAULT still applies after a restart");
    let row = client
        .query_one("SELECT banned FROM members WHERE id = 5", &[])
        .await
        .expect("read it back");
    assert_eq!(row.get::<_, i64>("banned"), 0);
}

/// The two features meeting: a table that has just grown a column, read
/// back through a `JOIN`, a `GROUP BY`, an `ORDER BY` and a `LIKE`.
///
/// Nothing rewrites the rows written before an `ADD COLUMN`, so they reach
/// the join, the sort and the aggregate one value short of the schema
/// unless the scan reconciles the arity first. Half the rows here predate
/// each added column and half do not, on both sides of the join, so a
/// missing reconciliation shows up as a wrong value rather than a crash —
/// which is the only reason to check the values and not just the shapes.
#[tokio::test]
async fn a_table_that_just_grew_a_column_still_joins_groups_and_sorts() {
    let data = scratch("alter-then-select");
    let port = free_port();
    let _server = spawn(port, &data, &[("FERRITE_TLS_DISABLE", "1")]);
    wait_for_port(port).await;
    let client = connect(port).await;

    client
        .batch_execute("CREATE TABLE authors (id BIGINT NOT NULL, name TEXT NOT NULL)")
        .await
        .expect("CREATE TABLE authors");
    client
        .batch_execute("CREATE TABLE posts (id BIGINT NOT NULL, author BIGINT NOT NULL)")
        .await
        .expect("CREATE TABLE posts");
    client
        .execute("INSERT INTO authors VALUES (1, 'Ada'), (2, 'Alan')", &[])
        .await
        .expect("INSERT authors");
    client
        .execute("INSERT INTO posts VALUES (10, 1), (11, 1), (12, 2)", &[])
        .await
        .expect("INSERT posts");

    client
        .batch_execute("ALTER TABLE authors ADD COLUMN rank BIGINT NOT NULL DEFAULT 7")
        .await
        .expect("ADD COLUMN rank");
    client
        .batch_execute("ALTER TABLE posts ADD COLUMN title TEXT")
        .await
        .expect("ADD COLUMN title");

    // Rows written after the migration, so the two tables now mix short and
    // full-width rows.
    client
        .execute("INSERT INTO authors VALUES (3, 'Grace', 1)", &[])
        .await
        .expect("INSERT a full-width author");
    client
        .execute("INSERT INTO posts VALUES (13, 3, 'about a bug')", &[])
        .await
        .expect("INSERT a full-width post");

    let rows = client
        .query(
            "SELECT authors.name, authors.rank, posts.id FROM posts \
             JOIN authors ON authors.id = posts.author \
             ORDER BY authors.rank DESC, posts.id ASC",
            &[],
        )
        .await
        .expect("JOIN + ORDER BY across a column added after the rows");
    let joined: Vec<(String, i64, i64)> = rows
        .iter()
        .map(|r| (r.get("name"), r.get("rank"), r.get("id")))
        .collect();
    assert_eq!(
        joined,
        vec![
            ("Ada".to_string(), 7, 10),
            ("Ada".to_string(), 7, 11),
            ("Alan".to_string(), 7, 12),
            ("Grace".to_string(), 1, 13),
        ],
        "the backfilled DEFAULT is what the join reads and what the sort orders on"
    );

    let rows = client
        .query(
            "SELECT authors.rank, count(*) FROM authors \
             LEFT JOIN posts ON posts.author = authors.id \
             GROUP BY authors.rank ORDER BY authors.rank",
            &[],
        )
        .await
        .expect("GROUP BY on a column added after the rows");
    let grouped: Vec<(i64, i64)> = rows.iter().map(|r| (r.get(0), r.get(1))).collect();
    assert_eq!(grouped, vec![(1, 1), (7, 3)]);

    // A column added with no DEFAULT reads back NULL on the old rows, so a
    // `LIKE` must not match them and `count()` must not count them.
    let row = client
        .query_one(
            "SELECT count(*), count(title) FROM posts WHERE title LIKE '%bug%'",
            &[],
        )
        .await
        .expect("LIKE over a column added after the rows");
    assert_eq!(row.get::<_, i64>(0), 1);
    assert_eq!(row.get::<_, i64>(1), 1);

    let rows = client
        .query("SELECT DISTINCT rank FROM authors ORDER BY rank DESC", &[])
        .await
        .expect("DISTINCT on a column added after the rows");
    assert_eq!(
        rows.iter().map(|r| r.get::<_, i64>(0)).collect::<Vec<_>>(),
        vec![7, 1]
    );

    // And an `INSERT` that omits the added column still takes its DEFAULT
    // once every read path above has run against the same table.
    client
        .execute("INSERT INTO authors (id, name) VALUES (4, 'Edsger')", &[])
        .await
        .expect("INSERT omitting the added column");
    let row = client
        .query_one(
            "SELECT rank FROM authors WHERE name LIKE 'Eds%' ORDER BY id",
            &[],
        )
        .await
        .expect("read the defaulted row back through LIKE + ORDER BY");
    assert_eq!(row.get::<_, i64>("rank"), 7);
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
        "SELECT * FROM t JOIN u ON u.a = t.a",
        "SELECT * FROM t WHERE b LIKE 'x%'",
        "SELECT * FROM t WHERE b ILIKE 'X%'",
        "SELECT * FROM t WHERE b = 'X' COLLATE NOCASE",
        "SELECT CAST(a AS TEXT) FROM t",
        "SELECT CASE WHEN a > 1 THEN 'big' ELSE 'small' END FROM t",
        "SELECT coalesce(b, 'none') FROM t",
        "SELECT datetime('now') FROM t",
    ] {
        client
            .query(sql, &[])
            .await
            .unwrap_or_else(|err| panic!("{sql:?} should run now: {err}"));
    }

    for sql in [
        // `*` has no meaning once the rows have been collapsed into groups.
        "SELECT * FROM t GROUP BY a",
        // A scope reaches a relation through its name *and* its alias, so a
        // self-join leaves `t.a` naming two columns.
        "SELECT * FROM t JOIN t t2 ON t2.a = t.a",
        "SELECT (SELECT a FROM t) FROM t",
        "SELECT strftime('%Y', b) FROM t",
        "ALTER TABLE t DROP COLUMN b",
        "ALTER TABLE t RENAME TO u",
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

/// Fetches one metric's value out of a Prometheus exposition body.
fn metric(body: &str, name: &str) -> f64 {
    body.lines()
        .find(|line| line.starts_with(name) && line[name.len()..].starts_with(' '))
        .and_then(|line| line.rsplit(' ').next())
        .and_then(|value| value.parse().ok())
        .unwrap_or_else(|| panic!("{name} is not in the exposition body:\n{body}"))
}

async fn scrape(port: u16) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("connect to the metrics endpoint");
    stream
        .write_all(b"GET /metrics HTTP/1.1\r\nHost: ferrite\r\n\r\n")
        .await
        .expect("send the scrape");
    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .await
        .expect("read the exposition");
    assert!(
        response.starts_with("HTTP/1.1 200 OK"),
        "unexpected scrape response: {response}"
    );
    response
}

/// The endpoint answers on its own port, and its counters move because of
/// traffic that crossed the *SQL* port — which is the only thing that makes
/// them worth scraping.
#[tokio::test]
async fn the_metrics_endpoint_reports_real_traffic() {
    let data = scratch("metrics");
    let port = free_port();
    let metrics_port = free_port();
    let _server = spawn(
        port,
        &data,
        &[
            ("FERRITE_TLS_DISABLE", "1"),
            (
                "FERRITE_METRICS_LISTEN",
                &format!("127.0.0.1:{metrics_port}"),
            ),
        ],
    );
    wait_for_port(port).await;
    wait_for_port(metrics_port).await;

    let before = scrape(metrics_port).await;
    assert!(before.contains("# TYPE ferrite_queries_total counter"));
    // Sampled, not counted: the data file exists from the first boot.
    assert!(metric(&before, "ferrite_data_file_bytes") > 0.0);

    let client = connect(port).await;
    client
        .batch_execute("CREATE TABLE m (id BIGINT NOT NULL, label TEXT)")
        .await
        .expect("create");
    for i in 0..5i64 {
        client
            .execute("INSERT INTO m (id, label) VALUES ($1, $2)", &[&i, &"x"])
            .await
            .expect("insert");
    }
    client.query("SELECT id FROM m", &[]).await.expect("select");

    let after = scrape(metrics_port).await;
    assert_eq!(
        metric(&after, "ferrite_queries_total{kind=\"insert\"}"),
        metric(&before, "ferrite_queries_total{kind=\"insert\"}") + 5.0
    );
    assert!(
        metric(&after, "ferrite_queries_total{kind=\"select\"}")
            > metric(&before, "ferrite_queries_total{kind=\"select\"}")
    );
    assert!(metric(&after, "ferrite_queries_total{kind=\"ddl\"}") >= 1.0);
    assert!(
        metric(&after, "ferrite_transactions_committed_total")
            >= metric(&before, "ferrite_transactions_committed_total") + 6.0
    );
    assert!(metric(&after, "ferrite_connections_total") >= 1.0);
    assert!(metric(&after, "ferrite_query_duration_seconds_count") >= 7.0);
    assert!(metric(&after, "ferrite_txn_id_ceiling") > 1.0e8);

    // A statement that fails is counted under its own category, not lost.
    let _ = client.query("SELECT * FROM absent", &[]).await;
    let errors = scrape(metrics_port).await;
    assert!(
        metric(
            &errors,
            "ferrite_query_errors_total{category=\"table_not_found\"}"
        ) >= 1.0
    );
}
