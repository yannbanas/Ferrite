//! End-to-end: provisional AST -> planner -> physical plan -> executor,
//! against the in-memory storage/catalog in `support`.

mod support;

use ferrite_common::{
    Catalog, DataType, FerriteError, Identity, Permission, Role, Row, Schema, StorageEngine,
    TableId, Value,
};
use ferrite_exec::{QueryResult, Session};
use ferrite_planner::ast::{
    Assignment, BinaryOp, CallStmt, DeleteStmt, Expr, InsertStmt, SelectItem, SelectStmt,
    Statement, TableRef, UpdateStmt,
};
use ferrite_planner::Planner;
use ferrite_proc::{ProcDecision, ProcRegistry, Procedure, TriggerEvent};

use support::{column, MemCatalog, MemIndexes, MemStorage};

const OWNER: Identity = Identity([1u8; 32]);
const GUEST: Identity = Identity([2u8; 32]);

fn users_schema() -> Schema {
    Schema {
        columns: vec![
            column("id", DataType::Int8, false),
            column("name", DataType::Text, true),
            column("age", DataType::Int4, true),
        ],
    }
}

fn setup() -> (MemStorage, MemCatalog, TableId) {
    let storage = MemStorage::new();
    let catalog = MemCatalog::new();
    let table = catalog
        .create_table("public", "users", users_schema())
        .unwrap();
    storage.create_table(0, table).unwrap();
    catalog.add_index("users_pkey", table, 0, true);
    (storage, catalog, table)
}

fn full_access() -> ProcRegistry {
    let mut registry = ProcRegistry::new();
    registry.grant_role(
        OWNER,
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

fn insert_stmt(rows: Vec<Vec<Expr>>) -> Statement {
    Statement::Insert(InsertStmt {
        table: TableRef::new("users"),
        columns: Vec::new(),
        rows,
    })
}

fn person(id: i64, name: &str, age: i32) -> Vec<Expr> {
    vec![
        Expr::Literal(Value::Int8(id)),
        Expr::Literal(Value::Text(name.into())),
        Expr::Literal(Value::Int4(age)),
    ]
}

fn select_all(filter: Option<Expr>) -> Statement {
    Statement::Select(SelectStmt {
        projection: vec![SelectItem::Wildcard],
        from: TableRef::new("users"),
        filter,
        limit: None,
    })
}

fn id_eq(id: i64) -> Expr {
    Expr::eq(Expr::column("id"), Expr::Literal(Value::Int8(id)))
}

fn run(
    storage: &MemStorage,
    catalog: &MemCatalog,
    procs: &ProcRegistry,
    identity: Identity,
    stmt: &Statement,
) -> Result<QueryResult, FerriteError> {
    let planner = Planner::new(catalog, catalog);
    let plan = planner.plan(stmt)?;
    Session::new(storage, catalog, procs, identity).execute(1, &plan)
}

fn rows_of(result: QueryResult) -> Vec<Row> {
    match result {
        QueryResult::Rows { rows, .. } => rows,
        other => panic!("expected rows, got {other:?}"),
    }
}

#[test]
fn insert_then_select_round_trips() {
    let (storage, catalog, _) = setup();
    let procs = full_access();

    let inserted = run(
        &storage,
        &catalog,
        &procs,
        OWNER,
        &insert_stmt(vec![person(1, "ada", 36), person(2, "grace", 45)]),
    )
    .unwrap();
    assert_eq!(inserted, QueryResult::Affected(2));

    let rows = rows_of(run(&storage, &catalog, &procs, OWNER, &select_all(None)).unwrap());
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].values[1], Value::Text("ada".into()));
}

