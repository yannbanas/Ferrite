//! Rule-based planner: `ferrite-sql` AST -> logical plan -> physical plan.

use ferrite_common::{
    Catalog, ColumnDef, ColumnDefault, FerriteError, IndexCatalog, IndexDef, Schema, Value,
};
use ferrite_sql::ast as sql;

use crate::expr::{AggregateCall, AggregateFunc, BinaryOp, Expr};
use crate::logical::{
    split_conjunction, JoinType, LogicalPlan, ProjectionItem, SortKey, TableSource,
};
use crate::lower::{
    coerce, collect_aggregates, contains_aggregate, reject_ungrouped, row_count, single_select,
    substitute_group_keys, unsupported, AggregateSlots, Lowerer,
};
use crate::physical::{
    aggregate_schema, bind, infer, PhysAggregate, PhysExpr, PhysSortKey, PhysicalPlan,
};
use crate::rules::optimize;
use crate::scope::Scope;

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
    /// `ferrite-sql` can parse but `ferrite-exec` cannot run — subqueries,
    /// set operations, DDL, transaction control — leaves here as a
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
            | sql::Statement::AlterTable(_)
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

    /// `FROM` → `WHERE` → grouping → `HAVING` → `ORDER BY` → projection →
    /// `DISTINCT` → `LIMIT`, which is SQL's own evaluation order and the
    /// order the names become available in.
    ///
    /// `ORDER BY` sits *below* the projection so it can sort on columns the
    /// select list does not carry; select-list aliases and ordinals are
    /// resolved against the projection first, so both spellings work.
    fn build_select(&self, query: &sql::Query) -> Result<LogicalPlan, FerriteError> {
        let tail = single_select(query)?;
        let select = tail.select;

        let (mut plan, qualifiers) = self.build_from(&select.from)?;
        let from_scope = plan
            .scope()
            .expect("a FROM tree is made of scans and joins, which both have a scope");
        let lowerer = Lowerer::new(self.params).with_qualifiers(qualifiers.clone());

        if let Some(predicate) = &select.selection {
            if contains_aggregate(predicate) {
                return Err(FerriteError::Plan(
                    "aggregates are not allowed in WHERE; use HAVING".into(),
                ));
            }
            plan = LogicalPlan::Filter {
                input: Box::new(plan),
                predicate: lowerer.expr(predicate)?,
            };
        }

        let mut calls = Vec::new();
        for item in &select.projection {
            if let sql::SelectItem::Expr { expr, .. } = item {
                collect_aggregates(expr, &mut calls)?;
            }
        }
        if let Some(having) = &select.having {
            collect_aggregates(having, &mut calls)?;
        }
        for item in tail.order_by {
            collect_aggregates(&item.expr, &mut calls)?;
        }

        let grouped = !select.group_by.is_empty() || !calls.is_empty() || select.having.is_some();
        let mut group_keys = Vec::new();
        let scope = if grouped {
            for key in &select.group_by {
                group_keys.push(lowerer.expr(key)?);
            }
            let aggregates = calls
                .iter()
                .map(|call| lower_aggregate(call, &lowerer))
                .collect::<Result<Vec<_>, _>>()?;
            let schema = aggregate_schema(&group_keys, &aggregates, &from_scope)?;
            plan = LogicalPlan::Aggregate {
                input: Box::new(plan),
                group_by: group_keys.clone(),
                aggregates,
            };
            Scope::anonymous(&schema)
        } else {
            from_scope
        };

        let slots = AggregateSlots {
            calls: &calls,
            offset: group_keys.len(),
        };
        let upper = Lowerer::new(self.params)
            .with_qualifiers(qualifiers)
            .with_aggregates(slots);
        let rewrite = |expr: &sql::Expr| -> Result<Expr, FerriteError> {
            let lowered = substitute_group_keys(upper.expr(expr)?, &group_keys);
            if grouped {
                reject_ungrouped(&lowered)?;
            }
            Ok(lowered)
        };

        if let Some(having) = &select.having {
            plan = LogicalPlan::Filter {
                input: Box::new(plan),
                predicate: rewrite(having)?,
            };
        }

        let items = projection_items(&select.projection, &scope, grouped, &rewrite)?;

        let keys = sort_keys(tail.order_by, items.as_deref(), &rewrite)?;
        if !keys.is_empty() {
            plan = LogicalPlan::Sort {
                input: Box::new(plan),
                keys,
            };
        }

        if let Some(items) = items {
            plan = LogicalPlan::Projection {
                input: Box::new(plan),
                items,
            };
        }

        if select.distinct {
            plan = LogicalPlan::Distinct {
                input: Box::new(plan),
            };
        }

        let count = row_count(tail.limit, &lowerer, "LIMIT")?;
        let offset = row_count(tail.offset, &lowerer, "OFFSET")?.unwrap_or(0);
        if count.is_some() || offset > 0 {
            plan = LogicalPlan::Limit {
                input: Box::new(plan),
                count,
                offset,
            };
        }

        Ok(plan)
    }

    /// A comma-separated `FROM` list is a cross join, which is what the
    /// standard says it is; a `WHERE` that relates the two is then turned
    /// into the join predicate by [`crate::rules`].
    fn build_from(
        &self,
        from: &[sql::TableWithJoins],
    ) -> Result<(LogicalPlan, Vec<String>), FerriteError> {
        let mut items = from.iter();
        let Some(first) = items.next() else {
            return Err(unsupported("a SELECT without FROM"));
        };
        let (mut plan, mut qualifiers) = self.build_joins(first)?;
        for next in items {
            let (right, right_qualifiers) = self.build_joins(next)?;
            qualifiers.extend(right_qualifiers);
            plan = LogicalPlan::Join {
                left: Box::new(plan),
                right: Box::new(right),
                join_type: JoinType::Cross,
                on: None,
            };
        }
        Ok((plan, qualifiers))
    }

    fn build_joins(
        &self,
        item: &sql::TableWithJoins,
    ) -> Result<(LogicalPlan, Vec<String>), FerriteError> {
        let (mut plan, mut qualifiers) = self.table_factor(&item.relation)?;
        for join in &item.joins {
            let (right, right_qualifiers) = self.table_factor(&join.relation)?;
            let left_scope = plan.scope().expect("a FROM tree always has a scope");
            let right_scope = right.scope().expect("a FROM tree always has a scope");
            qualifiers.extend(right_qualifiers);

            let on = match &join.constraint {
                sql::JoinConstraint::None => None,
                sql::JoinConstraint::On(expr) => {
                    if contains_aggregate(expr) {
                        return Err(unsupported("an aggregate in a join condition"));
                    }
                    let lowerer = Lowerer::new(self.params).with_qualifiers(qualifiers.clone());
                    Some(lowerer.expr(expr)?)
                }
                sql::JoinConstraint::Using(columns) => {
                    Some(using_predicate(columns, &left_scope, &right_scope)?)
                }
            };

            plan = LogicalPlan::Join {
                left: Box::new(plan),
                right: Box::new(right),
                join_type: join_type(join.join_type),
                on,
            };
        }
        Ok((plan, qualifiers))
    }

    fn table_factor(
        &self,
        factor: &sql::TableFactor,
    ) -> Result<(LogicalPlan, Vec<String>), FerriteError> {
        match factor {
            sql::TableFactor::Table { name, alias } => {
                let source = self.resolve(name, alias.clone())?;
                let qualifiers = source.qualifiers();
                Ok((
                    LogicalPlan::Scan {
                        source,
                        filter: None,
                    },
                    qualifiers,
                ))
            }
            sql::TableFactor::Derived { .. } => Err(unsupported("a subquery in FROM")),
        }
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

        let source = self.resolve(&stmt.table, None)?;
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

        // Every column the statement does not name starts at its
        // `DEFAULT`, which is `NULL` when there is none. Evaluated once
        // for the whole statement, so every row of a multi-row `INSERT`
        // gets the same `CURRENT_TIMESTAMP`, as PostgreSQL does.
        let defaults: Vec<Expr> = source
            .schema
            .columns
            .iter()
            .map(|column| default_expr(column.default.as_ref()))
            .collect();

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
            let mut row = defaults.clone();
            debug_assert_eq!(row.len(), width);
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
        let source = self.resolve(&stmt.table, stmt.alias.clone())?;
        let lowerer = Lowerer::new(self.params).with_qualifiers(source.qualifiers());

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
        let source = self.resolve(&stmt.table, stmt.alias.clone())?;
        let lowerer = Lowerer::new(self.params).with_qualifiers(source.qualifiers());

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
        Ok(self.lower_plan(plan)?.0)
    }

    /// The scope travels back up with the plan: a node binds its own
    /// expressions against the shape its input produces, which for a join
    /// is neither side's schema on its own.
    fn lower_plan(&self, plan: LogicalPlan) -> Result<(PhysicalPlan, Scope), FerriteError> {
        match plan {
            LogicalPlan::Scan { source, filter } => {
                let scope = Scope::for_relation(&source.schema, &source.qualifiers());
                let physical = self.access_path(&source, filter, &scope)?;
                Ok((physical, scope))
            }

            LogicalPlan::Join {
                left,
                right,
                join_type,
                on,
            } => {
                let (left_plan, mut left_scope) = self.lower_plan(*left)?;
                let (right_plan, mut right_scope) = self.lower_plan(*right)?;
                if join_type.preserves_right() {
                    left_scope = left_scope.nullable();
                }
                if join_type.preserves_left() {
                    right_scope = right_scope.nullable();
                }
                let scope = Scope::concat(left_scope, right_scope);
                let predicate = on.map(|e| bind(&e, &scope)).transpose()?;
                let output = scope.schema();
                Ok((
                    PhysicalPlan::NestedLoopJoin {
                        left: Box::new(left_plan),
                        right: Box::new(right_plan),
                        join_type,
                        predicate,
                        output,
                    },
                    scope,
                ))
            }

            LogicalPlan::Filter { input, predicate } => {
                let (input, scope) = self.lower_plan(*input)?;
                Ok((
                    PhysicalPlan::Filter {
                        predicate: bind(&predicate, &scope)?,
                        input: Box::new(input),
                    },
                    scope,
                ))
            }

            LogicalPlan::Aggregate {
                input,
                group_by,
                aggregates,
            } => {
                let (input, input_scope) = self.lower_plan(*input)?;
                let output = aggregate_schema(&group_by, &aggregates, &input_scope)?;
                let group_by = group_by
                    .iter()
                    .map(|e| bind(e, &input_scope))
                    .collect::<Result<Vec<_>, _>>()?;
                let aggregates = aggregates
                    .iter()
                    .map(|call| {
                        Ok(PhysAggregate {
                            func: call.func,
                            arg: call
                                .arg
                                .as_ref()
                                .map(|e| bind(e, &input_scope))
                                .transpose()?,
                            distinct: call.distinct,
                        })
                    })
                    .collect::<Result<Vec<_>, FerriteError>>()?;
                let scope = Scope::anonymous(&output);
                Ok((
                    PhysicalPlan::Aggregate {
                        input: Box::new(input),
                        group_by,
                        aggregates,
                        output,
                    },
                    scope,
                ))
            }

            LogicalPlan::Projection { input, items } => {
                let (input, input_scope) = self.lower_plan(*input)?;
                let mut exprs = Vec::with_capacity(items.len());
                let mut columns = Vec::with_capacity(items.len());
                for item in &items {
                    exprs.push(bind(&item.expr, &input_scope)?);
                    let (data_type, nullable) = infer(&item.expr, &input_scope)?;
                    columns.push(ColumnDef::new(
                        item.output_name.clone(),
                        data_type,
                        nullable,
                    ));
                }
                let output = Schema { columns };
                let scope = Scope::anonymous(&output);
                Ok((
                    PhysicalPlan::Projection {
                        input: Box::new(input),
                        exprs,
                        output,
                    },
                    scope,
                ))
            }

            LogicalPlan::Sort { input, keys } => {
                let (input, scope) = self.lower_plan(*input)?;
                let keys = keys
                    .iter()
                    .map(|key| {
                        Ok(PhysSortKey {
                            expr: bind(&key.expr, &scope)?,
                            asc: key.asc,
                            nulls_first: key.nulls_first,
                        })
                    })
                    .collect::<Result<Vec<_>, FerriteError>>()?;
                Ok((
                    PhysicalPlan::Sort {
                        input: Box::new(input),
                        keys,
                    },
                    scope,
                ))
            }

            LogicalPlan::Distinct { input } => {
                let (input, scope) = self.lower_plan(*input)?;
                Ok((
                    PhysicalPlan::Distinct {
                        input: Box::new(input),
                    },
                    scope,
                ))
            }

            LogicalPlan::Limit {
                input,
                count,
                offset,
            } => {
                let (input, scope) = self.lower_plan(*input)?;
                Ok((
                    PhysicalPlan::Limit {
                        input: Box::new(input),
                        count,
                        offset,
                    },
                    scope,
                ))
            }

            LogicalPlan::Insert { source, rows } => {
                let empty = Scope::empty();
                let rows = rows
                    .iter()
                    .map(|row| row.iter().map(|e| bind(e, &empty)).collect())
                    .collect::<Result<Vec<Vec<_>>, _>>()?;
                Ok((
                    PhysicalPlan::Insert {
                        table: source.id,
                        table_name: source.name,
                        schema: source.schema,
                        rows,
                    },
                    Scope::empty(),
                ))
            }

            LogicalPlan::Update {
                source,
                input,
                assignments,
            } => {
                let scan = self.row_identity_source(*input, &source.name)?;
                let scope = Scope::for_relation(&source.schema, &source.qualifiers());
                let assignments = assignments
                    .iter()
                    .map(|(position, expr)| Ok((*position, bind(expr, &scope)?)))
                    .collect::<Result<Vec<_>, FerriteError>>()?;
                Ok((
                    PhysicalPlan::Update {
                        table: source.id,
                        table_name: source.name,
                        schema: source.schema,
                        source: Box::new(scan),
                        assignments,
                    },
                    Scope::empty(),
                ))
            }

            LogicalPlan::Delete { source, input } => {
                let scan = self.row_identity_source(*input, &source.name)?;
                Ok((
                    PhysicalPlan::Delete {
                        table: source.id,
                        table_name: source.name,
                        schema: source.schema,
                        source: Box::new(scan),
                    },
                    Scope::empty(),
                ))
            }

            LogicalPlan::Call { name, args } => {
                let empty = Scope::empty();
                let args = args
                    .iter()
                    .map(|e| bind(e, &empty))
                    .collect::<Result<Vec<_>, _>>()?;
                Ok((PhysicalPlan::CallProcedure { name, args }, Scope::empty()))
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
        scope: &Scope,
    ) -> Result<PhysicalPlan, FerriteError> {
        let mut conjuncts = filter.map(split_conjunction).unwrap_or_default();
        let indexes = self.indexes.indexes_for(source.id)?;

        let chosen = conjuncts.iter().enumerate().find_map(|(position, expr)| {
            let (column, key) = index_equality(expr, scope)?;
            let index = pick_index(&indexes, &source.schema.columns[column].name)?;
            Some((position, index.clone(), column, key))
        });

        match chosen {
            Some((position, index, column, key)) => {
                conjuncts.remove(position);
                let residual = crate::logical::combine_conjunction(conjuncts)
                    .map(|e| bind(&e, scope))
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
                    .map(|e| bind(&e, scope))
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

    fn resolve(
        &self,
        table: &sql::ObjectName,
        alias: Option<String>,
    ) -> Result<TableSource, FerriteError> {
        let (namespace, name) = table.split(DEFAULT_NAMESPACE);
        let id = self
            .catalog
            .table_id(namespace, name)?
            .ok_or_else(|| FerriteError::TableNotFound(format!("{namespace}.{name}")))?;
        Ok(TableSource {
            id,
            name: name.to_string(),
            alias,
            schema: self.catalog.table_schema(id)?,
        })
    }
}

/// The value a column takes when a write does not supply one.
///
/// `CURRENT_TIMESTAMP` is resolved here, at planning time, rather than
/// carried into the physical plan as a node the executor evaluates per
/// row: PostgreSQL's `now()` is fixed for the whole statement too, and
/// folding it into a literal keeps `PhysExpr` free of a volatile variant
/// whose value would depend on when the executor happened to reach it.
fn default_expr(default: Option<&ColumnDefault>) -> Expr {
    Expr::Literal(match default {
        None => Value::Null,
        Some(ColumnDefault::Constant(value)) => value.clone(),
        Some(ColumnDefault::CurrentTimestamp) => Value::Timestamp(now_micros()),
    })
}

/// Microseconds since the Unix epoch, saturating at the epoch itself for a
/// clock set before 1970 — a `DEFAULT` must not be able to panic.
fn now_micros() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_micros()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

fn join_type(parsed: sql::JoinType) -> JoinType {
    match parsed {
        sql::JoinType::Inner => JoinType::Inner,
        sql::JoinType::Left => JoinType::Left,
        sql::JoinType::Right => JoinType::Right,
        sql::JoinType::Full => JoinType::Full,
        sql::JoinType::Cross => JoinType::Cross,
    }
}

/// `USING (a, b)` is `left.a = right.a AND left.b = right.b`, resolved to
/// positions here because the two sides may each be a join of several
/// relations, so neither name alone would be unambiguous.
fn using_predicate(columns: &[String], left: &Scope, right: &Scope) -> Result<Expr, FerriteError> {
    let mut conjuncts = Vec::with_capacity(columns.len());
    for name in columns {
        let reference = crate::expr::ColumnRef::new(name.clone());
        let left_position = left.resolve(&reference)?;
        let right_position = right.resolve(&reference)?;
        conjuncts.push(Expr::eq(
            Expr::Slot(left_position),
            Expr::Slot(left.len() + right_position),
        ));
    }
    crate::logical::combine_conjunction(conjuncts)
        .ok_or_else(|| FerriteError::Plan("USING needs at least one column".into()))
}

fn lower_aggregate(
    call: &sql::FunctionCall,
    lowerer: &Lowerer<'_>,
) -> Result<AggregateCall, FerriteError> {
    let func = AggregateFunc::parse(&call.name)
        .ok_or_else(|| unsupported(&format!("the aggregate {}", call.name)))?;
    let arg = match &call.args {
        sql::FunctionArgs::Wildcard => {
            if func != AggregateFunc::Count {
                return Err(FerriteError::Plan(format!(
                    "{}(*) is not defined; only count(*) is",
                    call.name
                )));
            }
            None
        }
        sql::FunctionArgs::List(args) => match args.as_slice() {
            [only] => Some(lowerer.expr(only)?),
            _ => {
                return Err(FerriteError::Plan(format!(
                    "{}() takes exactly one argument",
                    call.name
                )))
            }
        },
    };
    Ok(AggregateCall {
        func,
        arg,
        distinct: call.distinct,
    })
}

/// A projection item before its output name has been decided.
/// `None` means "no projection node needed" (bare `SELECT *`).
fn projection_items(
    projection: &[sql::SelectItem],
    scope: &Scope,
    grouped: bool,
    rewrite: &impl Fn(&sql::Expr) -> Result<Expr, FerriteError>,
) -> Result<Option<Vec<ProjectionItem>>, FerriteError> {
    if projection.is_empty() {
        return Err(FerriteError::Plan("empty SELECT list".into()));
    }
    if projection.len() == 1 && matches!(projection[0], sql::SelectItem::Wildcard) && !grouped {
        return Ok(None);
    }

    let mut items = Vec::with_capacity(projection.len());
    for item in projection {
        match item {
            sql::SelectItem::Wildcard | sql::SelectItem::QualifiedWildcard(_) if grouped => {
                return Err(FerriteError::Plan(
                    "* cannot be used with GROUP BY or an aggregate".into(),
                ))
            }
            sql::SelectItem::Wildcard => {
                items.extend(expand(0..scope.len(), scope)?);
            }
            sql::SelectItem::QualifiedWildcard(name) => {
                let positions = scope.positions_for(name.base());
                if positions.is_empty() {
                    return Err(FerriteError::Plan(format!(
                        "no relation named {} in this statement",
                        name.base()
                    )));
                }
                items.extend(expand(positions, scope)?);
            }
            sql::SelectItem::Expr { expr, alias } => items.push(ProjectionItem {
                expr: rewrite(expr)?,
                output_name: alias.clone().unwrap_or_else(|| output_name(expr)),
            }),
        }
    }
    Ok(Some(items))
}

fn expand(
    positions: impl IntoIterator<Item = usize>,
    scope: &Scope,
) -> Result<Vec<ProjectionItem>, FerriteError> {
    positions
        .into_iter()
        .map(|position| {
            Ok(ProjectionItem {
                expr: Expr::Slot(position),
                output_name: scope.column(position)?.name.clone(),
            })
        })
        .collect()
}

/// What PostgreSQL calls the column when the select item has no alias: the
/// column's own name, the function's name, or `?column?`.
fn output_name(expr: &sql::Expr) -> String {
    match expr {
        sql::Expr::Column(name) => name.base().to_string(),
        sql::Expr::Function(call) => call.name.clone(),
        sql::Expr::Cast { data_type, .. } => format!("{data_type:?}").to_lowercase(),
        _ => "?column?".to_string(),
    }
}

/// `ORDER BY` resolves an ordinal or a bare select-list alias against the
/// projection, and anything else against the row the projection reads —
/// which is why the sort node sits below it.
///
/// A missing `NULLS FIRST`/`NULLS LAST` follows PostgreSQL: nulls sort as
/// if larger than any value, so they come last ascending and first
/// descending.
fn sort_keys(
    order_by: &[sql::OrderByItem],
    items: Option<&[ProjectionItem]>,
    rewrite: &impl Fn(&sql::Expr) -> Result<Expr, FerriteError>,
) -> Result<Vec<SortKey>, FerriteError> {
    let mut keys = Vec::with_capacity(order_by.len());
    for item in order_by {
        let expr = match select_list_reference(&item.expr, items)? {
            Some(expr) => expr,
            None => rewrite(&item.expr)?,
        };
        keys.push(SortKey {
            expr,
            asc: item.asc,
            nulls_first: item.nulls_first.unwrap_or(!item.asc),
        });
    }
    Ok(keys)
}

fn select_list_reference(
    expr: &sql::Expr,
    items: Option<&[ProjectionItem]>,
) -> Result<Option<Expr>, FerriteError> {
    let items = items.unwrap_or_default();
    match expr {
        sql::Expr::Literal(sql::Literal::Int(n)) => {
            let position = usize::try_from(*n)
                .ok()
                .and_then(|n| n.checked_sub(1))
                .filter(|n| *n < items.len())
                .ok_or_else(|| {
                    FerriteError::Plan(format!("ORDER BY position {n} is not in the select list"))
                })?;
            Ok(Some(items[position].expr.clone()))
        }
        sql::Expr::Column(name) if name.qualifier().is_none() => Ok(items
            .iter()
            .find(|item| item.output_name == name.base())
            .map(|item| item.expr.clone())),
        _ => Ok(None),
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
fn index_equality(expr: &Expr, scope: &Scope) -> Option<(usize, Value)> {
    let Expr::Binary {
        left,
        op: BinaryOp::Eq,
        right,
    } = expr
    else {
        return None;
    };
    let (reference, value) = match (left.as_ref(), right.as_ref()) {
        (Expr::Column(reference), Expr::Literal(value)) => (reference, value),
        (Expr::Literal(value), Expr::Column(reference)) => (reference, value),
        _ => return None,
    };
    if value.is_null() {
        return None;
    }
    let position = scope.resolve(reference).ok()?;
    match coerce(
        Expr::Literal(value.clone()),
        scope.column(position).ok()?.data_type,
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
mod tests;
