use super::*;
use crate::logical::JoinType;
use ferrite_common::{DataType, IndexId, TableId};
use std::collections::HashMap;

struct TestCatalog {
    tables: HashMap<String, (TableId, Schema)>,
}

fn column(name: &str, data_type: DataType, nullable: bool) -> ColumnDef {
    ColumnDef::new(name, data_type, nullable)
}

impl TestCatalog {
    fn users() -> Self {
        let users = Schema {
            columns: vec![
                column("id", DataType::Int8, false),
                column("name", DataType::Text, true),
                column("age", DataType::Int4, true),
            ],
        };
        let posts = Schema {
            columns: vec![
                column("id", DataType::Int8, false),
                column("author", DataType::Int8, true),
                column("title", DataType::Text, true),
            ],
        };
        let mut tables = HashMap::new();
        tables.insert("public.users".to_string(), (1, users));
        tables.insert("public.posts".to_string(), (2, posts));
        Self { tables }
    }
}

impl Catalog for TestCatalog {
    fn table_id(&self, schema: &str, name: &str) -> Result<Option<TableId>, FerriteError> {
        Ok(self.tables.get(&format!("{schema}.{name}")).map(|t| t.0))
    }
    fn table_schema(&self, table: TableId) -> Result<Schema, FerriteError> {
        self.tables
            .values()
            .find(|(id, _)| *id == table)
            .map(|(_, s)| s.clone())
            .ok_or(FerriteError::RowNotFound)
    }
    fn create_table(
        &self,
        _schema: &str,
        _name: &str,
        _columns: Schema,
    ) -> Result<TableId, FerriteError> {
        unimplemented!("not needed for planner tests")
    }
    fn drop_table(&self, _table: TableId) -> Result<(), FerriteError> {
        unimplemented!("not needed for planner tests")
    }
    fn list_tables(&self, _schema: &str) -> Result<Vec<(TableId, String)>, FerriteError> {
        unimplemented!("not needed for planner tests")
    }
}

#[derive(Default)]
struct TestIndexes(Vec<IndexDef>);

impl IndexCatalog for TestIndexes {
    fn create_index(
        &self,
        _name: &str,
        _table: TableId,
        _columns: &[String],
        _unique: bool,
    ) -> Result<IndexId, FerriteError> {
        unimplemented!("not needed for planner tests")
    }
    fn drop_index(&self, _index: IndexId) -> Result<(), FerriteError> {
        unimplemented!("not needed for planner tests")
    }
    fn index(&self, index: IndexId) -> Result<Option<IndexDef>, FerriteError> {
        Ok(self.0.iter().find(|i| i.id == index).cloned())
    }
    fn index_by_name(
        &self,
        _namespace: &str,
        name: &str,
    ) -> Result<Option<IndexDef>, FerriteError> {
        Ok(self.0.iter().find(|i| i.name == name).cloned())
    }
    fn indexes_for(&self, table: TableId) -> Result<Vec<IndexDef>, FerriteError> {
        Ok(self
            .0
            .iter()
            .filter(|i| i.table == table)
            .cloned()
            .collect())
    }
}

fn id_index() -> TestIndexes {
    TestIndexes(vec![IndexDef {
        id: 20,
        name: "users_pkey".into(),
        table: 1,
        columns: vec!["id".into()],
        unique: true,
    }])
}

fn plan_with(indexes: &TestIndexes, sql: &str) -> Result<PhysicalPlan, FerriteError> {
    let catalog = TestCatalog::users();
    let planner = Planner::new(&catalog, indexes);
    planner.plan(&ferrite_sql::parse_statement(sql)?)
}

fn plan(sql: &str) -> Result<PhysicalPlan, FerriteError> {
    plan_with(&id_index(), sql)
}

fn plan_unindexed(sql: &str) -> Result<PhysicalPlan, FerriteError> {
    plan_with(&TestIndexes::default(), sql)
}

