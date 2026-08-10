//! Single-threaded physical plan executor.

use ferrite_common::{
    Catalog, ColumnDefault, DataType, FerriteError, Identity, Permission, Row, RowId, Schema,
    StorageEngine, TableId, TxnId, Value,
};
use ferrite_planner::{PhysExpr, PhysicalPlan};
use ferrite_proc::{ProcDecision, ProcRegistry, TriggerEvent};

use crate::eval::{eval, eval_predicate};
use crate::index::IndexProvider;

/// A row as it flows through the plan. `rid` is present only while the row
/// still comes straight from storage; a projection drops it, which is why
/// `UPDATE`/`DELETE` sources may not contain one (the planner enforces
/// this, see `PhysicalPlan::preserves_row_identity`).
#[derive(Debug, Clone, PartialEq)]
pub struct Tuple {
    pub rid: Option<RowId>,
    pub row: Row,
}

/// The table a scan reads, paired with the schema the plan was built
/// against. The two travel together because every row coming out of
/// storage is reconciled with that schema — see [`fill_added_columns`].
#[derive(Debug, Clone, Copy)]
struct ScanTarget<'a> {
    table: TableId,
    schema: &'a Schema,
}

/// Outcome of one statement.
#[derive(Debug, Clone, PartialEq)]
pub enum QueryResult {
    Rows {
        schema: Schema,
        rows: Vec<Row>,
    },
    /// Rows actually written by `INSERT`/`UPDATE`/`DELETE`. Rows skipped
    /// by a `BEFORE` trigger are not counted.
    Affected(usize),
    /// Return value of `CALL`.
    Value(Value),
}

/// One caller's execution context: the engines it talks to plus the
/// identity every permission check and every trigger will see.
///
/// v1 is deliberately single-threaded and materializing: each node
/// collects its input into a `Vec<Tuple>` rather than pulling lazily.
/// `docs/architecture.md` cuts parallel query execution for v1, and a
/// materializing walk keeps the borrow story trivial while the storage
/// scan iterator borrows the engine.
pub struct Session<'a> {
    storage: &'a dyn StorageEngine,
    catalog: &'a dyn Catalog,
    procs: &'a ProcRegistry,
    indexes: Option<&'a dyn IndexProvider>,
    identity: Identity,
}

impl<'a> Session<'a> {
    pub fn new(
        storage: &'a dyn StorageEngine,
        catalog: &'a dyn Catalog,
        procs: &'a ProcRegistry,
        identity: Identity,
    ) -> Self {
        Self {
            storage,
            catalog,
            procs,
            indexes: None,
            identity,
        }
    }

    /// Wire up index access. Without one, an `IndexScan` still executes —
    /// it degrades to a sequential scan filtered on the index key.
    pub fn with_indexes(mut self, indexes: &'a dyn IndexProvider) -> Self {
        self.indexes = Some(indexes);
        self
    }

    pub fn identity(&self) -> Identity {
        self.identity
    }

    /// Run one statement under `txn`. Permission checks happen first, so a
    /// denied statement never reaches storage.
    pub fn execute(&self, txn: TxnId, plan: &PhysicalPlan) -> Result<QueryResult, FerriteError> {
        match plan {
            PhysicalPlan::Insert {
                table,
                table_name,
                schema,
                rows,
            } => {
                self.procs
                    .authorize(self.identity, txn, Permission::Insert)?;
                self.check_schema(*table, table_name, schema)?;
                self.run_insert(txn, *table, table_name, schema, rows)
            }

            PhysicalPlan::Update {
                table,
                table_name,
                schema,
                source,
                assignments,
            } => {
                self.procs
                    .authorize(self.identity, txn, Permission::Update)?;
                self.check_schema(*table, table_name, schema)?;
                self.run_update(txn, *table, table_name, schema, source, assignments)
            }

            PhysicalPlan::Delete {
                table,
                table_name,
                schema,
                source,
            } => {
                self.procs
                    .authorize(self.identity, txn, Permission::Delete)?;
                self.check_schema(*table, table_name, schema)?;
                self.run_delete(txn, *table, table_name, source)
            }

            PhysicalPlan::CallProcedure { name, args } => {
                let empty = Row::new(Vec::new());
                let evaluated = args
                    .iter()
                    .map(|a| eval(a, &empty))
                    .collect::<Result<Vec<_>, _>>()?;
                let ctx = self.procs.context(self.identity, txn);
                Ok(QueryResult::Value(self.procs.call(&ctx, name, &evaluated)?))
            }

            query => {
                self.procs
                    .authorize(self.identity, txn, Permission::Select)?;
                let schema = query
                    .output_schema()
                    .cloned()
                    .ok_or_else(|| FerriteError::Exec("plan produces no rows".into()))?;
                let rows = self
                    .stream(txn, query)?
                    .into_iter()
                    .map(|t| t.row)
                    .collect();
                Ok(QueryResult::Rows { schema, rows })
            }
        }
    }

