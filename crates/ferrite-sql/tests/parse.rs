use ferrite_common::{ColumnDefault, DataType, Value};
use ferrite_sql::ast::*;
use ferrite_sql::{parse, parse_statement};

fn stmt(sql: &str) -> Statement {
    match parse_statement(sql) {
        Ok(s) => s,
        Err(e) => panic!("expected `{sql}` to parse, got: {e}"),
    }
}

fn query(sql: &str) -> Query {
    match stmt(sql) {
        Statement::Query(q) => *q,
        other => panic!("expected a query, got {other:?}"),
    }
}

fn select(sql: &str) -> Select {
    match query(sql).body {
        SetExpr::Select(s) => *s,
        other => panic!("expected a plain select, got {other:?}"),
    }
}

fn rejects(sql: &str) {
    if let Ok(parsed) = parse(sql) {
        panic!("expected `{sql}` to be rejected, got {parsed:?}");
    }
}

#[test]
fn empty_input_yields_no_statements() {
    assert!(parse("").unwrap().is_empty());
    assert!(parse("   \n\t ").unwrap().is_empty());
    assert!(parse("-- just a comment").unwrap().is_empty());
    assert!(parse("/* nested /* comment */ */").unwrap().is_empty());
    assert!(parse(";;;").unwrap().is_empty());
}

#[test]
fn multiple_statements() {
    let stmts = parse("BEGIN; SELECT 1; COMMIT;").unwrap();
    assert_eq!(stmts.len(), 3);
    assert_eq!(stmts[0], Statement::Begin);
    assert_eq!(stmts[2], Statement::Commit);
    assert_eq!(parse("SELECT 1; SELECT 2").unwrap().len(), 2);
}

#[test]
fn transaction_control() {
    assert_eq!(stmt("BEGIN"), Statement::Begin);
    assert_eq!(stmt("BEGIN TRANSACTION"), Statement::Begin);
    assert_eq!(stmt("BEGIN WORK"), Statement::Begin);
    assert_eq!(stmt("START TRANSACTION"), Statement::Begin);
    assert_eq!(stmt("COMMIT"), Statement::Commit);
    assert_eq!(stmt("END"), Statement::Commit);
    assert_eq!(stmt("ROLLBACK"), Statement::Rollback);
    assert_eq!(stmt("rollback work"), Statement::Rollback);
}

#[test]
fn create_table_all_types() {
    let sql = "CREATE TABLE public.users (
        id UUID PRIMARY KEY,
        email TEXT NOT NULL UNIQUE,
        nickname VARCHAR(64),
        age INT,
        balance BIGINT NOT NULL DEFAULT 0,
        ratio DOUBLE PRECISION,
        active BOOLEAN NOT NULL DEFAULT true,
        created_at TIMESTAMP NOT NULL,
        profile JSONB
    )";
    let created = match stmt(sql) {
        Statement::CreateTable(c) => c,
        other => panic!("{other:?}"),
    };
    assert!(!created.if_not_exists);
    assert_eq!(created.name.split("public"), ("public", "users"));
    let types: Vec<DataType> = created.columns.iter().map(|c| c.data_type).collect();
    assert_eq!(
        types,
        vec![
            DataType::Uuid,
            DataType::Text,
            DataType::Text,
            DataType::Int4,
            DataType::Int8,
            DataType::Float8,
            DataType::Boolean,
            DataType::Timestamp,
            DataType::Json,
        ]
    );

    let schema = created.to_schema().unwrap();
    assert_eq!(schema.columns.len(), 9);
    assert!(!schema.columns[0].nullable, "primary key implies not null");
    assert!(!schema.columns[1].nullable);
    assert!(schema.columns[2].nullable);
    assert_eq!(schema.column_index("profile"), Some(8));
}

#[test]
fn create_table_if_not_exists_and_table_constraints() {
    let created = match stmt(
        "CREATE TABLE IF NOT EXISTS memberships (user_id UUID, group_id UUID, \
         PRIMARY KEY (user_id, group_id), UNIQUE (group_id))",
    ) {
        Statement::CreateTable(c) => c,
        other => panic!("{other:?}"),
    };
    assert!(created.if_not_exists);
    assert_eq!(
        created.constraints,
        vec![
            TableConstraint::PrimaryKey(vec!["user_id".into(), "group_id".into()]),
            TableConstraint::Unique(vec!["group_id".into()]),
        ]
    );
    assert!(created
        .to_schema()
        .unwrap()
        .columns
        .iter()
        .all(|c| !c.nullable));
}