fn output_names(plan: &PhysicalPlan) -> Vec<String> {
    plan.output_schema()
        .expect("a query plan has an output schema")
        .columns
        .iter()
        .map(|c| c.name.clone())
        .collect()
}

#[test]
fn equality_on_an_indexed_column_selects_an_index_scan() {
    match plan("SELECT * FROM users WHERE id = 42").unwrap() {
        PhysicalPlan::IndexScan {
            index,
            column,
            key,
            residual,
            ..
        } => {
            assert_eq!(index, "users_pkey");
            assert_eq!(column, 0);
            assert_eq!(
                key,
                PhysExpr::Literal(Value::Int8(42)),
                "the key is coerced to the indexed column's declared type"
            );
            assert!(residual.is_none());
        }
        other => panic!("expected an IndexScan, got {other:?}"),
    }
}

#[test]
fn equality_on_an_unindexed_column_falls_back_to_a_seq_scan() {
    match plan("SELECT * FROM users WHERE name = 'ada'").unwrap() {
        PhysicalPlan::SeqScan { filter, .. } => assert_eq!(
            filter,
            Some(PhysExpr::binary(
                PhysExpr::Column(1),
                BinaryOp::Eq,
                PhysExpr::Literal(Value::Text("ada".into()))
            ))
        ),
        other => panic!("expected a SeqScan, got {other:?}"),
    }
}

#[test]
fn a_range_predicate_never_selects_an_index() {
    assert!(matches!(
        plan("SELECT * FROM users WHERE id > 10").unwrap(),
        PhysicalPlan::SeqScan { .. }
    ));
}

#[test]
fn no_index_catalog_entry_means_seq_scan() {
    assert!(matches!(
        plan_unindexed("SELECT * FROM users WHERE id = 42").unwrap(),
        PhysicalPlan::SeqScan { .. }
    ));
}

#[test]
fn the_non_indexable_conjunct_survives_as_a_residual() {
    match plan("SELECT * FROM users WHERE id = 42 AND age > 18").unwrap() {
        PhysicalPlan::IndexScan { residual, .. } => assert_eq!(
            residual,
            Some(PhysExpr::binary(
                PhysExpr::Column(2),
                BinaryOp::Gt,
                PhysExpr::Literal(Value::Int4(18))
            ))
        ),
        other => panic!("expected an IndexScan, got {other:?}"),
    }
}

#[test]
fn equality_against_null_does_not_probe_an_index() {
    assert!(matches!(
        plan("SELECT * FROM users WHERE id = NULL").unwrap(),
        PhysicalPlan::SeqScan { .. }
    ));
}

#[test]
fn pushdown_and_index_selection_compose() {
    let plan = plan("SELECT name FROM users WHERE id = 1 LIMIT 5").unwrap();

    let PhysicalPlan::Limit { input, count, .. } = plan else {
        panic!("expected a Limit at the root");
    };
    assert_eq!(count, Some(5));
    let PhysicalPlan::Projection { input, output, .. } = *input else {
        panic!("expected a Projection under the Limit");
    };
    assert_eq!(output.columns.len(), 1);
    assert_eq!(output.columns[0].name, "name");
    assert!(
        matches!(*input, PhysicalPlan::IndexScan { .. }),
        "the pushed-down predicate should have selected an index"
    );
}

#[test]
fn update_lowers_to_an_index_scan_source() {
    let plan = plan("UPDATE users SET name = 'grace' WHERE id = 3").unwrap();

    let PhysicalPlan::Update {
        source,
        assignments,
        ..
    } = plan
    else {
        panic!("expected an Update at the root");
    };
    assert_eq!(assignments.len(), 1);
    assert_eq!(assignments[0].0, 1);
    assert!(matches!(*source, PhysicalPlan::IndexScan { .. }));
}

