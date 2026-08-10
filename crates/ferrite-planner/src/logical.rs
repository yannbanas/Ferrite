//! Logical plan: relational algebra over resolved tables, still expressed
//! in terms of column *names* (binding to positions happens when lowering
//! to the physical plan).

use ferrite_common::{Schema, TableId};

use crate::ast::{BinaryOp, Expr};

/// Which table a scan reads, resolved against the catalog once so later
/// stages never touch name resolution again.
#[derive(Debug, Clone, PartialEq)]
pub struct TableSource {
    pub id: TableId,
    pub name: String,
    pub schema: Schema,
}

#[derive(Debug, Clone, PartialEq)]
pub enum LogicalPlan {
    /// Base relation. `filter` starts empty and is filled by predicate
    /// pushdown; a filter sitting here is what lets the physical stage
    /// consider an index.
    Scan {
        source: TableSource,
        filter: Option<Expr>,
    },
    Filter {
        input: Box<LogicalPlan>,
        predicate: Expr,
    },
    Projection {
        input: Box<LogicalPlan>,
        items: Vec<ProjectionItem>,
    },
    Limit {
        input: Box<LogicalPlan>,
        count: u64,
    },
    Insert {
        source: TableSource,
        /// One entry per table column, in schema order.
        rows: Vec<Vec<Expr>>,
    },
    Update {
        source: TableSource,
        input: Box<LogicalPlan>,
        /// `(column position, new value)`, in schema order.
        assignments: Vec<(usize, Expr)>,
    },
    Delete {
        source: TableSource,
        input: Box<LogicalPlan>,
    },
    Call {
        name: String,
        args: Vec<Expr>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProjectionItem {
    pub expr: Expr,
    pub output_name: String,
}

impl LogicalPlan {
    /// Depth-first walk, root first. Used by tests and by the rule engine.
    pub fn children(&self) -> Vec<&LogicalPlan> {
        match self {
            LogicalPlan::Scan { .. } | LogicalPlan::Insert { .. } | LogicalPlan::Call { .. } => {
                Vec::new()
            }
            LogicalPlan::Filter { input, .. }
            | LogicalPlan::Projection { input, .. }
            | LogicalPlan::Limit { input, .. }
            | LogicalPlan::Update { input, .. }
            | LogicalPlan::Delete { input, .. } => vec![input.as_ref()],
        }
    }
}

/// Break `a AND b AND c` into `[a, b, c]`. Conjuncts are pushed
/// independently, so a predicate that only partly matches an index still
/// gets its matching part used.
pub fn split_conjunction(expr: Expr) -> Vec<Expr> {
    let mut out = Vec::new();
    split_into(expr, &mut out);
    out
}

fn split_into(expr: Expr, out: &mut Vec<Expr>) {
    match expr {
        Expr::Binary {
            left,
            op: BinaryOp::And,
            right,
        } => {
            split_into(*left, out);
            split_into(*right, out);
        }
        other => out.push(other),
    }
}

/// Inverse of [`split_conjunction`]. Returns `None` for an empty list.
pub fn combine_conjunction(mut preds: Vec<Expr>) -> Option<Expr> {
    if preds.is_empty() {
        return None;
    }
    let mut acc = preds.remove(0);
    for p in preds {
        acc = Expr::and(acc, p);
    }
    Some(acc)
}
