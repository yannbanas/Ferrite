//! Rule-based planner: AST -> logical plan -> physical plan.

use ferrite_common::{Catalog, ColumnDef, DataType, FerriteError, Schema, Value};

use crate::ast::{
    Assignment, BinaryOp, CallStmt, DeleteStmt, Expr, InsertStmt, SelectItem, SelectStmt,
    Statement, TableRef, UpdateStmt,
};
use crate::index::{IndexCatalog, IndexInfo};
use crate::logical::{split_conjunction, LogicalPlan, ProjectionItem, TableSource};
use crate::physical::{bind, PhysExpr, PhysicalPlan};
use crate::rules::optimize;

/// Turns statements into executable plans. Holds no state beyond its two
/// borrowed metadata sources, so one planner can serve every session.
pub struct Planner<'a> {
    catalog: &'a dyn Catalog,
    indexes: &'a dyn IndexCatalog,
}

impl<'a> Planner<'a> {
    pub fn new(catalog: &'a dyn Catalog, indexes: &'a dyn IndexCatalog) -> Self {
        Self { catalog, indexes }
    }

    /// Full pipeline: build, optimize, lower.
    pub fn plan(&self, stmt: &Statement) -> Result<PhysicalPlan, FerriteError> {
        let logical = self.build_logical(stmt)?;
        self.to_physical(optimize(logical))
    }

    /// Direct, unoptimized translation of the AST. Kept public so the
    /// pushdown rule can be tested against the shape it actually receives.
    ///
    /// This is the only method that knows the AST type; replacing the
    /// provisional [`crate::ast`] with `ferrite-sql`'s AST means rewriting
    /// this method and nothing else.
    pub fn build_logical(&self, stmt: &Statement) -> Result<LogicalPlan, FerriteError> {
        match stmt {
            Statement::Select(s) => self.build_select(s),
            Statement::Insert(s) => self.build_insert(s),
            Statement::Update(s) => self.build_update(s),
            Statement::Delete(s) => self.build_delete(s),
            Statement::Call(CallStmt { name, args }) => Ok(LogicalPlan::Call {
                name: name.clone(),
                args: args.clone(),
            }),
        }
    }

    fn build_select(&self, stmt: &SelectStmt) -> Result<LogicalPlan, FerriteError> {
        let source = self.resolve(&stmt.from)?;
        let schema = source.schema.clone();

        let mut plan = LogicalPlan::Scan {
            source,
            filter: None,
        };

        if let Some(predicate) = stmt.filter.clone() {
            plan = LogicalPlan::Filter {
                input: Box::new(plan),
                predicate,
            };
        }

        if let Some(items) = projection_items(&stmt.projection, &schema)? {
            plan = LogicalPlan::Projection {
                input: Box::new(plan),
                items,
            };
        }

        if let Some(count) = stmt.limit {
            plan = LogicalPlan::Limit {
                input: Box::new(plan),
                count,
            };
        }

        Ok(plan)
    }

    fn build_insert(&self, stmt: &InsertStmt) -> Result<LogicalPlan, FerriteError> {
        let source = self.resolve(&stmt.table)?;
        let width = source.schema.columns.len();

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

        let mut rows = Vec::with_capacity(stmt.rows.len());
        for values in &stmt.rows {
            if values.len() != targets.len() {
                return Err(FerriteError::Plan(format!(
                    "INSERT into {} expects {} values per row, got {}",
                    source.name,
                    targets.len(),
                    values.len()
                )));
            }
            let mut row = vec![Expr::Literal(Value::Null); width];
            for (position, value) in targets.iter().zip(values) {
                row[*position] = value.clone();
            }
            rows.push(row);
        }

        Ok(LogicalPlan::Insert { source, rows })
    }

    fn build_update(&self, stmt: &UpdateStmt) -> Result<LogicalPlan, FerriteError> {
        let source = self.resolve(&stmt.table)?;

        let mut assignments = Vec::with_capacity(stmt.assignments.len());
        for Assignment { column, value } in &stmt.assignments {
            let position = source
                .schema
                .column_index(column)
                .ok_or_else(|| FerriteError::ColumnNotFound(column.clone()))?;
            assignments.push((position, value.clone()));
        }

        let mut input = LogicalPlan::Scan {
            source: source.clone(),
            filter: None,
        };
        if let Some(predicate) = stmt.filter.clone() {
            input = LogicalPlan::Filter {
                input: Box::new(input),
                predicate,
            };
        }

        Ok(LogicalPlan::Update {
            source,
            input: Box::new(input),
            assignments,
        })
    }