#[test]
fn column_defaults_reach_the_schema() {
    let created = match stmt(
        "CREATE TABLE users (id UUID PRIMARY KEY, \
         totp_enabled BIGINT NOT NULL DEFAULT 0, \
         visibility TEXT NOT NULL DEFAULT 'everyone', \
         balance DOUBLE PRECISION DEFAULT -1.5, \
         verified BOOLEAN DEFAULT false, \
         nickname TEXT DEFAULT NULL, \
         created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP, \
         seen_at TIMESTAMP DEFAULT now(), \
         bio TEXT)",
    ) {
        Statement::CreateTable(c) => c,
        other => panic!("{other:?}"),
    };
    let schema = created.to_schema().unwrap();
    let of = |name: &str| {
        schema.columns[schema.column_index(name).expect(name)]
            .default
            .clone()
    };
    assert_eq!(of("id"), None);
    assert_eq!(of("bio"), None);
    assert_eq!(
        of("totp_enabled"),
        Some(ColumnDefault::Constant(Value::Int4(0)))
    );
    assert_eq!(
        of("visibility"),
        Some(ColumnDefault::Constant(Value::Text("everyone".into())))
    );
    assert_eq!(
        of("balance"),
        Some(ColumnDefault::Constant(Value::Float8(-1.5)))
    );
    assert_eq!(
        of("verified"),
        Some(ColumnDefault::Constant(Value::Boolean(false)))
    );
    assert_eq!(of("nickname"), Some(ColumnDefault::Constant(Value::Null)));
    assert_eq!(of("created_at"), Some(ColumnDefault::CurrentTimestamp));
    assert_eq!(of("seen_at"), Some(ColumnDefault::CurrentTimestamp));

    // A default outside the stored subset is an error rather than a
    // silently dropped clause.
    for sql in [
        "CREATE TABLE t (a BIGINT DEFAULT 1 + 1)",
        "CREATE TABLE t (a BIGINT DEFAULT random())",
        "CREATE TABLE t (a BIGINT DEFAULT b)",
        "CREATE TABLE t (a TIMESTAMP DEFAULT now(1))",
    ] {
        let Statement::CreateTable(c) = stmt(sql) else {
            panic!("{sql}");
        };
        assert!(c.to_schema().is_err(), "{sql:?} should have been refused");
    }
}

#[test]
fn alter_table_add_column() {
    let altered = match stmt("ALTER TABLE IF EXISTS public.users ADD COLUMN IF NOT EXISTS bio TEXT")
    {
        Statement::AlterTable(a) => a,
        other => panic!("{other:?}"),
    };
    assert!(altered.if_exists);
    assert_eq!(altered.name.split("public"), ("public", "users"));
    let AlterTableAction::AddColumn {
        if_not_exists,
        column,
    } = &altered.action;
    assert!(if_not_exists);
    assert_eq!(column.name, "bio");
    assert_eq!(column.data_type, DataType::Text);

    // `COLUMN` is optional, as it is in PostgreSQL.
    let altered = match stmt("ALTER TABLE t ADD flag BIGINT NOT NULL DEFAULT 0") {
        Statement::AlterTable(a) => a,
        other => panic!("{other:?}"),
    };
    assert!(!altered.if_exists);
    let AlterTableAction::AddColumn {
        if_not_exists,
        column,
    } = &altered.action;
    assert!(!if_not_exists);
    let def = column.to_column_def(false).unwrap();
    assert!(!def.nullable);
    assert_eq!(def.default, Some(ColumnDefault::Constant(Value::Int4(0))));

    // Every other action is named in the error, never accepted as a no-op.
    for sql in [
        "ALTER TABLE t DROP COLUMN a",
        "ALTER TABLE t RENAME TO u",
        "ALTER TABLE t ALTER COLUMN a TYPE TEXT",
        "ALTER TABLE t ADD",
        "ALTER TABLE t ADD COLUMN a",
        "ALTER TABLE ADD COLUMN a TEXT",
        "ALTER INDEX i RENAME TO j",
    ] {
        rejects(sql);
    }
}

#[test]
fn drop_table() {
    let dropped = match stmt("DROP TABLE IF EXISTS a, public.b CASCADE") {
        Statement::DropTable(d) => d,
        other => panic!("{other:?}"),
    };
    assert!(dropped.if_exists);
    assert!(dropped.cascade);
    assert_eq!(dropped.names.len(), 2);
    assert_eq!(dropped.names[1].split("public"), ("public", "b"));

    let dropped = match stmt("DROP TABLE a RESTRICT") {
        Statement::DropTable(d) => d,
        other => panic!("{other:?}"),
    };
    assert!(!dropped.if_exists);
    assert!(!dropped.cascade);
}