#[test]
fn insert_pads_omitted_columns_with_null() {
    let plan = plan_unindexed("INSERT INTO users (name) VALUES ('ada')").unwrap();

    let PhysicalPlan::Insert { rows, .. } = plan else {
        panic!("expected an Insert");
    };
    assert_eq!(
        rows[0],
        vec![
            PhysExpr::Literal(Value::Null),
            PhysExpr::Literal(Value::Text("ada".into())),
            PhysExpr::Literal(Value::Null),
        ]
    );
}

#[test]
fn an_unknown_table_is_a_planning_error() {
    assert!(matches!(
        plan("SELECT * FROM ghosts"),
        Err(FerriteError::TableNotFound(_))
    ));
}

#[test]
fn an_unknown_column_is_a_planning_error() {
    assert!(matches!(
        plan("SELECT * FROM users WHERE nope = 1"),
        Err(FerriteError::ColumnNotFound(_))
    ));
}

#[test]
fn parameters_are_substituted_as_literals() {
    let catalog = TestCatalog::users();
    let indexes = id_index();
    let params = [Value::Int8(7)];
    let planner = Planner::new(&catalog, &indexes).with_params(&params);
    let stmt = ferrite_sql::parse_statement("SELECT * FROM users WHERE id = $1").unwrap();

    match planner.plan(&stmt).unwrap() {
        PhysicalPlan::IndexScan { key, .. } => {
            assert_eq!(key, PhysExpr::Literal(Value::Int8(7)))
        }
        other => panic!("expected an IndexScan, got {other:?}"),
    }
}

#[test]
fn an_unbound_parameter_is_a_planning_error() {
    assert!(matches!(
        plan("SELECT * FROM users WHERE id = $1"),
        Err(FerriteError::Plan(_))
    ));
}

#[test]
fn a_table_alias_qualifies_column_references() {
    assert!(plan("SELECT u.name FROM users u WHERE u.id = 1").is_ok());
    assert!(matches!(
        plan("SELECT * FROM users u WHERE other.id = 1"),
        Err(FerriteError::Plan(_))
    ));
}

#[test]
fn between_and_in_expand_into_comparisons() {
    assert!(plan("SELECT * FROM users WHERE age BETWEEN 18 AND 99").is_ok());
    assert!(plan("SELECT * FROM users WHERE age IN (1, 2, 3)").is_ok());
}

#[test]
fn a_join_produces_the_columns_of_both_relations() {
    let plan = plan("SELECT * FROM users u JOIN posts p ON p.author = u.id").unwrap();

    let PhysicalPlan::NestedLoopJoin {
        join_type,
        predicate,
        output,
        ..
    } = &plan
    else {
        panic!("expected a join, got {plan:?}");
    };
    assert_eq!(*join_type, JoinType::Inner);
    assert_eq!(output.columns.len(), 6);
    assert_eq!(
        *predicate,
        Some(PhysExpr::binary(
            PhysExpr::Column(4),
            BinaryOp::Eq,
            PhysExpr::Column(0)
        )),
        "both sides bind against the concatenated row"
    );
}

#[test]
fn a_left_join_makes_the_right_side_nullable() {
    let plan = plan("SELECT * FROM users u LEFT JOIN posts p ON p.author = u.id").unwrap();
    let schema = plan.output_schema().unwrap();
    assert!(!schema.columns[0].nullable, "users.id stays NOT NULL");
    assert!(schema.columns[3].nullable, "posts.id may now be NULL");
}

#[test]
fn using_resolves_the_same_column_on_both_sides() {
    let plan = plan("SELECT * FROM users JOIN posts USING (id)").unwrap();
    let PhysicalPlan::NestedLoopJoin { predicate, .. } = &plan else {
        panic!("expected a join, got {plan:?}");
    };
    assert_eq!(
        *predicate,
        Some(PhysExpr::binary(
            PhysExpr::Column(0),
            BinaryOp::Eq,
            PhysExpr::Column(3)
        ))
    );
}