    fn build_delete(&self, stmt: &DeleteStmt) -> Result<LogicalPlan, FerriteError> {
        let source = self.resolve(&stmt.table)?;

        let mut input = LogicalPlan::Scan {
            source: source.clone(),
            filter: None,
        };
        if let Some(predicate) = stmt.filter.clone() {
            input = LogicalPlan::Filter {
                input: Box::new(input),
                predicate,
            };
        }

        Ok(LogicalPlan::Delete {
            source,
            input: Box::new(input),
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
        let indexes = self.indexes.indexes(source.id);

        let chosen = conjuncts.iter().enumerate().find_map(|(position, expr)| {
            let (column, key) = index_equality(expr, &source.schema)?;
            let index = pick_index(&indexes, column)?;
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

    fn resolve(&self, table: &TableRef) -> Result<TableSource, FerriteError> {
        let schema_name = table.schema_or_default();
        let id = self
            .catalog
            .table_id(schema_name, &table.name)?
            .ok_or_else(|| FerriteError::TableNotFound(format!("{schema_name}.{}", table.name)))?;
        Ok(TableSource {
            id,
            name: table.name.clone(),
            schema: self.catalog.table_schema(id)?,
        })
    }
}

/// `None` means "no projection node needed" (bare `SELECT *`).
fn projection_items(
    projection: &[SelectItem],
    schema: &Schema,
) -> Result<Option<Vec<ProjectionItem>>, FerriteError> {
    if projection.is_empty() {
        return Err(FerriteError::Plan("empty SELECT list".into()));
    }
    let wildcards = projection
        .iter()
        .filter(|i| matches!(i, SelectItem::Wildcard))
        .count();
    if wildcards == projection.len() && wildcards == 1 {
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
            let SelectItem::Expr { expr, alias } = item else {
                unreachable!("wildcards were rejected above");
            };
            let output_name = alias.clone().unwrap_or_else(|| default_name(expr, schema));
            ProjectionItem {
                expr: expr.clone(),
                output_name,
            }
        })
        .collect();
    Ok(Some(items))
}

fn default_name(expr: &Expr, _schema: &Schema) -> String {
    match expr {
        Expr::Column(name) => name.clone(),
        _ => "?column?".to_string(),
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
    Some((schema.column_index(name)?, value.clone()))
}

/// Prefer a unique index over a non-unique one on the same column.
fn pick_index(indexes: &[IndexInfo], column: usize) -> Option<&IndexInfo> {
    indexes
        .iter()
        .filter(|i| i.column == column)
        .max_by_key(|i| i.unique)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::IndexInfo;
    use ferrite_common::TableId;
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

    struct TestIndexes(Vec<IndexInfo>);

    impl IndexCatalog for TestIndexes {
        fn indexes(&self, table: TableId) -> Vec<IndexInfo> {
            self.0
                .iter()
                .filter(|i| i.table == table)
                .cloned()
                .collect()
        }
    }

    fn id_index() -> TestIndexes {
        TestIndexes(vec![IndexInfo {
            name: "users_pkey".into(),
            table: 1,
            column: 0,
            unique: true,
        }])
    }

    fn select(filter: Option<Expr>) -> Statement {
        Statement::Select(SelectStmt {
            projection: vec![SelectItem::Wildcard],
            from: TableRef::new("users"),
            filter,
            limit: None,
        })
    }

    #[test]
    fn equality_on_an_indexed_column_selects_an_index_scan() {
        let catalog = TestCatalog::users();
        let indexes = id_index();
        let planner = Planner::new(&catalog, &indexes);

        let plan = planner
            .plan(&select(Some(Expr::eq(
                Expr::column("id"),
                Expr::Literal(Value::Int8(42)),
            ))))
            .unwrap();

        match plan {
            PhysicalPlan::IndexScan {
                index,
                column,
                key,
                residual,
                ..
            } => {
                assert_eq!(index, "users_pkey");
                assert_eq!(column, 0);
                assert_eq!(key, PhysExpr::Literal(Value::Int8(42)));
                assert!(residual.is_none());
            }
            other => panic!("expected an IndexScan, got {other:?}"),
        }
    }

    #[test]
    fn equality_on_an_unindexed_column_falls_back_to_a_seq_scan() {
        let catalog = TestCatalog::users();
        let indexes = id_index();
        let planner = Planner::new(&catalog, &indexes);

        let plan = planner
            .plan(&select(Some(Expr::eq(
                Expr::column("name"),
                Expr::Literal(Value::Text("ada".into())),
            ))))
            .unwrap();

        match plan {
            PhysicalPlan::SeqScan { filter, .. } => {
                assert_eq!(
                    filter,
                    Some(PhysExpr::binary(
                        PhysExpr::Column(1),
                        BinaryOp::Eq,
                        PhysExpr::Literal(Value::Text("ada".into()))
                    ))
                );
            }
            other => panic!("expected a SeqScan, got {other:?}"),
        }
    }

    #[test]
    fn a_range_predicate_never_selects_an_index() {
        let catalog = TestCatalog::users();
        let indexes = id_index();
        let planner = Planner::new(&catalog, &indexes);

        let plan = planner
            .plan(&select(Some(Expr::binary(
                Expr::column("id"),
                BinaryOp::Gt,
                Expr::Literal(Value::Int8(10)),
            ))))
            .unwrap();

        assert!(matches!(plan, PhysicalPlan::SeqScan { .. }));
    }

    #[test]
    fn no_index_catalog_entry_means_seq_scan() {
        let catalog = TestCatalog::users();
        let indexes = crate::index::NoIndexes;
        let planner = Planner::new(&catalog, &indexes);

        let plan = planner
            .plan(&select(Some(Expr::eq(
                Expr::column("id"),
                Expr::Literal(Value::Int8(42)),
            ))))
            .unwrap();

        assert!(matches!(plan, PhysicalPlan::SeqScan { .. }));
    }

    #[test]
    fn the_non_indexable_conjunct_survives_as_a_residual() {
        let catalog = TestCatalog::users();
        let indexes = id_index();
        let planner = Planner::new(&catalog, &indexes);

        let plan = planner
            .plan(&select(Some(Expr::and(
                Expr::eq(Expr::column("id"), Expr::Literal(Value::Int8(42))),
                Expr::binary(
                    Expr::column("age"),
                    BinaryOp::Gt,
                    Expr::Literal(Value::Int4(18)),
                ),
            ))))
            .unwrap();

        match plan {
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
        let catalog = TestCatalog::users();
        let indexes = id_index();
        let planner = Planner::new(&catalog, &indexes);

        let plan = planner
            .plan(&select(Some(Expr::eq(
                Expr::column("id"),
                Expr::Literal(Value::Null),
            ))))
            .unwrap();

        assert!(matches!(plan, PhysicalPlan::SeqScan { .. }));
    }

    #[test]
    fn pushdown_and_index_selection_compose() {
        let catalog = TestCatalog::users();
        let indexes = id_index();
        let planner = Planner::new(&catalog, &indexes);

        let stmt = Statement::Select(SelectStmt {
            projection: vec![SelectItem::Expr {
                expr: Expr::column("name"),
                alias: None,
            }],
            from: TableRef::new("users"),
            filter: Some(Expr::eq(Expr::column("id"), Expr::Literal(Value::Int8(1)))),
            limit: Some(5),
        });

        let plan = planner.plan(&stmt).unwrap();

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
        let catalog = TestCatalog::users();
        let indexes = id_index();
        let planner = Planner::new(&catalog, &indexes);

        let stmt = Statement::Update(UpdateStmt {
            table: TableRef::new("users"),
            assignments: vec![Assignment {
                column: "name".into(),
                value: Expr::Literal(Value::Text("grace".into())),
            }],
            filter: Some(Expr::eq(Expr::column("id"), Expr::Literal(Value::Int8(3)))),
        });

        let plan = planner.plan(&stmt).unwrap();

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
        let catalog = TestCatalog::users();
        let indexes = crate::index::NoIndexes;
        let planner = Planner::new(&catalog, &indexes);

        let stmt = Statement::Insert(InsertStmt {
            table: TableRef::new("users"),
            columns: vec!["name".into()],
            rows: vec![vec![Expr::Literal(Value::Text("ada".into()))]],
        });

        let plan = planner.plan(&stmt).unwrap();

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
        let catalog = TestCatalog::users();
        let indexes = crate::index::NoIndexes;
        let planner = Planner::new(&catalog, &indexes);

        let stmt = Statement::Select(SelectStmt {
            projection: vec![SelectItem::Wildcard],
            from: TableRef::new("ghosts"),
            filter: None,
            limit: None,
        });

        assert!(matches!(
            planner.plan(&stmt),
            Err(FerriteError::TableNotFound(_))
        ));
    }

    #[test]
    fn an_unknown_column_is_a_planning_error() {
        let catalog = TestCatalog::users();
        let indexes = crate::index::NoIndexes;
        let planner = Planner::new(&catalog, &indexes);

        assert!(matches!(
            planner.plan(&select(Some(Expr::eq(
                Expr::column("nope"),
                Expr::Literal(Value::Int8(1))
            )))),
            Err(FerriteError::ColumnNotFound(_))
        ));
    }
}
