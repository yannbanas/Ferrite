//! Statements that have no answer must produce an error, never a panic.
//!
//! `cargo-fuzz` covers the lexer and the parser (`crates/ferrite-sql/fuzz`).
//! What it does not cover is everything *after* a statement parses: the
//! planner's slot bookkeeping, the executor's type handling, the arithmetic
//! it performs on values a query chose. This file walks the whole pipeline
//! — text to result — with input picked to hit those, so that a panic there
//! is a failing test rather than a connection the server drops in
//! production.

mod support;

use ferrite_common::{
    Catalog, DataType, Identity, Permission, Role, Row, Schema, StorageEngine, Value,
};
use ferrite_exec::Session;
use ferrite_planner::Planner;
use ferrite_proc::ProcRegistry;

use support::{column, MemCatalog, MemIndexes, MemStorage};

const CALLER: Identity = Identity([1u8; 32]);

fn everything() -> ProcRegistry {
    let mut registry = ProcRegistry::new();
    registry.grant_role(
        CALLER,
        Role {
            name: "owner".into(),
            permissions: vec![
                Permission::Select,
                Permission::Insert,
                Permission::Update,
                Permission::Delete,
                Permission::Execute,
            ],
        },
    );
    registry
}

/// Runs a statement all the way through, reporting only whether it got
/// there. A refusal is a pass; the assertion is that the process is still
/// running to make it.
fn survives(sql: &str) -> bool {
    let storage = MemStorage::new();
    let catalog = MemCatalog::new();
    let table = catalog
        .create_table(
            "public",
            "t",
            Schema {
                columns: vec![
                    column("id", DataType::Int8, false),
                    column("name", DataType::Text, true),
                    column("n", DataType::Int4, true),
                ],
            },
        )
        .unwrap();
    storage.create_table(0, table).unwrap();
    storage
        .insert(
            0,
            table,
            Row::new(vec![
                Value::Int8(1),
                Value::Text("ada".into()),
                Value::Int4(i32::MAX),
            ]),
        )
        .unwrap();
    let procs = everything();

    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let statements = match ferrite_sql::parse(sql) {
            Ok(statements) => statements,
            Err(_) => return,
        };
        for statement in &statements {
            let Ok(plan) = Planner::new(&catalog, &catalog).plan(statement) else {
                continue;
            };
            let _ = Session::new(&storage, &catalog, &procs, CALLER).execute(1, &plan);
        }
    }));
    outcome.is_ok()
}

#[test]
fn arithmetic_at_the_edges_of_a_type_never_panics() {
    for sql in [
        "SELECT n + 2147483647 FROM t",
        "SELECT n * n FROM t",
        "SELECT -9223372036854775808 - 1 FROM t",
        "SELECT id / 0 FROM t",
        "SELECT n % 0 FROM t",
        "SELECT id + 9223372036854775807 FROM t",
        "SELECT CAST(name AS BIGINT) FROM t",
        "SELECT CAST('9999999999999999999999' AS BIGINT) FROM t",
        "SELECT CAST(1e308 * 10 AS BIGINT) FROM t",
        "SELECT substr(name, -1, -1) FROM t",
        "SELECT substr(name, 9223372036854775807, 9223372036854775807) FROM t",
    ] {
        assert!(survives(sql), "panicked on {sql:?}");
    }
}

#[test]
fn misshapen_but_parseable_statements_never_panic() {
    for sql in [
        "SELECT count(count(*)) FROM t",
        "SELECT count(*) FROM t GROUP BY count(*)",
        "SELECT * FROM t ORDER BY count(*)",
        "SELECT * FROM t HAVING count(*) > 1",
        "SELECT id FROM t WHERE id IN ()",
        "SELECT id FROM t WHERE id IN (SELECT id, name FROM t)",
        "INSERT INTO t VALUES ()",
        "INSERT INTO t (id) VALUES (1, 2, 3)",
        "INSERT INTO t (nope) VALUES (1)",
        "UPDATE t SET nope = 1",
        "UPDATE t SET id = 'not a number'",
        "DELETE FROM t WHERE nope = 1",
        "SELECT * FROM t LIMIT 18446744073709551615 OFFSET 18446744073709551615",
        "SELECT * FROM t ORDER BY 999",
        "SELECT * FROM t ORDER BY id, id, id, id",
        "SELECT * FROM t t2 JOIN t t3 ON t2.id = t3.id",
        "SELECT * FROM t WHERE name = 1",
        "SELECT * FROM t WHERE id LIKE 'x'",
        "SELECT * FROM t WHERE name LIKE '%%%%%%%%%%'",
        "SELECT * FROM t WHERE name COLLATE nope = 'x'",
        "SELECT nope(id) FROM t",
        "SELECT datetime('now', 'not a modifier') FROM t",
        "SELECT date('not a date') FROM t",
        "CALL nope()",
    ] {
        assert!(survives(sql), "panicked on {sql:?}");
    }
}

