//! Parameter inference for the extended query flow.
//!
//! `Describe` on a prepared statement has to answer *before* any value is
//! bound, so the engine has to work the parameter types out of the
//! statement's shape. PostgreSQL does this during parse analysis with a
//! full type checker; Ferrite v1 does the two cases that carry almost all
//! real traffic — a placeholder written into a column, and a placeholder
//! compared against one — and leaves the rest for the protocol layer to
//! fall back to text on.
//!
//! Getting the *count* right matters more than getting the types right:
//! clients that prepare statements refuse to bind a different number of
//! parameters than the server described.

use ferrite_common::{Catalog, DataType, Schema};
use ferrite_sql::ast as sql;

use ferrite_planner::DEFAULT_NAMESPACE;

/// One entry per `$n` in the statement, in order. `None` means "could not
/// be inferred".
pub fn parameter_types(stmt: &sql::Statement, catalog: &dyn Catalog) -> Vec<Option<DataType>> {
    let mut types = vec![None; count(stmt)];
    infer(stmt, catalog, &mut types);
    types
}

/// `$n` is 1-based, so the highest `n` is the number of parameters even
/// when a statement skips one.
fn count(stmt: &sql::Statement) -> usize {
    let mut highest = 0;
    for_each_expr(stmt, &mut |expr| {
        if let sql::Expr::Parameter(n) = expr {
            highest = highest.max(*n);
        }
    });
    usize::try_from(highest).unwrap_or(0)
}

fn infer(stmt: &sql::Statement, catalog: &dyn Catalog, types: &mut [Option<DataType>]) {
    match stmt {
        sql::Statement::Insert(insert) => {
            let Some(schema) = schema_of(&insert.table, catalog) else {
                return;
            };
            let sql::InsertSource::Values(rows) = &insert.source else {
                return;
            };
            let positions: Vec<usize> = if insert.columns.is_empty() {
                (0..schema.columns.len()).collect()
            } else {
                insert
                    .columns
                    .iter()
                    .filter_map(|name| schema.column_index(name))
                    .collect()
            };
            for row in rows {
                for (position, expr) in positions.iter().zip(row) {
                    if let sql::Expr::Parameter(n) = expr {
                        set(types, *n, schema.columns[*position].data_type);
                    }
                }
            }
        }
        sql::Statement::Update(update) => {
            let Some(schema) = schema_of(&update.table, catalog) else {
                return;
            };
            for assignment in &update.assignments {
                if let (sql::Expr::Parameter(n), Some(position)) =
                    (&assignment.value, schema.column_index(&assignment.column))
                {
                    set(types, *n, schema.columns[position].data_type);
                }
            }
            if let Some(selection) = &update.selection {
                from_predicate(selection, &schema, types);
            }
        }
        sql::Statement::Delete(delete) => {
            let Some(schema) = schema_of(&delete.table, catalog) else {
                return;
            };
            if let Some(selection) = &delete.selection {
                from_predicate(selection, &schema, types);
            }
        }
        sql::Statement::Query(query) => {
            let sql::SetExpr::Select(select) = &query.body else {
                return;
            };
            let Some(sql::TableWithJoins {
                relation: sql::TableFactor::Table { name, .. },
                ..
            }) = select.from.first()
            else {
                return;
            };
            let Some(schema) = schema_of(name, catalog) else {
                return;
            };
            if let Some(selection) = &select.selection {
                from_predicate(selection, &schema, types);
            }
        }
        _ => {}
    }
}

