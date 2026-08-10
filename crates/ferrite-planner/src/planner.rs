//! Rule-based planner: `ferrite-sql` AST -> logical plan -> physical plan.

use ferrite_common::{
    Catalog, ColumnDef, DataType, FerriteError, IndexCatalog, IndexDef, Schema, Value,
};
use ferrite_sql::ast as sql;

use crate::expr::{BinaryOp, Expr};
use crate::logical::{split_conjunction, LogicalPlan, ProjectionItem, TableSource};
use crate::lower::{coerce, single_relation, single_select, Lowerer};
use crate::physical::{bind, PhysExpr, PhysicalPlan};
use crate::rules::optimize;

/// Schema names resolve here when a statement writes an unqualified table
/// name, matching PostgreSQL's default search path.
pub const DEFAULT_NAMESPACE: &str = "public";

/// Turns statements into executable plans. Holds no state beyond its
/// borrowed metadata sources and bound parameters, so one planner can be
/// built per statement for the cost of two pointer copies.
pub struct Planner<'a> {
    catalog: &'a dyn Catalog,
    indexes: &'a dyn IndexCatalog,
    params: &'a [Value],
}

impl<'a> Planner<'a> {
    pub fn new(catalog: &'a dyn Catalog, indexes: &'a dyn IndexCatalog) -> Self {
        Self {
            catalog,
            indexes,
            params: &[],
        }
    }

    /// Bind the values of the `$n` placeholders in the statement. They are
    /// substituted as literals while lowering, so the rest of the pipeline
    /// never sees a placeholder.
    pub fn with_params(mut self, params: &'a [Value]) -> Self {
        self.params = params;
        self
    }

    /// Full pipeline: build, optimize, lower.
    pub fn plan(&self, stmt: &sql::Statement) -> Result<PhysicalPlan, FerriteError> {
        let logical = self.build_logical(stmt)?;
        self.to_physical(optimize(logical))
    }

    /// Direct, unoptimized translation of the AST. Kept public so the
    /// pushdown rule can be tested against the shape it actually receives.
    ///
    /// This is the only method that knows the AST type. Everything
    /// `ferrite-sql` can parse but `ferrite-exec` cannot run — joins,
    /// aggregates, `ORDER BY`, DDL, transaction control — leaves here as a
    /// [`FerriteError::Plan`]; see [`crate::lower`].
    pub fn build_logical(&self, stmt: &sql::Statement) -> Result<LogicalPlan, FerriteError> {
        match stmt {
            sql::Statement::Query(query) => self.build_select(query),
            sql::Statement::Insert(insert) => self.build_insert(insert),
            sql::Statement::Update(update) => self.build_update(update),
            sql::Statement::Delete(delete) => self.build_delete(delete),
            sql::Statement::Call(call) => self.build_call(call),

            // DDL and transaction control are not plan nodes: they are
            // catalog and storage operations the caller drives directly,
            // and the v1 executor has no node set for them.
            sql::Statement::CreateTable(_)
            | sql::Statement::DropTable(_)
            | sql::Statement::CreateIndex(_)
            | sql::Statement::DropIndex(_) => Err(FerriteError::Plan(
                "DDL does not go through the planner; call the catalog directly".into(),
            )),
            sql::Statement::Begin | sql::Statement::Commit | sql::Statement::Rollback => Err(
                FerriteError::Plan("transaction control does not go through the planner".into()),
            ),
            sql::Statement::CreateProcedure(_)
            | sql::Statement::DropProcedure(_)
            | sql::Statement::CreateTrigger(_)
            | sql::Statement::DropTrigger(_) => Err(FerriteError::Plan(
                "procedures and triggers are native Rust closures registered in \
                 ferrite-proc at startup; there is no procedural language in v1"
                    .into(),
            )),
        }
    }