#[test]
fn create_and_drop_index() {
    let created = match stmt(
        "CREATE UNIQUE INDEX IF NOT EXISTS users_email_idx ON public.users (email, age)",
    ) {
        Statement::CreateIndex(c) => c,
        other => panic!("{other:?}"),
    };
    assert!(created.unique);
    assert!(created.if_not_exists);
    assert_eq!(created.name, "users_email_idx");
    assert_eq!(created.table.split("public"), ("public", "users"));
    assert_eq!(
        created.columns,
        vec!["email".to_string(), "age".to_string()]
    );

    let created = match stmt("CREATE INDEX i ON t (a)") {
        Statement::CreateIndex(c) => c,
        other => panic!("{other:?}"),
    };
    assert!(!created.unique);
    assert!(!created.if_not_exists);

    let dropped = match stmt("DROP INDEX IF EXISTS users_email_idx") {
        Statement::DropIndex(d) => d,
        other => panic!("{other:?}"),
    };
    assert!(dropped.if_exists);
    assert_eq!(dropped.name, "users_email_idx");

    rejects("CREATE INDEX i ON t");
    rejects("CREATE INDEX i ON t ()");
    rejects("CREATE INDEX ON t (a)");
    rejects("CREATE OR REPLACE INDEX i ON t (a)");
    rejects("DROP INDEX");
}

#[test]
fn select_projection_forms() {
    assert_eq!(
        select("SELECT * FROM t").projection,
        vec![SelectItem::Wildcard]
    );
    assert_eq!(
        select("SELECT t.* FROM t").projection,
        vec![SelectItem::QualifiedWildcard(ObjectName(vec!["t".into()]))]
    );
    let s = select("SELECT a, b AS bee, c c2 FROM t");
    assert_eq!(s.projection.len(), 3);
    match &s.projection[1] {
        SelectItem::Expr { alias, .. } => assert_eq!(alias.as_deref(), Some("bee")),
        other => panic!("{other:?}"),
    }
    match &s.projection[2] {
        SelectItem::Expr { alias, .. } => assert_eq!(alias.as_deref(), Some("c2")),
        other => panic!("{other:?}"),
    }
    assert!(select("SELECT DISTINCT a FROM t").distinct);
    assert!(select("SELECT 1").from.is_empty());
}

#[test]
fn select_full_shape() {
    let q = query(
        "SELECT u.id, count(*) AS n FROM users u \
         WHERE u.active = true GROUP BY u.id HAVING count(*) > 1 \
         ORDER BY n DESC NULLS LAST, u.id LIMIT 10 OFFSET 5",
    );
    let s = match &q.body {
        SetExpr::Select(s) => s.as_ref().clone(),
        other => panic!("{other:?}"),
    };
    assert_eq!(s.group_by.len(), 1);
    assert!(s.having.is_some());
    assert!(s.selection.is_some());
    assert_eq!(q.order_by.len(), 2);
    assert!(!q.order_by[0].asc);
    assert_eq!(q.order_by[0].nulls_first, Some(false));
    assert!(q.order_by[1].asc);
    assert_eq!(q.limit, Some(Expr::Literal(Literal::Int(10))));
    assert_eq!(q.offset, Some(Expr::Literal(Literal::Int(5))));
}

#[test]
fn having_requires_group_by() {
    rejects("SELECT a FROM t HAVING count(*) > 1");
}

#[test]
fn joins() {
    let s = select(
        "SELECT * FROM a \
         JOIN b ON a.id = b.a_id \
         INNER JOIN c ON c.id = a.c_id \
         LEFT OUTER JOIN d ON d.id = a.d_id \
         RIGHT JOIN e ON e.id = a.e_id \
         FULL OUTER JOIN f ON f.id = a.f_id \
         CROSS JOIN g \
         JOIN h USING (id)",
    );
    let kinds: Vec<JoinType> = s.from[0].joins.iter().map(|j| j.join_type).collect();
    assert_eq!(
        kinds,
        vec![
            JoinType::Inner,
            JoinType::Inner,
            JoinType::Left,
            JoinType::Right,
            JoinType::Full,
            JoinType::Cross,
            JoinType::Inner,
        ]
    );
    assert_eq!(
        s.from[0].joins[6].constraint,
        JoinConstraint::Using(vec!["id".into()])
    );
    assert_eq!(s.from[0].joins[5].constraint, JoinConstraint::None);
}

#[test]
fn join_condition_rules() {
    rejects("SELECT * FROM a JOIN b");
    rejects("SELECT * FROM a LEFT JOIN b");
    rejects("SELECT * FROM a CROSS JOIN b ON a.id = b.id");
}