/// `column <op> $n` in either operand order, through `AND`/`OR`, `BETWEEN`
/// and `IN`.
fn from_predicate(expr: &sql::Expr, schema: &Schema, types: &mut [Option<DataType>]) {
    match expr {
        sql::Expr::BinaryOp { left, op, right } => {
            if matches!(op, sql::BinaryOp::And | sql::BinaryOp::Or) {
                from_predicate(left, schema, types);
                from_predicate(right, schema, types);
                return;
            }
            match (left.as_ref(), right.as_ref()) {
                (sql::Expr::Column(name), sql::Expr::Parameter(n))
                | (sql::Expr::Parameter(n), sql::Expr::Column(name)) => {
                    if let Some(position) = schema.column_index(name.base()) {
                        set(types, *n, schema.columns[position].data_type);
                    }
                }
                _ => {}
            }
        }
        sql::Expr::UnaryOp { expr, .. } | sql::Expr::IsNull { expr, .. } => {
            from_predicate(expr, schema, types)
        }
        sql::Expr::Between {
            expr, low, high, ..
        } => {
            if let sql::Expr::Column(name) = expr.as_ref() {
                if let Some(position) = schema.column_index(name.base()) {
                    let ty = schema.columns[position].data_type;
                    for bound in [low.as_ref(), high.as_ref()] {
                        if let sql::Expr::Parameter(n) = bound {
                            set(types, *n, ty);
                        }
                    }
                }
            }
        }
        sql::Expr::InList { expr, list, .. } => {
            if let sql::Expr::Column(name) = expr.as_ref() {
                if let Some(position) = schema.column_index(name.base()) {
                    let ty = schema.columns[position].data_type;
                    for item in list {
                        if let sql::Expr::Parameter(n) = item {
                            set(types, *n, ty);
                        }
                    }
                }
            }
        }
        _ => {}
    }
}

fn schema_of(name: &sql::ObjectName, catalog: &dyn Catalog) -> Option<Schema> {
    let (namespace, table) = name.split(DEFAULT_NAMESPACE);
    let id = catalog.table_id(namespace, table).ok().flatten()?;
    catalog.table_schema(id).ok()
}

fn set(types: &mut [Option<DataType>], n: u32, ty: DataType) {
    if let Some(slot) = usize::try_from(n)
        .ok()
        .and_then(|n| n.checked_sub(1))
        .and_then(|n| types.get_mut(n))
    {
        *slot = Some(ty);
    }
}

/// Visit every expression in a statement, including those nested in
/// subqueries, so the placeholder count is never short.
fn for_each_expr(stmt: &sql::Statement, visit: &mut impl FnMut(&sql::Expr)) {
    match stmt {
        sql::Statement::Query(query) => walk_query(query, visit),
        sql::Statement::Insert(insert) => {
            match &insert.source {
                sql::InsertSource::Values(rows) => {
                    for row in rows {
                        for expr in row {
                            walk_expr(expr, visit);
                        }
                    }
                }
                sql::InsertSource::Query(query) => walk_query(query, visit),
            }
            walk_items(&insert.returning, visit);
        }
        sql::Statement::Update(update) => {
            for assignment in &update.assignments {
                walk_expr(&assignment.value, visit);
            }
            if let Some(selection) = &update.selection {
                walk_expr(selection, visit);
            }
            walk_items(&update.returning, visit);
        }
        sql::Statement::Delete(delete) => {
            if let Some(selection) = &delete.selection {
                walk_expr(selection, visit);
            }
            walk_items(&delete.returning, visit);
        }
        sql::Statement::Call(call) => {
            for arg in &call.args {
                walk_expr(arg, visit);
            }
        }
        _ => {}
    }
}

fn walk_query(query: &sql::Query, visit: &mut impl FnMut(&sql::Expr)) {
    for cte in &query.with {
        walk_query(&cte.query, visit);
    }
    walk_set_expr(&query.body, visit);
    for item in &query.order_by {
        walk_expr(&item.expr, visit);
    }
    for expr in [&query.limit, &query.offset].into_iter().flatten() {
        walk_expr(expr, visit);
    }
}

fn walk_set_expr(body: &sql::SetExpr, visit: &mut impl FnMut(&sql::Expr)) {
    match body {
        sql::SetExpr::Select(select) => {
            walk_items(&select.projection, visit);
            for relation in &select.from {
                walk_table_factor(&relation.relation, visit);
                for join in &relation.joins {
                    walk_table_factor(&join.relation, visit);
                    if let sql::JoinConstraint::On(expr) = &join.constraint {
                        walk_expr(expr, visit);
                    }
                }
            }
            for expr in [&select.selection, &select.having].into_iter().flatten() {
                walk_expr(expr, visit);
            }
            for expr in &select.group_by {
                walk_expr(expr, visit);
            }
        }
        sql::SetExpr::Query(query) => walk_query(query, visit),
        sql::SetExpr::SetOp { left, right, .. } => {
            walk_set_expr(left, visit);
            walk_set_expr(right, visit);
        }
    }
}