    fn build_select(&self, query: &sql::Query) -> Result<LogicalPlan, FerriteError> {
        let (select, limit) = single_select(query)?;
        let (name, qualifiers) = single_relation(&select.from)?;
        let lowerer = Lowerer::new(self.params).with_qualifiers(qualifiers.clone());

        let source = self.resolve(name)?;
        let schema = source.schema.clone();

        let mut plan = LogicalPlan::Scan {
            source,
            filter: None,
        };

        if let Some(predicate) = &select.selection {
            plan = LogicalPlan::Filter {
                input: Box::new(plan),
                predicate: lowerer.expr(predicate)?,
            };
        }

        if let Some(items) = projection_items(&select.projection, &qualifiers, &lowerer)? {
            let items = items
                .into_iter()
                .map(|item| resolve_output_name(item, &schema))
                .collect();
            plan = LogicalPlan::Projection {
                input: Box::new(plan),
                items,
            };
        }

        if let Some(count) = limit {
            plan = LogicalPlan::Limit {
                input: Box::new(plan),
                count,
            };
        }

        Ok(plan)
    }

    fn build_insert(&self, stmt: &sql::Insert) -> Result<LogicalPlan, FerriteError> {
        if !stmt.returning.is_empty() {
            return Err(FerriteError::Plan(
                "RETURNING is not supported by the v1 planner".into(),
            ));
        }
        let sql::InsertSource::Values(values) = &stmt.source else {
            return Err(FerriteError::Plan(
                "INSERT ... SELECT is not supported by the v1 planner".into(),
            ));
        };

        let source = self.resolve(&stmt.table)?;
        let width = source.schema.columns.len();
        let lowerer = Lowerer::new(self.params);

        let targets: Vec<usize> = if stmt.columns.is_empty() {
            (0..width).collect()
        } else {
            stmt.columns
                .iter()
                .map(|name| {
                    source
                        .schema
                        .column_index(name)
                        .ok_or_else(|| FerriteError::ColumnNotFound(name.clone()))
                })
                .collect::<Result<_, _>>()?
        };

        let mut rows = Vec::with_capacity(values.len());
        for exprs in values {
            if exprs.len() != targets.len() {
                return Err(FerriteError::Plan(format!(
                    "INSERT into {} expects {} values per row, got {}",
                    source.name,
                    targets.len(),
                    exprs.len()
                )));
            }
            let mut row = vec![Expr::Literal(Value::Null); width];
            for (position, expr) in targets.iter().zip(exprs) {
                row[*position] = coerce(
                    lowerer.expr(expr)?,
                    source.schema.columns[*position].data_type,
                )?;
            }
            rows.push(row);
        }

        Ok(LogicalPlan::Insert { source, rows })
    }

    fn build_update(&self, stmt: &sql::Update) -> Result<LogicalPlan, FerriteError> {
        if !stmt.returning.is_empty() {
            return Err(FerriteError::Plan(
                "RETURNING is not supported by the v1 planner".into(),
            ));
        }
        let source = self.resolve(&stmt.table)?;
        let mut qualifiers = vec![stmt.table.base().to_string()];
        qualifiers.extend(stmt.alias.clone());
        let lowerer = Lowerer::new(self.params).with_qualifiers(qualifiers);

        let mut assignments = Vec::with_capacity(stmt.assignments.len());
        for sql::Assignment { column, value } in &stmt.assignments {
            let position = source
                .schema
                .column_index(column)
                .ok_or_else(|| FerriteError::ColumnNotFound(column.clone()))?;
            let value = coerce(
                lowerer.expr(value)?,
                source.schema.columns[position].data_type,
            )?;
            assignments.push((position, value));
        }

        let input = self.filtered_scan(&source, stmt.selection.as_ref(), &lowerer)?;
        Ok(LogicalPlan::Update {
            source,
            input: Box::new(input),
            assignments,
        })
    }

    fn build_delete(&self, stmt: &sql::Delete) -> Result<LogicalPlan, FerriteError> {
        if !stmt.returning.is_empty() {
            return Err(FerriteError::Plan(
                "RETURNING is not supported by the v1 planner".into(),
            ));
        }
        let source = self.resolve(&stmt.table)?;
        let mut qualifiers = vec![stmt.table.base().to_string()];
        qualifiers.extend(stmt.alias.clone());
        let lowerer = Lowerer::new(self.params).with_qualifiers(qualifiers);

        let input = self.filtered_scan(&source, stmt.selection.as_ref(), &lowerer)?;
        Ok(LogicalPlan::Delete {
            source,
            input: Box::new(input),
        })
    }