#[test]
fn comma_joins_and_derived_tables() {
    let s = select("SELECT * FROM a, b");
    assert_eq!(s.from.len(), 2);

    let s = select("SELECT x.n FROM (SELECT count(*) AS n FROM t) AS x");
    match &s.from[0].relation {
        TableFactor::Derived { alias, .. } => assert_eq!(alias, "x"),
        other => panic!("{other:?}"),
    }
    rejects("SELECT * FROM (SELECT 1)");
}

#[test]
fn set_operations() {
    let q = query("SELECT a FROM t UNION ALL SELECT a FROM u EXCEPT SELECT a FROM v");
    match q.body {
        SetExpr::SetOp { op, left, .. } => {
            assert_eq!(op, SetOp::Except);
            match *left {
                SetExpr::SetOp { op, all, .. } => {
                    assert_eq!(op, SetOp::Union);
                    assert!(all);
                }
                other => panic!("{other:?}"),
            }
        }
        other => panic!("{other:?}"),
    }
    assert!(matches!(
        query("SELECT 1 INTERSECT SELECT 1").body,
        SetExpr::SetOp {
            op: SetOp::Intersect,
            ..
        }
    ));
}

#[test]
fn common_table_expressions() {
    let q = query(
        "WITH recent (id) AS (SELECT id FROM events WHERE at > 0), \
         other AS (SELECT 1) \
         SELECT * FROM recent",
    );
    assert_eq!(q.with.len(), 2);
    assert_eq!(q.with[0].name, "recent");
    assert_eq!(q.with[0].columns, vec!["id".to_string()]);
    assert!(q.with[1].columns.is_empty());
}

