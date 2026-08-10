//! Physical plan: an access-path decision plus expressions bound to column
//! positions, so the executor never resolves a name at runtime.

use ferrite_common::{ColumnDef, DataType, FerriteError, Schema, TableId, Value};

use crate::expr::{AggregateFunc, BinaryOp, Expr};
use crate::logical::{JoinType, LogicalPlan};
use crate::scalar::ScalarFunc;
use crate::scope::Scope;

/// Expression with every column reference resolved to its position in the
/// input row.
#[derive(Debug, Clone, PartialEq)]
pub enum PhysExpr {
    Column(usize),
    Literal(Value),
    Binary {
        left: Box<PhysExpr>,
        op: BinaryOp,
        right: Box<PhysExpr>,
    },
    Not(Box<PhysExpr>),
    IsNull(Box<PhysExpr>),
    Like {
        expr: Box<PhysExpr>,
        pattern: Box<PhysExpr>,
        negated: bool,
        case_insensitive: bool,
    },
    Case {
        operand: Option<Box<PhysExpr>>,
        branches: Vec<(PhysExpr, PhysExpr)>,
        else_result: Option<Box<PhysExpr>>,
    },
    Cast {
        expr: Box<PhysExpr>,
        data_type: DataType,
    },
    Function {
        func: ScalarFunc,
        args: Vec<PhysExpr>,
    },
    /// `expr IN (SELECT …)`, still holding its subplan. The executor runs
    /// the subplan once and rewrites this node into the equivalent value
    /// test before evaluating any row — see
    /// `ferrite_exec::Session::execute`. Reaching [`crate::PhysExpr`]
    /// evaluation with this variant still in place is a bug, and says so.
    InSubquery {
        expr: Box<PhysExpr>,
        subquery: Box<PhysicalPlan>,
        negated: bool,
    },
}

impl PhysExpr {
    pub fn binary(left: PhysExpr, op: BinaryOp, right: PhysExpr) -> Self {
        PhysExpr::Binary {
            left: Box::new(left),
            op,
            right: Box::new(right),
        }
    }
}

/// One aggregate to compute over a group. `arg` is `None` for `count(*)`.
#[derive(Debug, Clone, PartialEq)]
pub struct PhysAggregate {
    pub func: AggregateFunc,
    pub arg: Option<PhysExpr>,
    pub distinct: bool,
}

