//! Physical plan: an access-path decision plus expressions bound to column
//! positions, so the executor never resolves a name at runtime.

use ferrite_common::{DataType, FerriteError, Schema, TableId, Value};

use crate::expr::{BinaryOp, Expr};

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

/// Resolve column names against `schema`. Pass an empty schema for
/// contexts where column references are illegal (`INSERT ... VALUES`,
/// `CALL` arguments); the resulting `ColumnNotFound` is the correct error.
///
/// A literal compared against a column is coerced to that column's declared
/// type on the way through. The executor compares `Value` variants and an
/// index probe compares them for exact equality, so `WHERE id = 1` against
/// a `BIGINT` column has to carry an `Int8`, not the `Int4` the literal
/// parsed as — otherwise the probe silently matches nothing.
pub fn bind(expr: &Expr, schema: &Schema) -> Result<PhysExpr, FerriteError> {
    Ok(match expr {
        Expr::Column(name) => PhysExpr::Column(
            schema
                .column_index(name)
                .ok_or_else(|| FerriteError::ColumnNotFound(name.clone()))?,
        ),
        Expr::Literal(v) => PhysExpr::Literal(v.clone()),
        Expr::Binary { left, op, right } if op.is_comparison() => {
            let (left, right) = align_comparison(left, right, schema)?;
            PhysExpr::binary(left, *op, right)
        }
        Expr::Binary { left, op, right } => {
            PhysExpr::binary(bind(left, schema)?, *op, bind(right, schema)?)
        }
        Expr::Not(inner) => PhysExpr::Not(Box::new(bind(inner, schema)?)),
        Expr::IsNull(inner) => PhysExpr::IsNull(Box::new(bind(inner, schema)?)),
    })
}

fn align_comparison(
    left: &Expr,
    right: &Expr,
    schema: &Schema,
) -> Result<(PhysExpr, PhysExpr), FerriteError> {
    let target = match (left, right) {
        (Expr::Column(name), Expr::Literal(_)) | (Expr::Literal(_), Expr::Column(name)) => schema
            .column_index(name)
            .map(|position| schema.columns[position].data_type),
        _ => None,
    };
    let (left, right) = (bind(left, schema)?, bind(right, schema)?);
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
    Filter {
        input: Box<PhysicalPlan>,
        predicate: PhysExpr,
    },
    Projection {
        input: Box<PhysicalPlan>,
        exprs: Vec<PhysExpr>,
        output: Schema,
    },
    Limit {
        input: Box<PhysicalPlan>,
        count: u64,
    },
    Insert {
        table: TableId,
        table_name: String,
        schema: Schema,
        /// One `PhysExpr` per table column, in schema order.
        rows: Vec<Vec<PhysExpr>>,
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
            PhysicalPlan::Filter { input, .. } | PhysicalPlan::Limit { input, .. } => {
                input.output_schema()
            }
            PhysicalPlan::Projection { output, .. } => Some(output),
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