    fn build_call(&self, stmt: &sql::Call) -> Result<LogicalPlan, FerriteError> {
        let lowerer = Lowerer::new(self.params);
        Ok(LogicalPlan::Call {
            name: stmt.name.base().to_string(),
            args: stmt
                .args
                .iter()
                .map(|arg| lowerer.expr(arg))
                .collect::<Result<_, _>>()?,
        })
    }

    fn filtered_scan(
        &self,
        source: &TableSource,
        selection: Option<&sql::Expr>,
        lowerer: &Lowerer<'_>,
    ) -> Result<LogicalPlan, FerriteError> {
        let scan = LogicalPlan::Scan {
            source: source.clone(),
            filter: None,
        };
        Ok(match selection {
            None => scan,
            Some(predicate) => LogicalPlan::Filter {
                input: Box::new(scan),
                predicate: lowerer.expr(predicate)?,
            },
        })
    }

    /// Lower an optimized logical plan, choosing an access path for every
    /// scan on the way down.
    pub fn to_physical(&self, plan: LogicalPlan) -> Result<PhysicalPlan, FerriteError> {
        match plan {
            LogicalPlan::Scan { source, filter } => self.access_path(&source, filter),

            LogicalPlan::Filter { input, predicate } => {
                let input = self.to_physical(*input)?;
                let schema = input.output_schema().cloned().ok_or_else(|| {
                    FerriteError::Plan("cannot filter a statement that produces no rows".into())
                })?;
                Ok(PhysicalPlan::Filter {
                    predicate: bind(&predicate, &schema)?,
                    input: Box::new(input),
                })
            }

            LogicalPlan::Projection { input, items } => {
                let input = self.to_physical(*input)?;
                let schema = input.output_schema().cloned().ok_or_else(|| {
                    FerriteError::Plan("cannot project a statement that produces no rows".into())
                })?;
                let mut exprs = Vec::with_capacity(items.len());
                let mut columns = Vec::with_capacity(items.len());
                for item in &items {
                    exprs.push(bind(&item.expr, &schema)?);
                    columns.push(output_column(item, &schema)?);
                }
                Ok(PhysicalPlan::Projection {
                    input: Box::new(input),
                    exprs,
                    output: Schema { columns },
                })
            }

            LogicalPlan::Limit { input, count } => Ok(PhysicalPlan::Limit {
                input: Box::new(self.to_physical(*input)?),
                count,
            }),

            LogicalPlan::Insert { source, rows } => {
                let empty = Schema {
                    columns: Vec::new(),
                };
                let rows = rows
                    .iter()
                    .map(|row| row.iter().map(|e| bind(e, &empty)).collect())
                    .collect::<Result<Vec<Vec<_>>, _>>()?;
                Ok(PhysicalPlan::Insert {
                    table: source.id,
                    table_name: source.name,
                    schema: source.schema,
                    rows,
                })
            }

            LogicalPlan::Update {
                source,
                input,
                assignments,
            } => {
                let scan = self.row_identity_source(*input, &source.name)?;
                let assignments = assignments
                    .iter()
                    .map(|(position, expr)| Ok((*position, bind(expr, &source.schema)?)))
                    .collect::<Result<Vec<_>, FerriteError>>()?;
                Ok(PhysicalPlan::Update {
                    table: source.id,
                    table_name: source.name,
                    schema: source.schema,
                    source: Box::new(scan),
                    assignments,
                })
            }

            LogicalPlan::Delete { source, input } => {
                let scan = self.row_identity_source(*input, &source.name)?;
                Ok(PhysicalPlan::Delete {
                    table: source.id,
                    table_name: source.name,
                    schema: source.schema,
                    source: Box::new(scan),
                })
            }

            LogicalPlan::Call { name, args } => {
                let empty = Schema {
                    columns: Vec::new(),
                };
                let args = args
                    .iter()
                    .map(|e| bind(e, &empty))
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(PhysicalPlan::CallProcedure { name, args })
            }
        }
    }

    fn row_identity_source(
        &self,
        input: LogicalPlan,
        table: &str,
    ) -> Result<PhysicalPlan, FerriteError> {
        let plan = self.to_physical(input)?;
        if !plan.preserves_row_identity() {
            return Err(FerriteError::Plan(format!(
                "UPDATE/DELETE on {table} needs a source that preserves row identity"
            )));
        }
        Ok(plan)
    }