#[test]
fn an_unqualified_name_present_on_both_sides_of_a_join_is_ambiguous() {
    assert!(matches!(
        plan("SELECT id FROM users JOIN posts ON posts.author = users.id"),
        Err(FerriteError::Plan(_))
    ));
}

#[test]
fn a_qualified_wildcard_selects_one_relation() {
    let plan = plan("SELECT p.* FROM users u JOIN posts p ON p.author = u.id").unwrap();
    assert_eq!(output_names(&plan), ["id", "author", "title"]);
}

#[test]
fn count_star_is_typed_as_a_non_null_bigint() {
    let plan = plan("SELECT count(*) FROM users").unwrap();
    let schema = plan.output_schema().unwrap();
    assert_eq!(schema.columns[0].name, "count");
    assert_eq!(schema.columns[0].data_type, DataType::Int8);
    assert!(!schema.columns[0].nullable);
}

#[test]
fn a_group_key_in_the_select_list_becomes_a_slot() {
    let plan = plan("SELECT age, count(*) FROM users GROUP BY age").unwrap();

    let PhysicalPlan::Projection { input, exprs, .. } = &plan else {
        panic!("expected a Projection, got {plan:?}");
    };
    assert_eq!(
        *exprs,
        vec![PhysExpr::Column(0), PhysExpr::Column(1)],
        "the projection reads the aggregate's output row"
    );
    assert!(matches!(**input, PhysicalPlan::Aggregate { .. }));
    assert_eq!(output_names(&plan), ["age", "count"]);
}

#[test]
fn the_same_aggregate_twice_is_computed_once() {
    let plan = plan("SELECT count(*), count(*) FROM users").unwrap();
    let PhysicalPlan::Projection { input, .. } = &plan else {
        panic!("expected a Projection, got {plan:?}");
    };
    let PhysicalPlan::Aggregate { aggregates, .. } = input.as_ref() else {
        panic!("expected an Aggregate under the Projection");
    };
    assert_eq!(aggregates.len(), 1);
}

#[test]
fn selecting_a_column_that_is_neither_grouped_nor_aggregated_is_refused() {
    assert!(matches!(
        plan("SELECT name, count(*) FROM users GROUP BY age"),
        Err(FerriteError::Plan(_))
    ));
}

#[test]
fn having_becomes_a_filter_above_the_aggregate() {
    let plan = plan("SELECT age FROM users GROUP BY age HAVING count(*) > 1").unwrap();
    let PhysicalPlan::Projection { input, .. } = &plan else {
        panic!("expected a Projection, got {plan:?}");
    };
    let PhysicalPlan::Filter { input, .. } = input.as_ref() else {
        panic!("expected a Filter under the Projection, got {input:?}");
    };
    assert!(matches!(**input, PhysicalPlan::Aggregate { .. }));
}

#[test]
fn an_aggregate_in_where_is_refused() {
    assert!(matches!(
        plan("SELECT name FROM users WHERE count(*) > 1"),
        Err(FerriteError::Plan(_))
    ));
}

#[test]
fn order_by_defaults_nulls_last_ascending_and_first_descending() {
    let PhysicalPlan::Sort { keys, .. } =
        plan("SELECT * FROM users ORDER BY age, name DESC").unwrap()
    else {
        panic!("expected a Sort at the root");
    };
    assert_eq!((keys[0].asc, keys[0].nulls_first), (true, false));
    assert_eq!((keys[1].asc, keys[1].nulls_first), (false, true));
}

#[test]
fn order_by_accepts_a_select_list_position_and_an_alias() {
    for sql in [
        "SELECT name AS who FROM users ORDER BY 1",
        "SELECT name AS who FROM users ORDER BY who",
    ] {
        let PhysicalPlan::Projection { input, .. } = plan(sql).unwrap() else {
            panic!("expected a Projection at the root of {sql:?}");
        };
        let PhysicalPlan::Sort { keys, .. } = input.as_ref() else {
            panic!("expected a Sort below the projection of {sql:?}");
        };
        assert_eq!(keys[0].expr, PhysExpr::Column(1));
    }
    assert!(matches!(
        plan("SELECT name FROM users ORDER BY 7"),
        Err(FerriteError::Plan(_))
    ));
}