#[test]
fn an_equality_filter_really_goes_through_the_index() {
    let (storage, catalog, _) = setup();
    let procs = full_access();
    run(
        &storage,
        &catalog,
        &procs,
        OWNER,
        &insert_stmt(vec![person(1, "ada", 36), person(2, "grace", 45)]),
    )
    .unwrap();

    let indexes = MemIndexes::new(&storage, &catalog);
    let planner = Planner::new(&catalog, &catalog);
    let plan = planner.plan(&select_all(Some(id_eq(2)))).unwrap();

    let result = Session::new(&storage, &catalog, &procs, OWNER)
        .with_indexes(&indexes)
        .execute(1, &plan)
        .unwrap();

    let rows = rows_of(result);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].values[1], Value::Text("grace".into()));
    assert_eq!(
        indexes.lookups(),
        1,
        "the index path should have been taken"
    );
}

#[test]
fn an_index_scan_still_works_without_an_index_provider() {
    let (storage, catalog, _) = setup();
    let procs = full_access();
    run(
        &storage,
        &catalog,
        &procs,
        OWNER,
        &insert_stmt(vec![person(1, "ada", 36), person(2, "grace", 45)]),
    )
    .unwrap();

    let rows = rows_of(
        run(
            &storage,
            &catalog,
            &procs,
            OWNER,
            &select_all(Some(id_eq(1))),
        )
        .unwrap(),
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].values[1], Value::Text("ada".into()));
}

#[test]
fn a_before_insert_trigger_that_denies_leaves_no_row() {
    let (storage, catalog, table) = setup();
    let mut procs = full_access();
    procs.register_before(
        "adults_only",
        table,
        TriggerEvent::Insert,
        |_ctx, row| match row.values[2] {
            Value::Int4(age) if age >= 18 => Ok(ProcDecision::Allow),
            _ => Err(FerriteError::PermissionDenied("minors are refused".into())),
        },
    );

    let denied = run(
        &storage,
        &catalog,
        &procs,
        OWNER,
        &insert_stmt(vec![person(1, "kid", 12)]),
    );

    assert!(matches!(denied, Err(FerriteError::PermissionDenied(_))));
    assert!(
        storage.dump(table).is_empty(),
        "the refused row must not be in storage"
    );

    run(
        &storage,
        &catalog,
        &procs,
        OWNER,
        &insert_stmt(vec![person(2, "ada", 36)]),
    )
    .unwrap();
    assert_eq!(storage.dump(table).len(), 1);
}

#[test]
fn a_before_insert_trigger_can_rewrite_the_row() {
    let (storage, catalog, table) = setup();
    let mut procs = full_access();
    procs.register_before("anonymize", table, TriggerEvent::Insert, |_ctx, row| {
        let mut new = row.clone();
        new.values[1] = Value::Text("redacted".into());
        Ok(ProcDecision::Replace(new))
    });

    run(
        &storage,
        &catalog,
        &procs,
        OWNER,
        &insert_stmt(vec![person(1, "ada", 36)]),
    )
    .unwrap();

    assert_eq!(
        storage.dump(table)[0].values[1],
        Value::Text("redacted".into())
    );
}

#[test]
fn a_before_insert_trigger_can_skip_a_row_without_failing_the_statement() {
    let (storage, catalog, table) = setup();
    let mut procs = full_access();
    procs.register_before(
        "drop_odd_ids",
        table,
        TriggerEvent::Insert,
        |_ctx, row| match row.values[0] {
            Value::Int8(id) if id % 2 == 1 => Ok(ProcDecision::Skip),
            _ => Ok(ProcDecision::Allow),
        },
    );

    let result = run(
        &storage,
        &catalog,
        &procs,
        OWNER,
        &insert_stmt(vec![person(1, "ada", 36), person(2, "grace", 45)]),
    )
    .unwrap();

    assert_eq!(result, QueryResult::Affected(1));
    assert_eq!(storage.dump(table).len(), 1);
}