    /// Materialize the rows produced by a query node.
    fn stream(&self, txn: TxnId, plan: &PhysicalPlan) -> Result<Vec<Tuple>, FerriteError> {
        match plan {
            PhysicalPlan::SeqScan {
                table,
                table_name,
                schema,
                filter,
            } => {
                self.check_schema(*table, table_name, schema)?;
                self.seq_scan(
                    txn,
                    ScanTarget {
                        table: *table,
                        schema,
                    },
                    filter.as_ref(),
                )
            }

            PhysicalPlan::IndexScan {
                table,
                table_name,
                schema,
                index,
                column,
                key,
                residual,
            } => {
                self.check_schema(*table, table_name, schema)?;
                self.index_scan(
                    txn,
                    ScanTarget {
                        table: *table,
                        schema,
                    },
                    index,
                    *column,
                    key,
                    residual.as_ref(),
                )
            }

            PhysicalPlan::Filter { input, predicate } => {
                let mut out = Vec::new();
                for tuple in self.stream(txn, input)? {
                    if eval_predicate(predicate, &tuple.row)? {
                        out.push(tuple);
                    }
                }
                Ok(out)
            }

            PhysicalPlan::Projection { input, exprs, .. } => self
                .stream(txn, input)?
                .into_iter()
                .map(|tuple| {
                    let values = exprs
                        .iter()
                        .map(|e| eval(e, &tuple.row))
                        .collect::<Result<Vec<_>, _>>()?;
                    Ok(Tuple {
                        rid: None,
                        row: Row::new(values),
                    })
                })
                .collect(),

            PhysicalPlan::Limit { input, count } => {
                let mut rows = self.stream(txn, input)?;
                rows.truncate(usize::try_from(*count).unwrap_or(usize::MAX));
                Ok(rows)
            }

            other => Err(FerriteError::Exec(format!(
                "{} is a statement root, not a row source",
                node_name(other)
            ))),
        }
    }

    fn seq_scan(
        &self,
        txn: TxnId,
        target: ScanTarget<'_>,
        filter: Option<&PhysExpr>,
    ) -> Result<Vec<Tuple>, FerriteError> {
        let scanned = self
            .storage
            .scan(txn, target.table)?
            .collect::<Result<Vec<_>, _>>()?;
        let mut out = Vec::new();
        for (rid, mut row) in scanned {
            fill_added_columns(target.schema, &mut row);
            let keep = match filter {
                Some(predicate) => eval_predicate(predicate, &row)?,
                None => true,
            };
            if keep {
                out.push(Tuple {
                    rid: Some(rid),
                    row,
                });
            }
        }
        Ok(out)
    }

    fn index_scan(
        &self,
        txn: TxnId,
        target: ScanTarget<'_>,
        index: &str,
        column: usize,
        key: &PhysExpr,
        residual: Option<&PhysExpr>,
    ) -> Result<Vec<Tuple>, FerriteError> {
        let key = eval(key, &Row::new(Vec::new()))?;

        let candidates = match self.indexes {
            Some(provider) => {
                let mut out = Vec::new();
                for rid in provider.lookup(txn, target.table, index, &key)? {
                    let mut row = self.storage.get(txn, target.table, rid)?;
                    fill_added_columns(target.schema, &mut row);
                    out.push(Tuple {
                        row,
                        rid: Some(rid),
                    });
                }
                out
            }
            None => {
                tracing::warn!(
                    index,
                    table = target.table,
                    "no index provider wired: falling back to a sequential scan"
                );
                let equality = PhysExpr::binary(
                    PhysExpr::Column(column),
                    ferrite_planner::BinaryOp::Eq,
                    PhysExpr::Literal(key),
                );
                self.seq_scan(txn, target, Some(&equality))?
            }
        };

        let Some(predicate) = residual else {
            return Ok(candidates);
        };
        let mut out = Vec::new();
        for tuple in candidates {
            if eval_predicate(predicate, &tuple.row)? {
                out.push(tuple);
            }
        }
        Ok(out)
    }