/// [`crate::logical::OnConflict`] with its expressions bound.
#[derive(Debug, Clone, PartialEq)]
pub struct PhysOnConflict {
    pub target: Vec<usize>,
    pub action: PhysConflictAction,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PhysConflictAction {
    Nothing,
    /// Bound against the existing row followed by the excluded row, so a
    /// position past the table's width reads the excluded half.
    Update {
        assignments: Vec<(usize, PhysExpr)>,
        selection: Option<PhysExpr>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct PhysSortKey {
    pub expr: PhysExpr,
    pub asc: bool,
    pub nulls_first: bool,
}

/// Resolve column names against `scope`. Pass [`Scope::empty`] for contexts
/// where column references are illegal (`INSERT ... VALUES`, `CALL`
/// arguments); the resulting `ColumnNotFound` is the correct error.
///
/// A literal compared against a column is coerced to that column's declared
/// type on the way through. The executor compares `Value` variants and an
/// index probe compares them for exact equality, so `WHERE id = 1` against
/// a `BIGINT` column has to carry an `Int8`, not the `Int4` the literal
/// parsed as — otherwise the probe silently matches nothing.
pub fn bind(expr: &Expr, scope: &Scope) -> Result<PhysExpr, FerriteError> {
    bind_with(expr, scope, &|_| {
        Err(FerriteError::Plan(
            "a subquery is not allowed here".to_string(),
        ))
    })
}

/// How a subquery's logical plan becomes a physical one. Only the planner
/// can do it — choosing an access path needs the index catalog — so it is
/// handed in rather than reached for.
pub type SubPlanLowerer<'a> = &'a dyn Fn(&LogicalPlan) -> Result<PhysicalPlan, FerriteError>;

/// [`bind`], plus the ability to lower a subquery's plan.
pub fn bind_with(
    expr: &Expr,
    scope: &Scope,
    lower: SubPlanLowerer,
) -> Result<PhysExpr, FerriteError> {
    let bind = |expr: &Expr, scope: &Scope| bind_with(expr, scope, lower);
    Ok(match expr {
        Expr::Column(reference) => PhysExpr::Column(scope.resolve(reference)?),
        Expr::Slot(position) => {
            scope.column(*position)?;
            PhysExpr::Column(*position)
        }
        Expr::Literal(v) => PhysExpr::Literal(v.clone()),
        Expr::Binary { left, op, right } if op.is_comparison() => {
            let (left, right) = align_comparison(left, right, scope, lower)?;
            PhysExpr::binary(left, *op, right)
        }
        Expr::Binary { left, op, right } => {
            PhysExpr::binary(bind(left, scope)?, *op, bind(right, scope)?)
        }
        Expr::Not(inner) => PhysExpr::Not(Box::new(bind(inner, scope)?)),
        Expr::IsNull(inner) => PhysExpr::IsNull(Box::new(bind(inner, scope)?)),
        Expr::Like {
            expr,
            pattern,
            negated,
            case_insensitive,
        } => PhysExpr::Like {
            expr: Box::new(bind(expr, scope)?),
            pattern: Box::new(bind(pattern, scope)?),
            negated: *negated,
            case_insensitive: *case_insensitive,
        },
        Expr::Case {
            operand,
            branches,
            else_result,
        } => PhysExpr::Case {
            operand: operand
                .as_ref()
                .map(|o| bind(o, scope).map(Box::new))
                .transpose()?,
            branches: branches
                .iter()
                .map(|(when, then)| Ok((bind(when, scope)?, bind(then, scope)?)))
                .collect::<Result<_, FerriteError>>()?,
            else_result: else_result
                .as_ref()
                .map(|e| bind(e, scope).map(Box::new))
                .transpose()?,
        },
        Expr::Cast { expr, data_type } => PhysExpr::Cast {
            expr: Box::new(bind(expr, scope)?),
            data_type: *data_type,
        },
        Expr::Function { func, args } => {
            let args = args
                .iter()
                .map(|arg| bind(arg, scope))
                .collect::<Result<Vec<_>, FerriteError>>()?;
            match fold_temporal(*func, &args)? {
                Some(folded) => PhysExpr::Literal(folded),
                None => PhysExpr::Function { func: *func, args },
            }
        }
        Expr::InSubquery {
            expr,
            subquery,
            negated,
        } => {
            let subquery = lower(subquery)?;
            // `x IN (SELECT ...)` compares one value against one column;
            // a wider subquery is a row constructor, which v1 has no
            // comparison for.
            let width = subquery.output_schema().map_or(0, |s| s.columns.len());
            if width != 1 {
                return Err(FerriteError::Plan(format!(
                    "the subquery of IN must select exactly one column, not {width}"
                )));
            }
            PhysExpr::InSubquery {
                expr: Box::new(bind(expr, scope)?),
                subquery: Box::new(subquery),
                negated: *negated,
            }
        }
    })
}

/// Evaluate `datetime`/`date` over literal arguments once, at plan time.
///
/// `datetime('now')` is constant for the duration of a statement in
/// SQLite, and PawChat relies on that: a `WHERE created_at >=
/// datetime('now', '-30 days')` evaluated per row could straddle a second
/// boundary mid-scan and admit rows inconsistently. Folding here gives the
/// whole statement one reading of the clock.
///
/// Only these two functions are folded. `randomblob` is deliberately left
/// alone — SQLite re-rolls it per row, and folding would hand every row
/// the same bytes.
fn fold_temporal(func: ScalarFunc, args: &[PhysExpr]) -> Result<Option<Value>, FerriteError> {
    let with_time = match func {
        ScalarFunc::Datetime => true,
        ScalarFunc::Date => false,
        _ => return Ok(None),
    };
    let mut values = Vec::with_capacity(args.len());
    for arg in args {
        match arg {
            PhysExpr::Literal(value) => values.push(value.clone()),
            _ => return Ok(None),
        }
    }
    ferrite_common::datetime::eval_datetime(&values, with_time).map(Some)
}

fn align_comparison(
    left: &Expr,
    right: &Expr,
    scope: &Scope,
    lower: SubPlanLowerer,
) -> Result<(PhysExpr, PhysExpr), FerriteError> {
    let bind = |expr: &Expr, scope: &Scope| bind_with(expr, scope, lower);
    let target = match (left, right) {
        (Expr::Column(_), Expr::Literal(_)) => Some(infer(left, scope)?.0),
        (Expr::Literal(_), Expr::Column(_)) => Some(infer(right, scope)?.0),
        _ => None,
    };
    let (left, right) = (bind(left, scope)?, bind(right, scope)?);
    match target {
        None => Ok((left, right)),
        Some(target) => Ok((
            coerce_literal(left, target)?,
            coerce_literal(right, target)?,
        )),
    }
}

fn coerce_literal(expr: PhysExpr, target: DataType) -> Result<PhysExpr, FerriteError> {
    let PhysExpr::Literal(value) = expr else {
        return Ok(expr);
    };
    match crate::lower::coerce(Expr::Literal(value), target)? {
        Expr::Literal(coerced) => Ok(PhysExpr::Literal(coerced)),
        other => unreachable!("coerce maps a literal to a literal, got {other:?}"),
    }
}

/// The type and nullability an expression produces over `scope`.
///
/// This is the whole of Ferrite v1's type inference: enough to give a
/// projection's output columns an OID for the wire, and to type an
/// aggregate's result. It is deliberately structural — no function
/// overload resolution, no implicit casts beyond numeric promotion.
pub fn infer(expr: &Expr, scope: &Scope) -> Result<(DataType, bool), FerriteError> {
    Ok(match expr {
        Expr::Column(reference) => {
            let column = scope.column(scope.resolve(reference)?)?;
            (column.data_type, column.nullable)
        }
        Expr::Slot(position) => {
            let column = scope.column(*position)?;
            (column.data_type, column.nullable)
        }
        // An untyped `NULL` has to be given some type for the wire; `Text`
        // is what PostgreSQL falls back to as well.
        Expr::Literal(value) => (value.data_type().unwrap_or(DataType::Text), value.is_null()),
        Expr::Not(_) | Expr::IsNull(_) | Expr::Like { .. } | Expr::InSubquery { .. } => {
            (DataType::Boolean, true)
        }
        Expr::Cast { data_type, .. } => (*data_type, true),
        // `coalesce` and `nocase` take the type of their first argument;
        // for the rest the argument's type is irrelevant, so inferring it
        // is only ever a way to reject an unresolvable column reference.
        Expr::Function { func, args } => {
            let inferred = args
                .iter()
                .map(|arg| infer(arg, scope))
                .collect::<Result<Vec<_>, _>>()?;
            let first = inferred.first().map_or(DataType::Text, |(t, _)| *t);
            // `coalesce` is null only when every argument is; every other
            // function propagates a null argument to its result.
            let nullable = match func.is_null_preserving() {
                true => inferred.iter().any(|(_, n)| *n),
                false => inferred.iter().all(|(_, n)| *n),
            };
            (func.result_type(first), nullable)
        }
        // The branch results decide the type; a `CASE` with no `ELSE`
        // yields null when nothing matches, so it is always nullable.
        Expr::Case {
            branches,
            else_result,
            ..
        } => {
            let data_type = match branches.first() {
                Some((_, then)) => infer(then, scope)?.0,
                None => DataType::Text,
            };
            let exhaustive = else_result
                .as_ref()
                .map(|e| infer(e, scope).map(|(_, n)| !n))
                .transpose()?
                .unwrap_or(false);
            let any_branch_nullable = branches
                .iter()
                .map(|(_, then)| infer(then, scope).map(|(_, n)| n))
                .collect::<Result<Vec<_>, _>>()?
                .into_iter()
                .any(|n| n);
            (data_type, !exhaustive || any_branch_nullable)
        }
        Expr::Binary { left, op, right } if op.is_arithmetic() => {
            let (left, left_null) = infer(left, scope)?;
            let (right, right_null) = infer(right, scope)?;
            let nullable = left_null || right_null;
            match op {
                BinaryOp::Concat => (DataType::Text, nullable),
                _ => (numeric_result(left, right, *op)?, nullable),
            }
        }
        Expr::Binary { .. } => (DataType::Boolean, true),
    })
}

fn numeric_result(left: DataType, right: DataType, op: BinaryOp) -> Result<DataType, FerriteError> {
    let rank = |t: DataType| match t {
        DataType::Int4 => Some(0),
        DataType::Int8 => Some(1),
        DataType::Float8 => Some(2),
        _ => None,
    };
    match (rank(left), rank(right)) {
        (Some(a), Some(b)) if a >= b => Ok(left),
        (Some(_), Some(_)) => Ok(right),
        _ => Err(FerriteError::Plan(format!(
            "{:?} is not defined for {left:?} and {right:?}",
            op
        ))),
    }
}

/// The column an aggregate produces. Mirrors what `ferrite-exec` computes,
/// so the wire type always matches the value actually sent.
pub fn aggregate_column(
    func: AggregateFunc,
    arg: Option<(DataType, bool)>,
) -> Result<ColumnDef, FerriteError> {
    let (data_type, nullable) = match (func, arg) {
        (AggregateFunc::Count, _) => (DataType::Int8, false),
        (AggregateFunc::Avg, Some((DataType::Int4 | DataType::Int8 | DataType::Float8, _))) => {
            (DataType::Float8, true)
        }
        (AggregateFunc::Sum, Some((DataType::Int4 | DataType::Int8, _))) => (DataType::Int8, true),
        (AggregateFunc::Sum, Some((DataType::Float8, _))) => (DataType::Float8, true),
        (AggregateFunc::Min | AggregateFunc::Max, Some((data_type, _))) => (data_type, true),
        (func, arg) => {
            return Err(FerriteError::Plan(format!(
                "{}() is not defined for {arg:?}",
                func.name()
            )))
        }
    };
    Ok(ColumnDef::new(func.name(), data_type, nullable))
}

/// The row an `Aggregate` node produces: the group keys in order, then one
/// column per aggregate. Computed identically when the plan is built and
/// when it is lowered, so what sits above the aggregate and what the
/// aggregate emits can never disagree.
pub fn aggregate_schema(
    group_by: &[Expr],
    aggregates: &[crate::expr::AggregateCall],
    input: &Scope,
) -> Result<Schema, FerriteError> {
    let mut columns = Vec::with_capacity(group_by.len() + aggregates.len());
    for key in group_by {
        let (data_type, nullable) = infer(key, input)?;
        columns.push(ColumnDef::new(
            match key {
                Expr::Column(reference) => reference.name.clone(),
                _ => "?column?".to_string(),
            },
            data_type,
            nullable,
        ));
    }
    for call in aggregates {
        let arg = call.arg.as_ref().map(|e| infer(e, input)).transpose()?;
        columns.push(aggregate_column(call.func, arg)?);
    }
    Ok(Schema { columns })
}

/// Executable plan. Every node produces a stream of rows; `Insert`,
/// `Update`, `Delete` and `CallProcedure` are statement roots and produce a
/// count or a value instead.
#[derive(Debug, Clone, PartialEq)]
pub enum PhysicalPlan {
    /// Full table scan, optionally rejecting rows as they are read.
    SeqScan {
        table: TableId,
        table_name: String,
        schema: Schema,
        filter: Option<PhysExpr>,
    },
    /// Equality lookup through a single-column B-tree index. `residual`
    /// holds the conjuncts the index could not satisfy.
    IndexScan {
        table: TableId,
        table_name: String,
        schema: Schema,
        index: String,
        column: usize,
        key: PhysExpr,
        residual: Option<PhysExpr>,
    },
    /// The only join algorithm in v1. `docs/architecture.md` cuts the cost
    /// model, and without statistics there is nothing to choose a hash or
    /// merge join *with*; a nested loop is correct for every join type and
    /// needs no build phase.
    NestedLoopJoin {
        left: Box<PhysicalPlan>,
        right: Box<PhysicalPlan>,
        join_type: JoinType,
        predicate: Option<PhysExpr>,
        output: Schema,
    },
    Filter {
        input: Box<PhysicalPlan>,
        predicate: PhysExpr,
    },
    Aggregate {
        input: Box<PhysicalPlan>,
        group_by: Vec<PhysExpr>,
        aggregates: Vec<PhysAggregate>,
        output: Schema,
    },
    Projection {
        input: Box<PhysicalPlan>,
        exprs: Vec<PhysExpr>,
        output: Schema,
    },
    Sort {
        input: Box<PhysicalPlan>,
        keys: Vec<PhysSortKey>,
    },
    Distinct {
        input: Box<PhysicalPlan>,
    },
    Limit {
        input: Box<PhysicalPlan>,
        count: Option<u64>,
        offset: u64,
    },
    Insert {
        table: TableId,
        table_name: String,
        schema: Schema,
        /// One `PhysExpr` per table column, in schema order.
        rows: Vec<Vec<PhysExpr>>,
        on_conflict: Option<PhysOnConflict>,
    },
    Update {
        table: TableId,
        table_name: String,
        schema: Schema,
        /// Must preserve row identity, so only scan/filter nodes are valid
        /// here — the planner never puts a projection underneath.
        source: Box<PhysicalPlan>,
        assignments: Vec<(usize, PhysExpr)>,
    },
    Delete {
        table: TableId,
        table_name: String,
        schema: Schema,
        source: Box<PhysicalPlan>,
    },
    CallProcedure {
        name: String,
        args: Vec<PhysExpr>,
    },
}

impl PhysicalPlan {
    /// Shape of the rows this node produces, or `None` for statement roots
    /// that produce a count/value.
    pub fn output_schema(&self) -> Option<&Schema> {
        match self {
            PhysicalPlan::SeqScan { schema, .. } | PhysicalPlan::IndexScan { schema, .. } => {
                Some(schema)
            }
            PhysicalPlan::NestedLoopJoin { output, .. }
            | PhysicalPlan::Aggregate { output, .. }
            | PhysicalPlan::Projection { output, .. } => Some(output),
            PhysicalPlan::Filter { input, .. }
            | PhysicalPlan::Sort { input, .. }
            | PhysicalPlan::Distinct { input }
            | PhysicalPlan::Limit { input, .. } => input.output_schema(),
            PhysicalPlan::Insert { .. }
            | PhysicalPlan::Update { .. }
            | PhysicalPlan::Delete { .. }
            | PhysicalPlan::CallProcedure { .. } => None,
        }
    }

    /// `true` when this node is a scan or a chain of filters over one —
    /// i.e. when it still carries `RowId`s and can therefore feed an
    /// `Update`/`Delete`.
    pub fn preserves_row_identity(&self) -> bool {
        match self {
            PhysicalPlan::SeqScan { .. } | PhysicalPlan::IndexScan { .. } => true,
            PhysicalPlan::Filter { input, .. } => input.preserves_row_identity(),
            _ => false,
        }
    }
}