#[test]
fn limit_and_offset_accept_a_bound_parameter() {
    let catalog = TestCatalog::users();
    let indexes = TestIndexes::default();
    let params = [Value::Int8(10), Value::Int8(20)];
    let planner = Planner::new(&catalog, &indexes).with_params(&params);
    let stmt = ferrite_sql::parse_statement("SELECT * FROM users LIMIT $1 OFFSET $2").unwrap();

    let PhysicalPlan::Limit { count, offset, .. } = planner.plan(&stmt).unwrap() else {
        panic!("expected a Limit at the root");
    };
    assert_eq!((count, offset), (Some(10), 20));
}

#[test]
fn a_negative_limit_is_a_planning_error() {
    assert!(matches!(
        plan("SELECT * FROM users LIMIT -1"),
        Err(FerriteError::Plan(_))
    ));
}

#[test]
fn distinct_wraps_the_projection() {
    let plan = plan("SELECT DISTINCT name FROM users").unwrap();
    let PhysicalPlan::Distinct { input } = &plan else {
        panic!("expected a Distinct at the root, got {plan:?}");
    };
    assert!(matches!(**input, PhysicalPlan::Projection { .. }));
}

#[test]
fn like_binds_both_operands() {
    let PhysicalPlan::SeqScan { filter, .. } =
        plan("SELECT * FROM users WHERE name NOT LIKE 'a%'").unwrap()
    else {
        panic!("expected a SeqScan");
    };
    assert_eq!(
        filter,
        Some(PhysExpr::Like {
            expr: Box::new(PhysExpr::Column(1)),
            pattern: Box::new(PhysExpr::Literal(Value::Text("a%".into()))),
            negated: true,
            case_insensitive: false,
        })
    );
}

#[test]
fn ilike_binds_the_same_way_but_folds_case() {
    let PhysicalPlan::SeqScan { filter, .. } =
        plan("SELECT * FROM users WHERE name ILIKE 'a%'").unwrap()
    else {
        panic!("expected a SeqScan");
    };
    assert_eq!(
        filter,
        Some(PhysExpr::Like {
            expr: Box::new(PhysExpr::Column(1)),
            pattern: Box::new(PhysExpr::Literal(Value::Text("a%".into()))),
            negated: false,
            case_insensitive: true,
        })
    );
}

#[test]
fn collate_nocase_on_one_operand_folds_both_sides_of_the_comparison() {
    let PhysicalPlan::SeqScan { filter, .. } =
        plan("SELECT * FROM users WHERE name = 'Ada' COLLATE NOCASE").unwrap()
    else {
        panic!("expected a SeqScan");
    };
    let fold = |inner| PhysExpr::Function {
        func: crate::ScalarFunc::Nocase,
        args: vec![inner],
    };
    assert_eq!(
        filter,
        Some(PhysExpr::binary(
            fold(PhysExpr::Column(1)),
            BinaryOp::Eq,
            fold(PhysExpr::Literal(Value::Text("Ada".into()))),
        ))
    );
}

#[test]
fn datetime_now_is_folded_to_one_literal_for_the_whole_statement() {
    let PhysicalPlan::SeqScan { filter, .. } =
        plan("SELECT * FROM users WHERE name > datetime('now', '-30 days')").unwrap()
    else {
        panic!("expected a SeqScan");
    };
    let Some(PhysExpr::Binary { right, .. }) = filter else {
        panic!("expected a comparison");
    };
    assert!(matches!(*right, PhysExpr::Literal(Value::Text(_))));
}