    fn run_insert(
        &self,
        txn: TxnId,
        table: TableId,
        table_name: &str,
        schema: &Schema,
        rows: &[Vec<PhysExpr>],
    ) -> Result<QueryResult, FerriteError> {
        let empty = Row::new(Vec::new());
        let ctx = self
            .procs
            .context(self.identity, txn)
            .with_table(table, table_name)
            .with_event(TriggerEvent::Insert);

        let mut affected = 0;
        for exprs in rows {
            let values = exprs
                .iter()
                .map(|e| eval(e, &empty))
                .collect::<Result<Vec<_>, _>>()?;
            let candidate = Row::new(values);

            let mut row = match self
                .procs
                .fire_before(&ctx, TriggerEvent::Insert, &candidate)?
            {
                ProcDecision::Skip => continue,
                ProcDecision::Allow => candidate,
                ProcDecision::Replace(row) => row,
            };

            conform_row(schema, &mut row, table_name)?;
            self.storage.insert(txn, table, row)?;
            affected += 1;
        }
        Ok(QueryResult::Affected(affected))
    }

    fn run_update(
        &self,
        txn: TxnId,
        table: TableId,
        table_name: &str,
        schema: &Schema,
        source: &PhysicalPlan,
        assignments: &[(usize, PhysExpr)],
    ) -> Result<QueryResult, FerriteError> {
        let targets = self.stream(txn, source)?;
        let mut affected = 0;

        for tuple in targets {
            let rid = tuple
                .rid
                .ok_or_else(|| FerriteError::Exec("UPDATE source lost row identity".to_string()))?;

            // Assignments read the pre-image, as SQL requires.
            let mut candidate = tuple.row.clone();
            for (position, expr) in assignments {
                let value = eval(expr, &tuple.row)?;
                *candidate.values.get_mut(*position).ok_or_else(|| {
                    FerriteError::Exec(format!("column {position} out of range"))
                })? = value;
            }

            let ctx = self
                .procs
                .context(self.identity, txn)
                .with_table(table, table_name)
                .with_event(TriggerEvent::Update)
                .with_old_row(&tuple.row);

            let mut row = match self
                .procs
                .fire_before(&ctx, TriggerEvent::Update, &candidate)?
            {
                ProcDecision::Skip => continue,
                ProcDecision::Allow => candidate,
                ProcDecision::Replace(row) => row,
            };

            conform_row(schema, &mut row, table_name)?;
            self.storage.update(txn, table, rid, row)?;
            affected += 1;
        }
        Ok(QueryResult::Affected(affected))
    }

    fn run_delete(
        &self,
        txn: TxnId,
        table: TableId,
        table_name: &str,
        source: &PhysicalPlan,
    ) -> Result<QueryResult, FerriteError> {
        let targets = self.stream(txn, source)?;
        let mut affected = 0;

        for tuple in targets {
            let rid = tuple
                .rid
                .ok_or_else(|| FerriteError::Exec("DELETE source lost row identity".to_string()))?;

            let ctx = self
                .procs
                .context(self.identity, txn)
                .with_table(table, table_name)
                .with_event(TriggerEvent::Delete)
                .with_old_row(&tuple.row);

            match self
                .procs
                .fire_before(&ctx, TriggerEvent::Delete, &tuple.row)?
            {
                ProcDecision::Skip => continue,
                // A `DELETE` has no row to rewrite; a trigger asking for
                // one is a programming error, not a runtime condition.
                ProcDecision::Replace(_) => {
                    return Err(FerriteError::Exec(format!(
                        "a BEFORE DELETE trigger on {table_name} returned Replace"
                    )))
                }
                ProcDecision::Allow => {}
            }

            self.storage.delete(txn, table, rid)?;
            affected += 1;
        }
        Ok(QueryResult::Affected(affected))
    }

    /// Guard against a plan cached across a schema change. The planner
    /// baked the schema into the plan; if the catalog disagrees, the plan
    /// is stale and its column positions cannot be trusted.
    fn check_schema(
        &self,
        table: TableId,
        table_name: &str,
        planned: &Schema,
    ) -> Result<(), FerriteError> {
        if self.catalog.table_schema(table)? == *planned {
            return Ok(());
        }
        Err(FerriteError::Plan(format!(
            "stale plan: the schema of {table_name} changed since planning"
        )))
    }
}