#[test]
fn a_trigger_sees_the_caller_identity_and_can_reject_a_stranger() {
    let (storage, catalog, table) = setup();
    let mut procs = full_access();
    procs.grant_role(
        GUEST,
        Role {
            name: "guest".into(),
            permissions: vec![Permission::Insert],
        },
    );
    procs.register_before("owner_only", table, TriggerEvent::Insert, |ctx, _row| {
        if ctx.sender() == OWNER {
            Ok(ProcDecision::Allow)
        } else {
            Err(FerriteError::PermissionDenied(
                "only the owner may write here".into(),
            ))
        }
    });

    assert!(matches!(
        run(
            &storage,
            &catalog,
            &procs,
            GUEST,
            &insert_stmt(vec![person(1, "mallory", 30)])
        ),
        Err(FerriteError::PermissionDenied(_))
    ));
    assert!(storage.dump(table).is_empty());

    run(
        &storage,
        &catalog,
        &procs,
        OWNER,
        &insert_stmt(vec![person(1, "ada", 36)]),
    )
    .unwrap();
    assert_eq!(storage.dump(table).len(), 1);
}

#[test]
fn update_applies_assignments_and_exposes_the_old_row_to_triggers() {
    let (storage, catalog, table) = setup();
    let mut procs = full_access();
    procs.register_before(
        "id_is_immutable",
        table,
        TriggerEvent::Update,
        |ctx, row| {
            let old = ctx.old_row().expect("BEFORE UPDATE must carry the old row");
            if old.values[0] == row.values[0] {
                Ok(ProcDecision::Allow)
            } else {
                Err(FerriteError::PermissionDenied("id is immutable".into()))
            }
        },
    );

    run(
        &storage,
        &catalog,
        &procs,
        OWNER,
        &insert_stmt(vec![person(1, "ada", 36), person(2, "grace", 45)]),
    )
    .unwrap();

    let stmt = Statement::Update(UpdateStmt {
        table: TableRef::new("users"),
        assignments: vec![Assignment {
            column: "name".into(),
            value: Expr::Literal(Value::Text("ada l.".into())),
        }],
        filter: Some(id_eq(1)),
    });
    assert_eq!(
        run(&storage, &catalog, &procs, OWNER, &stmt).unwrap(),
        QueryResult::Affected(1)
    );

    let stored = storage.dump(table);
    assert_eq!(stored[0].values[1], Value::Text("ada l.".into()));
    assert_eq!(stored[1].values[1], Value::Text("grace".into()));

    let forbidden = Statement::Update(UpdateStmt {
        table: TableRef::new("users"),
        assignments: vec![Assignment {
            column: "id".into(),
            value: Expr::Literal(Value::Int8(99)),
        }],
        filter: Some(id_eq(2)),
    });
    assert!(matches!(
        run(&storage, &catalog, &procs, OWNER, &forbidden),
        Err(FerriteError::PermissionDenied(_))
    ));
    assert_eq!(storage.dump(table)[1].values[0], Value::Int8(2));
}

#[test]
fn delete_removes_only_the_matching_rows_and_a_trigger_can_veto() {
    let (storage, catalog, table) = setup();
    let mut procs = full_access();
    procs.register_before("keep_grace", table, TriggerEvent::Delete, |_ctx, row| {
        if row.values[1] == Value::Text("grace".into()) {
            Err(FerriteError::PermissionDenied("grace stays".into()))
        } else {
            Ok(ProcDecision::Allow)
        }
    });

    run(
        &storage,
        &catalog,
        &procs,
        OWNER,
        &insert_stmt(vec![person(1, "ada", 36), person(2, "grace", 45)]),
    )
    .unwrap();

    let delete_ada = Statement::Delete(DeleteStmt {
        table: TableRef::new("users"),
        filter: Some(id_eq(1)),
    });
    assert_eq!(
        run(&storage, &catalog, &procs, OWNER, &delete_ada).unwrap(),
        QueryResult::Affected(1)
    );
    assert_eq!(storage.dump(table).len(), 1);

    let delete_grace = Statement::Delete(DeleteStmt {
        table: TableRef::new("users"),
        filter: Some(id_eq(2)),
    });
    assert!(matches!(
        run(&storage, &catalog, &procs, OWNER, &delete_grace),
        Err(FerriteError::PermissionDenied(_))
    ));
    assert_eq!(storage.dump(table).len(), 1);
}