fn walk_table_factor(factor: &sql::TableFactor, visit: &mut impl FnMut(&sql::Expr)) {
    if let sql::TableFactor::Derived { subquery, .. } = factor {
        walk_query(subquery, visit);
    }
}

fn walk_items(items: &[sql::SelectItem], visit: &mut impl FnMut(&sql::Expr)) {
    for item in items {
        if let sql::SelectItem::Expr { expr, .. } = item {
            walk_expr(expr, visit);
        }
    }
}

fn walk_expr(expr: &sql::Expr, visit: &mut impl FnMut(&sql::Expr)) {
    visit(expr);
    match expr {
        sql::Expr::Literal(_) | sql::Expr::Column(_) | sql::Expr::Parameter(_) => {}
        sql::Expr::UnaryOp { expr, .. }
        | sql::Expr::IsNull { expr, .. }
        | sql::Expr::Cast { expr, .. } => walk_expr(expr, visit),
        sql::Expr::BinaryOp { left, right, .. } => {
            walk_expr(left, visit);
            walk_expr(right, visit);
        }
        sql::Expr::Between {
            expr, low, high, ..
        } => {
            walk_expr(expr, visit);
            walk_expr(low, visit);
            walk_expr(high, visit);
        }
        sql::Expr::InList { expr, list, .. } => {
            walk_expr(expr, visit);
            for item in list {
                walk_expr(item, visit);
            }
        }
        sql::Expr::InSubquery { expr, subquery, .. } => {
            walk_expr(expr, visit);
            walk_query(subquery, visit);
        }
        sql::Expr::Exists { subquery, .. } | sql::Expr::Subquery(subquery) => {
            walk_query(subquery, visit)
        }
        sql::Expr::Like { expr, pattern, .. } => {
            walk_expr(expr, visit);
            walk_expr(pattern, visit);
        }
        sql::Expr::Case {
            operand,
            branches,
            else_result,
        } => {
            for expr in [operand, else_result].into_iter().flatten() {
                walk_expr(expr, visit);
            }
            for (when, then) in branches {
                walk_expr(when, visit);
                walk_expr(then, visit);
            }
        }
        sql::Expr::Function(call) => {
            if let sql::FunctionArgs::List(args) = &call.args {
                for arg in args {
                    walk_expr(arg, visit);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrite_common::{ColumnDef, FerriteError, TableId};

    struct Users;

    impl Catalog for Users {
        fn table_id(&self, _ns: &str, name: &str) -> Result<Option<TableId>, FerriteError> {
            Ok((name == "users").then_some(16))
        }
        fn table_schema(&self, _table: TableId) -> Result<Schema, FerriteError> {
            Ok(Schema {
                columns: vec![
                    ColumnDef {
                        name: "id".into(),
                        data_type: DataType::Int8,
                        nullable: false,
                    },
                    ColumnDef {
                        name: "name".into(),
                        data_type: DataType::Text,
                        nullable: true,
                    },
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

    fn types_of(sql: &str) -> Vec<Option<DataType>> {
        parameter_types(&ferrite_sql::parse_statement(sql).unwrap(), &Users)
    }

    #[test]
    fn a_placeholder_written_into_a_column_takes_that_column_type() {
        assert_eq!(
            types_of("INSERT INTO users (id, name) VALUES ($1, $2)"),
            vec![Some(DataType::Int8), Some(DataType::Text)]
        );
        assert_eq!(
            types_of("UPDATE users SET name = $1 WHERE id = $2"),
            vec![Some(DataType::Text), Some(DataType::Int8)]
        );
    }

    #[test]
    fn a_placeholder_compared_against_a_column_takes_that_column_type() {
        assert_eq!(
            types_of("SELECT * FROM users WHERE id = $1 AND name <> $2"),
            vec![Some(DataType::Int8), Some(DataType::Text)]
        );
        assert_eq!(
            types_of("DELETE FROM users WHERE id BETWEEN $1 AND $2"),
            vec![Some(DataType::Int8), Some(DataType::Int8)]
        );
    }

    #[test]
    fn the_count_is_right_even_where_the_type_is_not_inferred() {
        // Neither side names a column, so nothing pins a type — but the
        // client still binds two parameters.
        assert_eq!(types_of("SELECT * FROM users WHERE $1 = $2"), vec![None; 2]);
        assert!(types_of("SELECT * FROM users").is_empty());
    }
}