fn node_name(plan: &PhysicalPlan) -> &'static str {
    match plan {
        PhysicalPlan::SeqScan { .. } => "SeqScan",
        PhysicalPlan::IndexScan { .. } => "IndexScan",
        PhysicalPlan::Filter { .. } => "Filter",
        PhysicalPlan::Projection { .. } => "Projection",
        PhysicalPlan::Limit { .. } => "Limit",
        PhysicalPlan::Insert { .. } => "Insert",
        PhysicalPlan::Update { .. } => "Update",
        PhysicalPlan::Delete { .. } => "Delete",
        PhysicalPlan::CallProcedure { .. } => "CallProcedure",
    }
}

/// Reconcile a stored row with a schema that has grown since it was
/// written, by appending the missing trailing values.
///
/// `ALTER TABLE … ADD COLUMN` records a column in the catalog and touches
/// no table data, so every row written before it is one value short of the
/// new schema. Nothing below this crate can fix that: `ferrite_common::Row`
/// is positional and `ferrite-storage` is schema-blind — it stores a
/// `Vec<Value>` and has never been told how many columns a table has. The
/// alternative, rewriting every row at `ALTER` time, would turn an `O(1)`
/// catalog write into a full table rewrite under one engine-wide lock, and
/// it is not what PostgreSQL does either (`pg_attribute.attmissingval`
/// holds the value and the heap tuples stay short).
///
/// The filler is the column's `DEFAULT` when that default is a constant,
/// and `NULL` otherwise. A volatile default — `CURRENT_TIMESTAMP` — has no
/// honest answer for a row that predates the column, and inventing one at
/// read time would make the same row read differently twice. `ALTER TABLE`
/// refuses to add a `NOT NULL` column to a non-empty table unless the
/// default is a constant, which is what keeps that `NULL` from reaching a
/// column declared not to hold one.
///
/// A row longer than the schema is left alone: v1 has no `DROP COLUMN`, so
/// the only way to see one is a plan built against a stale schema, and
/// `Session::check_schema` catches that first.
fn fill_added_columns(schema: &Schema, row: &mut Row) {
    for column in schema.columns.iter().skip(row.values.len()) {
        row.values.push(
            match column.default.as_ref().and_then(ColumnDefault::constant) {
                Some(value) => value.clone(),
                None => Value::Null,
            },
        );
    }
}

/// Arity, nullability and type check before a row reaches storage, plus the
/// widening that check allows. Runs *after* `BEFORE` triggers, so a trigger
/// that rewrites a row cannot smuggle a malformed one past it.
///
/// The widening is applied, not merely permitted. A stored value whose
/// variant disagreed with its column's declared type would be read back and
/// put on the wire under that column's OID, so an `Int4` left sitting in a
/// `BIGINT` column would send four bytes where the client expects eight.
fn conform_row(schema: &Schema, row: &mut Row, table: &str) -> Result<(), FerriteError> {
    if row.values.len() != schema.columns.len() {
        return Err(FerriteError::Exec(format!(
            "{table} has {} columns, got a row of {}",
            schema.columns.len(),
            row.values.len()
        )));
    }
    for (column, value) in schema.columns.iter().zip(&mut row.values) {
        match value.data_type() {
            None if column.nullable => {}
            None => {
                return Err(FerriteError::Exec(format!(
                    "{table}.{} is not nullable",
                    column.name
                )))
            }
            Some(actual) if actual == column.data_type => {}
            Some(actual) => match widen(value, column.data_type) {
                Some(widened) => *value = widened,
                None => {
                    return Err(FerriteError::TypeMismatch {
                        expected: column.data_type,
                        actual,
                    })
                }
            },
        }
    }
    Ok(())
}

/// Implicit widening only — no string/number coercion, no truncation.
fn widen(value: &Value, to: DataType) -> Option<Value> {
    match (value, to) {
        (Value::Int4(v), DataType::Int8) => Some(Value::Int8(i64::from(*v))),
        (Value::Int4(v), DataType::Float8) => Some(Value::Float8(f64::from(*v))),
        (Value::Int8(v), DataType::Float8) => Some(Value::Float8(*v as f64)),
        _ => None,
    }
}