    /// Index-vs-scan heuristic. No statistics and no cost model: the first
    /// conjunct of the form `indexed_column = <literal>` wins, everything
    /// else stays as a residual filter. Range predicates never select an
    /// index in v1 — an equality lookup is the only access path
    /// `ferrite-storage` is asked to provide beyond a full scan.
    fn access_path(
        &self,
        source: &TableSource,
        filter: Option<Expr>,
    ) -> Result<PhysicalPlan, FerriteError> {
        let mut conjuncts = filter.map(split_conjunction).unwrap_or_default();
        let indexes = self.indexes.indexes_for(source.id)?;

        let chosen = conjuncts.iter().enumerate().find_map(|(position, expr)| {
            let (column, key) = index_equality(expr, &source.schema)?;
            let index = pick_index(&indexes, &source.schema.columns[column].name)?;
            Some((position, index.clone(), column, key))
        });

        match chosen {
            Some((position, index, column, key)) => {
                conjuncts.remove(position);
                let residual = crate::logical::combine_conjunction(conjuncts)
                    .map(|e| bind(&e, &source.schema))
                    .transpose()?;
                Ok(PhysicalPlan::IndexScan {
                    table: source.id,
                    table_name: source.name.clone(),
                    schema: source.schema.clone(),
                    index: index.name,
                    column,
                    key: PhysExpr::Literal(key),
                    residual,
                })
            }
            None => {
                let filter = crate::logical::combine_conjunction(conjuncts)
                    .map(|e| bind(&e, &source.schema))
                    .transpose()?;
                Ok(PhysicalPlan::SeqScan {
                    table: source.id,
                    table_name: source.name.clone(),
                    schema: source.schema.clone(),
                    filter,
                })
            }
        }
    }

    fn resolve(&self, table: &sql::ObjectName) -> Result<TableSource, FerriteError> {
        let (namespace, name) = table.split(DEFAULT_NAMESPACE);
        let id = self
            .catalog
            .table_id(namespace, name)?
            .ok_or_else(|| FerriteError::TableNotFound(format!("{namespace}.{name}")))?;
        Ok(TableSource {
            id,
            name: name.to_string(),
            schema: self.catalog.table_schema(id)?,
        })
    }
}

/// A projection item before its output name has been decided; `None` there
/// means "name it after the column it reads".
struct RawItem {
    expr: Expr,
    alias: Option<String>,
}

/// `None` means "no projection node needed" (bare `SELECT *`).
fn projection_items(
    projection: &[sql::SelectItem],
    qualifiers: &[String],
    lowerer: &Lowerer<'_>,
) -> Result<Option<Vec<RawItem>>, FerriteError> {
    if projection.is_empty() {
        return Err(FerriteError::Plan("empty SELECT list".into()));
    }

    let mut wildcards = 0;
    for item in projection {
        match item {
            sql::SelectItem::Wildcard => wildcards += 1,
            sql::SelectItem::QualifiedWildcard(name) => {
                if !qualifiers.iter().any(|q| q == name.base()) {
                    return Err(FerriteError::Plan(format!(
                        "no relation named {} in this statement",
                        name.base()
                    )));
                }
                wildcards += 1;
            }
            sql::SelectItem::Expr { .. } => {}
        }
    }
    if wildcards == 1 && projection.len() == 1 {
        return Ok(None);
    }
    if wildcards > 0 {
        return Err(FerriteError::Plan(
            "mixing `*` with explicit select items is not supported in v1".into(),
        ));
    }

    let items = projection
        .iter()
        .map(|item| {
            let sql::SelectItem::Expr { expr, alias } = item else {
                unreachable!("wildcards were rejected above");
            };
            Ok(RawItem {
                expr: lowerer.expr(expr)?,
                alias: alias.clone(),
            })
        })
        .collect::<Result<_, FerriteError>>()?;
    Ok(Some(items))
}

fn resolve_output_name(item: RawItem, _schema: &Schema) -> ProjectionItem {
    let output_name = item.alias.unwrap_or_else(|| match &item.expr {
        Expr::Column(name) => name.clone(),
        _ => "?column?".to_string(),
    });
    ProjectionItem {
        expr: item.expr,
        output_name,
    }
}