#[test]
fn a_statement_is_denied_before_it_reaches_storage() {
    let (storage, catalog, table) = setup();
    let mut procs = ProcRegistry::new();
    procs.grant_role(
        GUEST,
        Role {
            name: "reader".into(),
            permissions: vec![Permission::Select],
        },
    );

    assert!(matches!(
        run(
            &storage,
            &catalog,
            &procs,
            GUEST,
            &insert_stmt(vec![person(1, "mallory", 30)])
        ),
        Err(FerriteError::PermissionDenied(_))
    ));
    assert!(storage.dump(table).is_empty());

    assert!(run(&storage, &catalog, &procs, GUEST, &select_all(None)).is_ok());

    assert!(matches!(
        run(&storage, &catalog, &procs, OWNER, &select_all(None)),
        Err(FerriteError::PermissionDenied(_))
    ));
}

#[test]
fn projection_and_limit_shape_the_result() {
    let (storage, catalog, _) = setup();
    let procs = full_access();
    run(
        &storage,
        &catalog,
        &procs,
        OWNER,
        &insert_stmt(vec![
            person(1, "ada", 36),
            person(2, "grace", 45),
            person(3, "alan", 41),
        ]),
    )
    .unwrap();

    let stmt = Statement::Select(SelectStmt {
        projection: vec![SelectItem::Expr {
            expr: Expr::column("name"),
            alias: Some("who".into()),
        }],
        from: TableRef::new("users"),
        filter: Some(Expr::binary(
            Expr::column("age"),
            BinaryOp::Gt,
            Expr::Literal(Value::Int4(35)),
        )),
        limit: Some(2),
    });

    let QueryResult::Rows { schema, rows } = run(&storage, &catalog, &procs, OWNER, &stmt).unwrap()
    else {
        panic!("expected rows");
    };
    assert_eq!(schema.columns.len(), 1);
    assert_eq!(schema.columns[0].name, "who");
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].values, vec![Value::Text("ada".into())]);
}

#[test]
fn call_runs_a_stored_procedure_under_the_callers_identity() {
    let (storage, catalog, _) = setup();
    let mut procs = full_access();
    procs.register_procedure(Procedure::new(
        "whoami",
        Permission::Execute,
        |ctx, _args| {
            let Identity(bytes) = ctx.sender();
            Ok(Value::Int4(i32::from(bytes[0])))
        },
    ));
    procs.grant_role(
        GUEST,
        Role {
            name: "guest".into(),
            permissions: vec![Permission::Select],
        },
    );

    let stmt = Statement::Call(CallStmt {
        name: "whoami".into(),
        args: Vec::new(),
    });

    assert_eq!(
        run(&storage, &catalog, &procs, OWNER, &stmt).unwrap(),
        QueryResult::Value(Value::Int4(1))
    );
    assert!(matches!(
        run(&storage, &catalog, &procs, GUEST, &stmt),
        Err(FerriteError::PermissionDenied(_))
    ));
}

#[test]
fn a_not_null_violation_is_rejected() {
    let (storage, catalog, table) = setup();
    let procs = full_access();

    let stmt = Statement::Insert(InsertStmt {
        table: TableRef::new("users"),
        columns: vec!["name".into()],
        rows: vec![vec![Expr::Literal(Value::Text("ada".into()))]],
    });

    assert!(run(&storage, &catalog, &procs, OWNER, &stmt).is_err());
    assert!(storage.dump(table).is_empty());
}

#[test]
fn a_plan_built_against_an_older_schema_is_rejected() {
    let (storage, catalog, table) = setup();
    let procs = full_access();

    let planner = Planner::new(&catalog, &catalog);
    let plan = planner.plan(&select_all(None)).unwrap();

    let mut altered = users_schema();
    altered.columns.push(column("email", DataType::Text, true));
    catalog.replace_schema(table, altered);

    let result = Session::new(&storage, &catalog, &procs, OWNER).execute(1, &plan);
    assert!(matches!(result, Err(FerriteError::Plan(_))));
}