#[test]
fn an_unknown_function_is_refused_by_name() {
    let error = plan("SELECT strftime('%Y', name) FROM users").unwrap_err();
    assert!(error.to_string().contains("strftime"), "{error}");
}

#[test]
fn an_unknown_collation_is_refused_rather_than_ignored() {
    let error = plan("SELECT * FROM users WHERE name = 'a' COLLATE rtrim").unwrap_err();
    assert!(error.to_string().contains("rtrim"), "{error}");
}

#[test]
fn arithmetic_in_a_projection_keeps_the_wider_type() {
    let plan = plan("SELECT id + age AS total FROM users").unwrap();
    let schema = plan.output_schema().unwrap();
    assert_eq!(schema.columns[0].name, "total");
    assert_eq!(schema.columns[0].data_type, DataType::Int8);
}

#[test]
fn everything_outside_the_executable_subset_is_a_plan_error() {
    for sql in [
        "SELECT * FROM users UNION SELECT * FROM users",
        "WITH x (id) AS (SELECT id FROM users) SELECT * FROM x",
        "SELECT * FROM (SELECT id FROM users) s",
        "SELECT * FROM users WHERE EXISTS (SELECT 1 FROM users)",
        "SELECT strftime('%Y', name) FROM users",
        "SELECT lower(name, name) FROM users",
        "INSERT INTO users (name) VALUES ('a') RETURNING id",
        "INSERT INTO users SELECT id, name, age FROM users",
        "UPDATE users SET name = 'a' RETURNING id",
        "DELETE FROM users RETURNING id",
        "BEGIN",
        "CREATE TABLE t (a INT)",
        "DROP TABLE users",
        "CREATE INDEX i ON users (name)",
    ] {
        assert!(
            matches!(plan(sql), Err(FerriteError::Plan(_))),
            "{sql:?} should be a Plan error, got {:?}",
            plan(sql)
        );
    }
}

#[test]
fn a_text_literal_is_coerced_to_the_target_column_type() {
    struct Events;
    impl Catalog for Events {
        fn table_id(&self, _ns: &str, name: &str) -> Result<Option<TableId>, FerriteError> {
            Ok((name == "events").then_some(2))
        }
        fn table_schema(&self, _table: TableId) -> Result<Schema, FerriteError> {
            Ok(Schema {
                columns: vec![
                    column("id", DataType::Uuid, false),
                    column("at", DataType::Timestamp, false),
                ],
            })
        }
        fn create_table(
            &self,
            _ns: &str,
            _name: &str,
            _columns: Schema,
        ) -> Result<TableId, FerriteError> {
            unimplemented!()
        }
        fn drop_table(&self, _table: TableId) -> Result<(), FerriteError> {
            unimplemented!()
        }
        fn list_tables(&self, _ns: &str) -> Result<Vec<(TableId, String)>, FerriteError> {
            unimplemented!()
        }
    }

    let indexes = TestIndexes::default();
    let planner = Planner::new(&Events, &indexes);
    let stmt = ferrite_sql::parse_statement(
        "INSERT INTO events VALUES ('0190f0d8-4b1a-7c3e-9d2f-1a2b3c4d5e6f', \
         '2024-02-29T12:00:00Z')",
    )
    .unwrap();

    let PhysicalPlan::Insert { rows, .. } = planner.plan(&stmt).unwrap() else {
        panic!("expected an Insert");
    };
    assert_eq!(
        rows[0],
        vec![
            PhysExpr::Literal(Value::Uuid(0x0190_f0d8_4b1a_7c3e_9d2f_1a2b_3c4d_5e6f)),
            PhysExpr::Literal(Value::Timestamp(1_709_208_000_000_000)),
        ]
    );
}