/// Deep nesting is the classic way to turn a recursive-descent parser or a
/// recursive plan walk into a stack overflow — a process death, which no
/// `catch_unwind` and no per-task isolation can contain.
///
/// A flat `a OR b OR c …` chain is the case that matters and the one that
/// is easy to miss: the parser never recurses for it, but the tree it
/// builds is one level taller per term, and everything downstream walks
/// that tree recursively. 192 terms and a few kilobytes of SQL used to be
/// enough to take the process down.
#[test]
fn deeply_nested_input_does_not_take_the_process_down() {
    for depth in [64, 256, 4096] {
        let parens = format!("SELECT {}1{} FROM t", "(".repeat(depth), ")".repeat(depth));
        assert!(survives(&parens), "gave up on {depth} nested parentheses");

        let ors = std::iter::repeat_n("id = 1", depth)
            .collect::<Vec<_>>()
            .join(" OR ");
        assert!(
            survives(&format!("SELECT * FROM t WHERE {ors}")),
            "gave up on {depth} chained ORs"
        );

        let cases = format!(
            "SELECT {}END FROM t",
            "CASE WHEN id = 1 THEN ".repeat(depth) + "1 " + &"ELSE 2 END ".repeat(depth - 1)
        );
        assert!(survives(&cases), "gave up on {depth} nested CASEs");
    }
}

