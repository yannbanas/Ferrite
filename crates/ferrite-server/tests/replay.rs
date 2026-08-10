//! Replays a translated third-party schema against a running server, table
//! by table, and prints what got in and what did not.
//!
//! Written for the PawChat migration (a real SQLite application schema, 72
//! tables), but it is not specific to it: point it at any directory holding
//! one `<table>.sql` per table, statements separated by a line reading
//! exactly `-- @@STATEMENT@@`, first statement being the `CREATE TABLE`.
//!
//! ```bash
//! FERRITE_REPLAY_DIR=/path/to/out \
//!   cargo test -p ferrite-server --test replay -- --ignored --nocapture
//! ```
//!
//! The point is the report, not a pass/fail: what a real application schema
//! costs Ferrite today is the useful signal.

use std::collections::BTreeMap;
use std::path::PathBuf;

use tokio_postgres::{Client, NoTls};

const SENTINEL: &str = "-- @@STATEMENT@@";

fn conn_str() -> String {
    let addr =
        std::env::var("FERRITE_STRESS_ADDR").unwrap_or_else(|_| "127.0.0.1:15432".to_owned());
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

fn describe(err: &tokio_postgres::Error) -> String {
    match err.as_db_error() {
        Some(db) => format!("[{}] {}", db.code().code(), db.message()),
        None => err.to_string(),
    }
}

struct Outcome {
    rows: usize,
    inserted: usize,
    ddl_error: Option<String>,
    /// One entry per distinct error message, with how many rows hit it.
    row_errors: BTreeMap<String, usize>,
}

#[tokio::test]
#[ignore = "needs a running ferrite-server and FERRITE_REPLAY_DIR"]
async fn replay_a_translated_schema() {
    let Ok(dir) = std::env::var("FERRITE_REPLAY_DIR") else {
        println!("FERRITE_REPLAY_DIR is unset: skipping");
        return;
    };
    let dir = PathBuf::from(dir);

    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .expect("read the replay directory")
        .filter_map(|entry| {
            let path = entry.ok()?.path();
            (path.extension()? == "sql").then_some(path)
        })
        .collect();
    files.sort();
    // `_after.sql` is not a table: it holds whatever should run once every
    // table exists — index DDL, representative application queries.
    let after = files
        .iter()
        .position(|f| f.file_stem().is_some_and(|s| s == "_after"))
        .map(|i| files.remove(i));

    let client = connect().await;
    let mut report: BTreeMap<String, Outcome> = BTreeMap::new();

    for file in &files {
        let table = file.file_stem().unwrap().to_string_lossy().to_string();
        let body = std::fs::read_to_string(file).expect("read a table file");
        // Split on the sentinel alone rather than on a line containing it:
        // the generator runs on Windows, so the file has CRLF endings.
        let mut statements = body
            .split(SENTINEL)
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let ddl = statements.next().unwrap_or_default();

        let mut outcome = Outcome {
            rows: 0,
            inserted: 0,
            ddl_error: None,
            row_errors: BTreeMap::new(),
        };

        let _ = client
            .batch_execute(&format!("DROP TABLE IF EXISTS \"{table}\""))
            .await;
        if let Err(err) = client.batch_execute(ddl).await {
            outcome.ddl_error = Some(describe(&err));
            // Without the table there is no point trying the rows, but they
            // still have to be counted.
            outcome.rows = statements.count();
            report.insert(table, outcome);
            continue;
        }

        for statement in statements {
            outcome.rows += 1;
            match client.batch_execute(statement).await {
                Ok(()) => outcome.inserted += 1,
                Err(err) => *outcome.row_errors.entry(describe(&err)).or_default() += 1,
            }
        }
        report.insert(table, outcome);
    }

    let mut ddl_ok = 0;
    let mut rows_total = 0;
    let mut rows_ok = 0;
    println!("\n{:<32} {:>7} {:>7}  outcome", "table", "rows", "loaded");
    println!("{}", "-".repeat(96));
    for (table, o) in &report {
        rows_total += o.rows;
        rows_ok += o.inserted;
        let note = match &o.ddl_error {
            Some(err) => format!("DDL REFUSED: {err}"),
            None => {
                ddl_ok += 1;
                if o.row_errors.is_empty() {
                    "ok".to_string()
                } else {
                    o.row_errors
                        .iter()
                        .map(|(err, n)| format!("{n}x {err}"))
                        .collect::<Vec<_>>()
                        .join(" | ")
                }
            }
        };
        println!("{table:<32} {:>7} {:>7}  {note}", o.rows, o.inserted);
    }
    println!("{}", "-".repeat(96));
    println!(
        "tables: {ddl_ok}/{} created   rows: {rows_ok}/{rows_total} loaded",
        report.len()
    );

    // Read every created table back, so "loaded" means "readable", not just
    // "accepted".
    let mut readable = 0;
    for (table, o) in &report {
        if o.ddl_error.is_some() {
            continue;
        }
        match client
            .query(&format!("SELECT * FROM \"{table}\""), &[])
            .await
        {
            Ok(rows) if rows.len() == o.inserted => readable += 1,
            Ok(rows) => println!("MISMATCH {table}: read {} of {}", rows.len(), o.inserted),
            Err(err) => println!("UNREADABLE {table}: {}", describe(&err)),
        }
    }
    println!("tables read back with the expected row count: {readable}/{ddl_ok}");

    let Some(after) = after else { return };
    let body = std::fs::read_to_string(&after).expect("read _after.sql");
    println!("\n--- statements replayed once every table exists ---");
    let (mut ok, mut total) = (0, 0);
    for statement in body
        .split(SENTINEL)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        total += 1;
        let one_line = statement.split_whitespace().collect::<Vec<_>>().join(" ");
        match client.batch_execute(statement).await {
            Ok(()) => {
                ok += 1;
                println!("  ok      {one_line}");
            }
            Err(err) => println!("  REFUSED {one_line}\n            {}", describe(&err)),
        }
    }
    println!("{ok}/{total} accepted");
}