/// A column an `INSERT` leaves out takes its `DEFAULT`, not `NULL` — the
/// case that used to write a silently wrong value into a nullable column
/// and a hard failure into a `NOT NULL` one.
#[test]
fn insert_fills_omitted_columns_with_their_default() {
    let mut schema = Schema {
        columns: vec![
            ColumnDef::new("id", DataType::Int8, false),
            ColumnDef::new("name", DataType::Text, true),
            ColumnDef::new("totp_enabled", DataType::Int8, false)
                .with_default(ColumnDefault::Constant(Value::Int4(0))),
            ColumnDef::new("visibility", DataType::Text, false)
                .with_default(ColumnDefault::Constant(Value::Text("everyone".into()))),
            ColumnDef::new("created_at", DataType::Timestamp, false)
                .with_default(ColumnDefault::CurrentTimestamp),
        ],
    };
    // The constant leaves `ferrite-sql` untyped; this is the step that pins
    // it to the column's declared type.
    crate::typecheck_defaults(&mut schema).unwrap();
    assert_eq!(
        schema.columns[2].default,
        Some(ColumnDefault::Constant(Value::Int8(0))),
        "the literal widens to the column's type at DDL time"
    );

    let mut tables = HashMap::new();
    tables.insert("public.users".to_string(), (1, schema));
    let catalog = TestCatalog { tables };
    let indexes = TestIndexes::default();
    let stmt = ferrite_sql::parse_statement("INSERT INTO users (id) VALUES (7)").unwrap();
    let PhysicalPlan::Insert { rows, .. } = Planner::new(&catalog, &indexes).plan(&stmt).unwrap()
    else {
        panic!("expected an Insert");
    };
    assert_eq!(rows[0][0], PhysExpr::Literal(Value::Int8(7)));
    assert_eq!(rows[0][1], PhysExpr::Literal(Value::Null));
    assert_eq!(rows[0][2], PhysExpr::Literal(Value::Int8(0)));
    assert_eq!(
        rows[0][3],
        PhysExpr::Literal(Value::Text("everyone".into()))
    );
    assert!(
        matches!(rows[0][4], PhysExpr::Literal(Value::Timestamp(t)) if t > 0),
        "CURRENT_TIMESTAMP is folded to one literal for the whole statement"
    );
}

#[test]
fn a_default_that_cannot_hold_is_refused_at_ddl_time() {
    let checked = |column: ColumnDef| {
        crate::typecheck_defaults(&mut Schema {
            columns: vec![column],
        })
    };
    assert!(checked(
        ColumnDef::new("a", DataType::Int8, false)
            .with_default(ColumnDefault::Constant(Value::Null))
    )
    .is_err());
    assert!(checked(
        ColumnDef::new("a", DataType::Int8, true)
            .with_default(ColumnDefault::Constant(Value::Text("x".into())))
    )
    .is_err());
    assert!(checked(
        ColumnDef::new("a", DataType::Text, true).with_default(ColumnDefault::CurrentTimestamp)
    )
    .is_err());
    assert!(checked(
        ColumnDef::new("a", DataType::Int8, true)
            .with_default(ColumnDefault::Constant(Value::Null))
    )
    .is_ok());
    assert!(checked(
        ColumnDef::new("a", DataType::Timestamp, false).with_default(ColumnDefault::Constant(
            Value::Text("2026-08-10T12:00:00Z".into())
        ))
    )
    .is_ok());
}

#[test]
fn an_uncorrelated_in_subquery_becomes_a_subplan_the_executor_runs() {
    let PhysicalPlan::SeqScan { filter, .. } =
        plan("SELECT * FROM users WHERE id IN (SELECT author FROM posts)").unwrap()
    else {
        panic!("expected a SeqScan");
    };
    assert!(matches!(filter, Some(PhysExpr::InSubquery { .. })));
}

#[test]
fn a_correlated_in_subquery_is_refused_by_the_column_it_cannot_see() {
    let error = plan(
        "SELECT * FROM users WHERE id IN (SELECT author FROM posts WHERE posts.author = users.id)",
    )
    .unwrap_err();
    assert!(error.to_string().contains("users"), "{error}");
}