/// Values a query can construct but a column cannot hold, and the reverse.
/// A long `IN` list is the same shape as a long `OR` chain — the lowerer
/// turns one into the other — but unlike a hand-written chain it is
/// something applications really send, so it has to keep working rather
/// than be refused.
#[test]
fn a_very_long_in_list_is_answered_rather_than_refused() {
    let list = (0..5000)
        .map(|n| n.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!("SELECT id FROM t WHERE id IN ({list})");
    assert!(survives(&sql), "gave up on a 5000-element IN list");

    let storage = MemStorage::new();
    let catalog = MemCatalog::new();
    let table = catalog
        .create_table(
            "public",
            "t",
            Schema {
                columns: vec![column("id", DataType::Int8, false)],
            },
        )
        .unwrap();
    storage.create_table(0, table).unwrap();
    storage
        .insert(0, table, Row::new(vec![Value::Int8(4999)]))
        .unwrap();
    let procs = everything();
    let statement = ferrite_sql::parse_statement(&sql).unwrap();
    let plan = Planner::new(&catalog, &catalog).plan(&statement).unwrap();
    let result = Session::new(&storage, &catalog, &procs, CALLER)
        .execute(1, &plan)
        .expect("a long IN list still answers");
    assert!(matches!(result, ferrite_exec::QueryResult::Rows { ref rows, .. } if rows.len() == 1));
}

#[test]
fn values_outside_a_column_never_panic_on_the_way_in() {
    for sql in [
        "INSERT INTO t VALUES (9223372036854775807, 'x', 2147483647)",
        "INSERT INTO t VALUES (1, 'x', 2147483648)",
        "INSERT INTO t VALUES (1, NULL, NULL)",
        "INSERT INTO t VALUES (NULL, 'x', 1)",
        "UPDATE t SET n = n + 1",
        "UPDATE t SET n = n * 2",
    ] {
        assert!(survives(sql), "panicked on {sql:?}");
    }
}

/// The index-backed access path, which the tests above never reach
/// because they wire no provider.
#[test]
fn an_index_probe_on_a_hostile_key_never_panics() {
    let storage = MemStorage::new();
    let catalog = MemCatalog::new();
    let table = catalog
        .create_table(
            "public",
            "t",
            Schema {
                columns: vec![column("id", DataType::Int8, false)],
            },
        )
        .unwrap();
    storage.create_table(0, table).unwrap();
    catalog.add_index("t_pkey", table, 0, true);
    let procs = everything();

    let indexes = MemIndexes::new(&storage, &catalog);
    let session = Session::new(&storage, &catalog, &procs, CALLER).with_indexes(&indexes);
    for sql in [
        "SELECT * FROM t WHERE id = 1",
        "SELECT * FROM t WHERE id = 9223372036854775807",
        "SELECT * FROM t WHERE id = 'not a number'",
        "SELECT * FROM t WHERE id = NULL",
    ] {
        let Ok(statement) = ferrite_sql::parse_statement(sql) else {
            continue;
        };
        let Ok(plan) = Planner::new(&catalog, &catalog).plan(&statement) else {
            continue;
        };
        let _ = session.execute(1, &plan);
    }
}

/// A materializing executor turns one `SELECT *` on a large table into
/// resident memory proportional to the table, and an accidental cross join
/// into the product of two of them. Refusing is the point: the alternative
/// is the process growing until the operating system kills it, which takes
/// every other connection with it.
#[test]
fn a_result_set_past_the_budget_is_refused_rather_than_materialized() {
    let storage = MemStorage::new();
    let catalog = MemCatalog::new();
    let table = catalog
        .create_table(
            "public",
            "t",
            Schema {
                columns: vec![column("id", DataType::Int8, false)],
            },
        )
        .unwrap();
    storage.create_table(0, table).unwrap();
    // A second table rather than a self-join: a scope reaches a relation
    // by name as well as by alias, so `FROM t JOIN t t2` is refused as
    // ambiguous (a known planner limit, documented in docs/architecture.md).
    let other = catalog
        .create_table(
            "public",
            "u",
            Schema {
                columns: vec![column("id", DataType::Int8, false)],
            },
        )
        .unwrap();
    storage.create_table(0, other).unwrap();
    for id in 0..200i64 {
        storage
            .insert(0, table, Row::new(vec![Value::Int8(id)]))
            .unwrap();
        storage
            .insert(0, other, Row::new(vec![Value::Int8(id)]))
            .unwrap();
    }
    let procs = everything();
    let run = |sql: &str, max_rows: usize| {
        let statement = ferrite_sql::parse_statement(sql).unwrap();
        let plan = Planner::new(&catalog, &catalog).plan(&statement).unwrap();
        Session::new(&storage, &catalog, &procs, CALLER)
            .with_limits(ferrite_exec::Limits {
                max_rows,
                statement_timeout: None,
            })
            .execute(1, &plan)
    };

    assert!(
        matches!(
            run("SELECT id FROM t", 50),
            Err(ferrite_common::FerriteError::ResourceLimit(_))
        ),
        "200 rows must not fit a 50-row budget"
    );
    assert!(run("SELECT id FROM t", 500).is_ok());

    // The cross join is the case a budget on the *inputs* alone misses:
    // both sides fit, their product does not.
    assert!(matches!(
        run("SELECT t.id FROM t JOIN u ON 1 = 1", 500),
        Err(ferrite_common::FerriteError::ResourceLimit(_))
    ));
}

/// A statement that runs long holds a blocking thread and an MVCC
/// snapshot, so it needs a deadline rather than only a size bound.
#[test]
fn a_statement_past_its_deadline_is_abandoned() {
    let storage = MemStorage::new();
    let catalog = MemCatalog::new();
    let table = catalog
        .create_table(
            "public",
            "t",
            Schema {
                columns: vec![column("id", DataType::Int8, false)],
            },
        )
        .unwrap();
    storage.create_table(0, table).unwrap();
    let other = catalog
        .create_table(
            "public",
            "u",
            Schema {
                columns: vec![column("id", DataType::Int8, false)],
            },
        )
        .unwrap();
    storage.create_table(0, other).unwrap();
    for id in 0..4000i64 {
        storage
            .insert(0, table, Row::new(vec![Value::Int8(id)]))
            .unwrap();
        storage
            .insert(0, other, Row::new(vec![Value::Int8(id)]))
            .unwrap();
    }
    let procs = everything();

    // A cross join over 4000 rows is sixteen million comparisons, which
    // takes far longer than the budget allows.
    let statement = ferrite_sql::parse_statement("SELECT t.id FROM t JOIN u ON 1 = 1").unwrap();
    let plan = Planner::new(&catalog, &catalog).plan(&statement).unwrap();
    let outcome = Session::new(&storage, &catalog, &procs, CALLER)
        .with_limits(ferrite_exec::Limits {
            max_rows: 0,
            statement_timeout: Some(std::time::Duration::from_millis(50)),
        })
        .execute(1, &plan);
    assert!(
        matches!(outcome, Err(ferrite_common::FerriteError::Timeout(_))),
        "expected a timeout, got {outcome:?}"
    );

    // The same session answers a cheap statement within the same budget.
    let statement = ferrite_sql::parse_statement("SELECT id FROM t LIMIT 1").unwrap();
    let plan = Planner::new(&catalog, &catalog).plan(&statement).unwrap();
    assert!(Session::new(&storage, &catalog, &procs, CALLER)
        .with_limits(ferrite_exec::Limits {
            max_rows: 0,
            statement_timeout: Some(std::time::Duration::from_secs(30)),
        })
        .execute(1, &plan)
        .is_ok());
}