#[test]
fn insert_values() {
    let ins = match stmt("INSERT INTO t (a, b) VALUES (1, 'x'), (2, 'y') RETURNING a") {
        Statement::Insert(i) => i,
        other => panic!("{other:?}"),
    };
    assert_eq!(ins.columns, vec!["a".to_string(), "b".to_string()]);
    match ins.source {
        InsertSource::Values(rows) => {
            assert_eq!(rows.len(), 2);
            assert_eq!(rows[0][0], Expr::Literal(Literal::Int(1)));
            assert_eq!(rows[1][1], Expr::Literal(Literal::String("y".into())));
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(ins.returning.len(), 1);
}

#[test]
fn insert_from_select_and_arity_check() {
    let ins = match stmt("INSERT INTO t SELECT a, b FROM u") {
        Statement::Insert(i) => i,
        other => panic!("{other:?}"),
    };
    assert!(ins.columns.is_empty());
    assert!(matches!(ins.source, InsertSource::Query(_)));
    assert!(matches!(
        stmt("INSERT INTO t (a) (SELECT 1)"),
        Statement::Insert(_)
    ));
    rejects("INSERT INTO t (a, b) VALUES (1)");
}

#[test]
fn update_and_delete() {
    let upd = match stmt("UPDATE t SET a = 1, b = a + 1 WHERE id = $1 RETURNING *") {
        Statement::Update(u) => u,
        other => panic!("{other:?}"),
    };
    assert_eq!(upd.assignments.len(), 2);
    assert_eq!(upd.assignments[0].column, "a");
    assert!(upd.selection.is_some());
    assert_eq!(upd.returning, vec![SelectItem::Wildcard]);

    let del = match stmt("DELETE FROM t AS x WHERE x.a IS NULL") {
        Statement::Delete(d) => d,
        other => panic!("{other:?}"),
    };
    assert_eq!(del.alias.as_deref(), Some("x"));
    assert!(del.selection.is_some());
    assert!(matches!(stmt("DELETE FROM t"), Statement::Delete(_)));
}

fn where_of(sql: &str) -> Expr {
    select(sql).selection.expect("a WHERE clause")
}

#[test]
fn operator_precedence() {
    // a OR b AND c  =>  a OR (b AND c)
    let e = where_of("SELECT 1 FROM t WHERE a OR b AND c");
    match e {
        Expr::BinaryOp { op, right, .. } => {
            assert_eq!(op, BinaryOp::Or);
            assert!(matches!(
                *right,
                Expr::BinaryOp {
                    op: BinaryOp::And,
                    ..
                }
            ));
        }
        other => panic!("{other:?}"),
    }

    // 1 + 2 * 3  =>  1 + (2 * 3)
    let e = where_of("SELECT 1 FROM t WHERE 1 + 2 * 3 = x");
    match e {
        Expr::BinaryOp {
            op: BinaryOp::Eq,
            left,
            ..
        } => match *left {
            Expr::BinaryOp { op, right, .. } => {
                assert_eq!(op, BinaryOp::Plus);
                assert!(matches!(
                    *right,
                    Expr::BinaryOp {
                        op: BinaryOp::Multiply,
                        ..
                    }
                ));
            }
            other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    }

    // -a * b  =>  (-a) * b
    let e = where_of("SELECT 1 FROM t WHERE -a * b = 0");
    match e {
        Expr::BinaryOp { left, .. } => match *left {
            Expr::BinaryOp { op, left, .. } => {
                assert_eq!(op, BinaryOp::Multiply);
                assert!(matches!(
                    *left,
                    Expr::UnaryOp {
                        op: UnaryOp::Minus,
                        ..
                    }
                ));
            }
            other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    }

    // Parentheses override precedence.
    let e = where_of("SELECT 1 FROM t WHERE (a OR b) AND c");
    assert!(matches!(
        e,
        Expr::BinaryOp {
            op: BinaryOp::And,
            ..
        }
    ));
}

#[test]
fn predicates() {
    assert!(matches!(
        where_of("SELECT 1 FROM t WHERE a IS NOT NULL"),
        Expr::IsNull { negated: true, .. }
    ));
    assert!(matches!(
        where_of("SELECT 1 FROM t WHERE a BETWEEN 1 AND 10"),
        Expr::Between { negated: false, .. }
    ));
    assert!(matches!(
        where_of("SELECT 1 FROM t WHERE a NOT BETWEEN 1 AND 10"),
        Expr::Between { negated: true, .. }
    ));
    assert!(matches!(
        where_of("SELECT 1 FROM t WHERE a IN (1, 2, 3)"),
        Expr::InList { negated: false, .. }
    ));
    assert!(matches!(
        where_of("SELECT 1 FROM t WHERE a NOT IN (SELECT b FROM u)"),
        Expr::InSubquery { negated: true, .. }
    ));
    assert!(matches!(
        where_of("SELECT 1 FROM t WHERE a LIKE 'x%'"),
        Expr::Like { negated: false, .. }
    ));
    assert!(matches!(
        where_of("SELECT 1 FROM t WHERE EXISTS (SELECT 1 FROM u WHERE u.t = t.id)"),
        Expr::Exists { .. }
    ));
    assert!(matches!(
        where_of("SELECT 1 FROM t WHERE NOT a"),
        Expr::UnaryOp {
            op: UnaryOp::Not,
            ..
        }
    ));
    assert!(matches!(
        where_of("SELECT 1 FROM t WHERE a = (SELECT max(b) FROM u)"),
        Expr::BinaryOp { .. }
    ));
}

#[test]
fn case_cast_and_functions() {
    let e = where_of("SELECT 1 FROM t WHERE CASE WHEN a > 0 THEN 1 ELSE 2 END = 1");
    match e {
        Expr::BinaryOp { left, .. } => match *left {
            Expr::Case {
                operand,
                branches,
                else_result,
            } => {
                assert!(operand.is_none());
                assert_eq!(branches.len(), 1);
                assert!(else_result.is_some());
            }
            other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    }
    assert!(matches!(
        where_of("SELECT 1 FROM t WHERE CASE a WHEN 1 THEN 'x' END = 'x'"),
        Expr::BinaryOp { .. }
    ));
    rejects("SELECT CASE END");

    let s = select("SELECT CAST(a AS BIGINT), count(*), count(DISTINCT b), lower(c) FROM t");
    match &s.projection[0] {
        SelectItem::Expr {
            expr: Expr::Cast { data_type, .. },
            ..
        } => assert_eq!(*data_type, DataType::Int8),
        other => panic!("{other:?}"),
    }
    match &s.projection[1] {
        SelectItem::Expr {
            expr: Expr::Function(f),
            ..
        } => {
            assert_eq!(f.name, "count");
            assert_eq!(f.args, FunctionArgs::Wildcard);
            assert!(is_aggregate(&f.name));
        }
        other => panic!("{other:?}"),
    }
    match &s.projection[2] {
        SelectItem::Expr {
            expr: Expr::Function(f),
            ..
        } => assert!(f.distinct),
        other => panic!("{other:?}"),
    }
    match &s.projection[3] {
        SelectItem::Expr {
            expr: Expr::Function(f),
            ..
        } => assert!(!is_aggregate(&f.name)),
        other => panic!("{other:?}"),
    }
}

#[test]
fn literals_identifiers_and_comments() {
    let s = select(
        "SELECT 1, -2, 3.5, 1e3, 'it''s', TRUE, FALSE, NULL, $2, \"Mixed Case\" \
         FROM /* block */ t -- trailing",
    );
    let exprs: Vec<&Expr> = s
        .projection
        .iter()
        .map(|item| match item {
            SelectItem::Expr { expr, .. } => expr,
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(*exprs[0], Expr::Literal(Literal::Int(1)));
    assert!(matches!(exprs[1], Expr::UnaryOp { .. }));
    assert_eq!(*exprs[2], Expr::Literal(Literal::Float(3.5)));
    assert_eq!(*exprs[3], Expr::Literal(Literal::Float(1000.0)));
    assert_eq!(*exprs[4], Expr::Literal(Literal::String("it's".into())));
    assert_eq!(*exprs[5], Expr::Literal(Literal::Boolean(true)));
    assert_eq!(*exprs[6], Expr::Literal(Literal::Boolean(false)));
    assert_eq!(*exprs[7], Expr::Literal(Literal::Null));
    assert_eq!(*exprs[8], Expr::Parameter(2));
    assert_eq!(
        *exprs[9],
        Expr::Column(ObjectName(vec!["Mixed Case".into()]))
    );
}

#[test]
fn unquoted_identifiers_fold_to_lower_case() {
    let s = select("SELECT Id FROM Users");
    assert_eq!(
        s.projection[0],
        SelectItem::Expr {
            expr: Expr::Column(ObjectName(vec!["id".into()])),
            alias: None
        }
    );
    match &s.from[0].relation {
        TableFactor::Table { name, .. } => assert_eq!(name.base(), "users"),
        other => panic!("{other:?}"),
    }
    assert_eq!(stmt("select 1"), stmt("SELECT 1"));
}

#[test]
fn non_reserved_keywords_usable_as_identifiers() {
    for sql in [
        "SELECT text FROM t",
        "SELECT key, row FROM t",
        "CREATE TABLE t (text TEXT, key INT)",
        "SELECT a FROM t AS json",
    ] {
        stmt(sql);
    }
    // ...but reserved words still need quoting.
    rejects("SELECT select FROM t");
    stmt("SELECT \"select\" FROM t");
}

#[test]
fn create_procedure_with_control_flow() {
    let proc = match stmt(
        "CREATE OR REPLACE PROCEDURE public.guard(caller UUID, target BIGINT) AS BEGIN \
             IF caller <> target THEN \
                 RAISE 'not your row'; \
             ELSIF target IS NULL THEN \
                 RETURN; \
             ELSE \
                 UPDATE rows SET seen = true WHERE id = target; \
                 RETURN 1; \
             END IF; \
         END",
    ) {
        Statement::CreateProcedure(p) => p,
        other => panic!("{other:?}"),
    };
    assert!(proc.or_replace);
    assert_eq!(proc.name.split("public"), ("public", "guard"));
    assert_eq!(proc.params.len(), 2);
    assert_eq!(proc.params[1].data_type, DataType::Int8);
    assert_eq!(proc.body.len(), 1);
    match &proc.body[0] {
        ProcStatement::If {
            branches,
            else_branch,
        } => {
            assert_eq!(branches.len(), 2);
            assert!(matches!(branches[0].1[0], ProcStatement::Raise(_)));
            let els = else_branch.as_ref().expect("an else branch");
            assert_eq!(els.len(), 2);
            assert!(matches!(els[0], ProcStatement::Sql(_)));
            assert!(matches!(els[1], ProcStatement::Return(Some(_))));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn procedure_body_rules() {
    assert!(matches!(
        stmt("CREATE PROCEDURE p() BEGIN RETURN; END"),
        Statement::CreateProcedure(_)
    ));
    rejects("CREATE PROCEDURE p() BEGIN END");
    rejects("CREATE PROCEDURE p() BEGIN RETURN END");
    rejects("CREATE PROCEDURE p() BEGIN RETURN;");
    rejects("CREATE PROCEDURE p(x) BEGIN RETURN; END");
}

#[test]
fn call_and_drop_procedure() {
    let call = match stmt("CALL guard($1, 2)") {
        Statement::Call(c) => c,
        other => panic!("{other:?}"),
    };
    assert_eq!(call.args.len(), 2);
    assert!(matches!(stmt("CALL p()"), Statement::Call(_)));

    let dropped = match stmt("DROP PROCEDURE IF EXISTS public.guard") {
        Statement::DropProcedure(d) => d,
        other => panic!("{other:?}"),
    };
    assert!(dropped.if_exists);
}

#[test]
fn create_and_drop_trigger() {
    let trg = match stmt(
        "CREATE TRIGGER audit AFTER INSERT OR UPDATE ON public.rows \
         FOR EACH ROW WHEN (new_active = true) EXECUTE PROCEDURE log_it('audit')",
    ) {
        Statement::CreateTrigger(t) => t,
        other => panic!("{other:?}"),
    };
    assert_eq!(trg.name, "audit");
    assert_eq!(trg.timing, TriggerTiming::After);
    assert_eq!(trg.events, vec![TriggerEvent::Insert, TriggerEvent::Update]);
    assert!(trg.for_each_row);
    assert!(trg.condition.is_some());
    assert_eq!(trg.procedure.base(), "log_it");
    assert_eq!(trg.args.len(), 1);

    let trg =
        match stmt("CREATE TRIGGER t BEFORE DELETE ON r FOR EACH STATEMENT EXECUTE FUNCTION f()") {
            Statement::CreateTrigger(t) => t,
            other => panic!("{other:?}"),
        };
    assert_eq!(trg.timing, TriggerTiming::Before);
    assert!(!trg.for_each_row);

    let dropped = match stmt("DROP TRIGGER IF EXISTS audit ON public.rows") {
        Statement::DropTrigger(d) => d,
        other => panic!("{other:?}"),
    };
    assert!(dropped.if_exists);
    assert_eq!(dropped.name, "audit");

    rejects("CREATE TRIGGER t AFTER INSERT OR INSERT ON r EXECUTE PROCEDURE f()");
    rejects("CREATE TRIGGER t DURING INSERT ON r EXECUTE PROCEDURE f()");
}

#[test]
fn malformed_input_is_rejected_cleanly() {
    for sql in [
        "SELECT",
        "SELECT FROM",
        "SELECT * FROM",
        "SELECT * FROM t WHERE",
        "SELECT * FROM t WHERE a =",
        "SELECT 'unterminated",
        "SELECT \"unterminated",
        "SELECT \"\" FROM t",
        "/* unterminated",
        "SELECT 1 SELECT 2",
        "CREATE TABLE t ()",
        "CREATE TABLE t (a)",
        "CREATE TABLE t (a NOTATYPE)",
        "CREATE TABLE (a INT)",
        "DROP",
        "DROP TABLE",
        "INSERT INTO",
        "INSERT INTO t VALUES",
        "INSERT INTO t VALUES ()",
        "UPDATE t SET",
        "UPDATE t SET a",
        "UPDATE SET a = 1",
        "DELETE t",
        "SELECT (1",
        "SELECT 1)",
        "SELECT a.b.c.d FROM t",
        "SELECT $ FROM t",
        "SELECT $99999999999999999999 FROM t",
        "SELECT 1e999 FROM t",
        "SELECT 99999999999999999999999 FROM t",
        "SELECT 1abc FROM t",
        "SELECT # FROM t",
        "SELECT a | b FROM t",
        "SELECT a ! b FROM t",
        "CALL p(",
        "CALL p",
        "\u{0}",
        "SELECT \u{1f980}",
    ] {
        rejects(sql);
    }
}

#[test]
fn errors_carry_an_offset() {
    let err = parse("SELECT * FROM t WHERE").unwrap_err();
    assert!(err.offset > 0, "{err}");
    assert!(err.to_string().contains("offset"));

    let ferrite: ferrite_common::FerriteError = err.into();
    assert!(matches!(ferrite, ferrite_common::FerriteError::Parse(_)));
}

#[test]
fn deep_nesting_is_rejected_rather_than_overflowing_the_stack() {
    let deep = format!("SELECT {}1{}", "(".repeat(5000), ")".repeat(5000));
    let err = parse(&deep).unwrap_err();
    assert!(err.message.contains("too deep"), "{err}");

    let deep = format!("SELECT {}1", "NOT ".repeat(5000));
    assert!(parse(&deep).is_err());

    let deep = format!("{}SELECT 1{}", "(".repeat(5000), ")".repeat(5000));
    assert!(parse(&deep).is_err());
}

/// A long run of `AND`/`OR` is a shape applications really send, so it
/// has to keep parsing — and the tree it produces has to stay shallow,
/// because a chain one level per term is what everything downstream then
/// walks recursively.
#[test]
fn long_flat_input_still_parses_and_stays_shallow() {
    let terms: Vec<String> = (0..2000).map(|i| format!("a = {i}")).collect();
    let sql = format!("SELECT 1 FROM t WHERE {}", terms.join(" OR "));
    let statements = parse(&sql).expect("2000 OR terms must parse");

    fn height(expr: &Expr) -> usize {
        match expr {
            Expr::BinaryOp { left, right, .. } => 1 + height(left).max(height(right)),
            _ => 1,
        }
    }
    let Statement::Query(query) = &statements[0] else {
        panic!("expected a query");
    };
    let SetExpr::Select(select) = &query.body else {
        panic!("expected a select");
    };
    let predicate = select.selection.as_ref().expect("a WHERE clause");
    // 2000 terms balanced is eleven levels plus the comparison; chained it
    // would be two thousand.
    assert!(
        height(predicate) <= 16,
        "2000 OR terms produced a tree {} levels tall",
        height(predicate)
    );
}

#[test]
fn insert_or_ignore_becomes_on_conflict_do_nothing() {
    let Statement::Insert(insert) =
        parse_statement("INSERT OR IGNORE INTO t (a, b) VALUES (1, 2)").unwrap()
    else {
        panic!("expected an INSERT");
    };
    let clause = insert
        .on_conflict
        .expect("OR IGNORE sets a conflict clause");
    assert!(clause.target.is_empty(), "the target is inferred later");
    assert_eq!(clause.action, InsertConflictAction::Nothing);
}

#[test]
fn insert_or_replace_becomes_do_update_over_the_named_columns() {
    let Statement::Insert(insert) =
        parse_statement("INSERT OR REPLACE INTO t (a, b) VALUES (1, 2)").unwrap()
    else {
        panic!("expected an INSERT");
    };
    let InsertConflictAction::Update { assignments, .. } = insert.on_conflict.unwrap().action
    else {
        panic!("expected DO UPDATE");
    };
    let columns: Vec<&str> = assignments.iter().map(|a| a.column.as_str()).collect();
    assert_eq!(columns, ["a", "b"]);
    assert_eq!(
        assignments[0].value,
        Expr::Column(ObjectName(vec!["excluded".into(), "a".into()]))
    );
}

#[test]
fn insert_or_abort_carries_no_clause_because_ferrite_aborts_anyway() {
    for sql in [
        "INSERT OR ABORT INTO t (a) VALUES (1)",
        "INSERT OR FAIL INTO t (a) VALUES (1)",
        "INSERT OR ROLLBACK INTO t (a) VALUES (1)",
    ] {
        let Statement::Insert(insert) = parse_statement(sql).unwrap() else {
            panic!("expected an INSERT");
        };
        assert_eq!(insert.on_conflict, None, "{sql}");
    }
}

#[test]
fn on_conflict_takes_a_target_a_do_update_and_a_where() {
    let Statement::Insert(insert) = parse_statement(
        "INSERT INTO t (a, b) VALUES (1, 2) \
         ON CONFLICT (a, b) DO UPDATE SET b = excluded.b WHERE t.b < excluded.b",
    )
    .unwrap() else {
        panic!("expected an INSERT");
    };
    let clause = insert.on_conflict.unwrap();
    assert_eq!(clause.target, ["a", "b"]);
    let InsertConflictAction::Update {
        assignments,
        selection,
    } = clause.action
    else {
        panic!("expected DO UPDATE");
    };
    assert_eq!(assignments.len(), 1);
    assert!(selection.is_some());
}

#[test]
fn ilike_and_collate_parse_as_their_own_nodes() {
    let Statement::Query(query) =
        parse_statement("SELECT * FROM t WHERE a ILIKE 'x%' AND b = 'y' COLLATE NOCASE").unwrap()
    else {
        panic!("expected a query");
    };
    let rendered = format!("{query:?}");
    assert!(rendered.contains("case_insensitive: true"), "{rendered}");
    assert!(rendered.contains("Collate"), "{rendered}");
}

#[test]
fn a_column_level_collate_is_recorded_on_the_column() {
    let Statement::CreateTable(create) =
        parse_statement("CREATE TABLE t (username TEXT NOT NULL UNIQUE COLLATE NOCASE)").unwrap()
    else {
        panic!("expected a CREATE TABLE");
    };
    assert!(create.columns[0]
        .constraints
        .contains(&ColumnConstraint::Collate("nocase".into())));
    assert_eq!(create.unique_keys(), vec![vec!["username".to_string()]]);
}

#[test]
fn table_level_primary_key_and_unique_are_both_unique_keys() {
    let Statement::CreateTable(create) =
        parse_statement("CREATE TABLE t (a INT, b TEXT, c TEXT, PRIMARY KEY (a, b), UNIQUE (c))")
            .unwrap()
    else {
        panic!("expected a CREATE TABLE");
    };
    assert_eq!(
        create.unique_keys(),
        vec![
            vec!["a".to_string(), "b".to_string()],
            vec!["c".to_string()]
        ]
    );
}