/// Output column metadata for one projection item. v1 projects columns and
/// literals only; computed expressions would need a type inference pass
/// that does not exist yet.
fn output_column(item: &ProjectionItem, input: &Schema) -> Result<ColumnDef, FerriteError> {
    match &item.expr {
        Expr::Column(name) => {
            let position = input
                .column_index(name)
                .ok_or_else(|| FerriteError::ColumnNotFound(name.clone()))?;
            Ok(ColumnDef {
                name: item.output_name.clone(),
                ..input.columns[position].clone()
            })
        }
        Expr::Literal(value) => Ok(ColumnDef {
            name: item.output_name.clone(),
            data_type: value.data_type().unwrap_or(DataType::Text),
            nullable: true,
        }),
        other => Err(FerriteError::Plan(format!(
            "computed select expressions are not supported in v1: {other:?}"
        ))),
    }
}

/// Recognize `col = <literal>` (either operand order). `NULL` keys are
/// rejected: `col = NULL` is never true, so an index probe would be wrong.
///
/// The key is coerced to the indexed column's declared type; a probe
/// compares `Value` variants for exact equality, so an uncoerced key would
/// match nothing at all rather than fall back to a scan. A literal that
/// cannot be coerced yields `None`, leaving the conjunct as an ordinary
/// filter whose evaluation reports the type error.
fn index_equality(expr: &Expr, schema: &Schema) -> Option<(usize, Value)> {
    let Expr::Binary {
        left,
        op: BinaryOp::Eq,
        right,
    } = expr
    else {
        return None;
    };
    let (name, value) = match (left.as_ref(), right.as_ref()) {
        (Expr::Column(name), Expr::Literal(value)) => (name, value),
        (Expr::Literal(value), Expr::Column(name)) => (name, value),
        _ => return None,
    };
    if value.is_null() {
        return None;
    }
    let position = schema.column_index(name)?;
    match coerce(
        Expr::Literal(value.clone()),
        schema.columns[position].data_type,
    ) {
        Ok(Expr::Literal(key)) => Some((position, key)),
        _ => None,
    }
}

/// Single-column indexes only, and a unique one wins over a non-unique one
/// on the same column. A multi-column index is not considered: the
/// executor's `IndexProvider` probes with one key value, so using only the
/// leading column of a composite key would need a range probe that does not
/// exist in v1.
fn pick_index<'i>(indexes: &'i [IndexDef], column: &str) -> Option<&'i IndexDef> {
    indexes
        .iter()
        .filter(|i| i.columns.len() == 1 && i.columns[0] == column)
        .max_by_key(|i| i.unique)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrite_common::{IndexId, TableId};
    use std::collections::HashMap;

    struct TestCatalog {
        tables: HashMap<String, (TableId, Schema)>,
    }

    impl TestCatalog {
        fn users() -> Self {
            let schema = Schema {
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
                    ColumnDef {
                        name: "age".into(),
                        data_type: DataType::Int4,
                        nullable: true,
                    },
                ],
            };
            let mut tables = HashMap::new();
            tables.insert("public.users".to_string(), (1, schema));
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

        let PhysicalPlan::Limit { input, count } = plan else {
            panic!("expected a Limit at the root");
        };
        assert_eq!(count, 5);
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
    fn everything_outside_the_executable_subset_is_a_plan_error() {
        for sql in [
            "SELECT * FROM users JOIN users u2 ON u2.id = users.id",
            "SELECT * FROM users, users u2",
            "SELECT count(*) FROM users",
            "SELECT * FROM users GROUP BY id",
            "SELECT * FROM users ORDER BY id",
            "SELECT DISTINCT name FROM users",
            "SELECT * FROM users LIMIT 1 OFFSET 2",
            "SELECT * FROM users UNION SELECT * FROM users",
            "WITH x (id) AS (SELECT id FROM users) SELECT * FROM x",
            "SELECT * FROM (SELECT id FROM users) s",
            "SELECT * FROM users WHERE name LIKE 'a%'",
            "SELECT * FROM users WHERE id IN (SELECT id FROM users)",
            "SELECT age + 1 FROM users",
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
                        ColumnDef {
                            name: "id".into(),
                            data_type: DataType::Uuid,
                            nullable: false,
                        },
                        ColumnDef {
                            name: "at".into(),
                            data_type: DataType::Timestamp,
                            nullable: false,
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
}
