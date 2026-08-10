//! Logical plan: relational algebra over resolved tables, still expressed
//! in terms of column *names* (binding to positions happens when lowering
//! to the physical plan).

use ferrite_common::{Schema, TableId};

use crate::expr::{AggregateCall, BinaryOp, Expr};
use crate::scope::Scope;

/// Which table a scan reads, resolved against the catalog once so later
/// stages never touch name resolution again.
#[derive(Debug, Clone, PartialEq)]
pub struct TableSource {
    pub id: TableId,
    pub name: String,
    pub alias: Option<String>,
    pub schema: Schema,
}

impl TableSource {
    /// The names a column reference may use to reach this relation. An
    /// alias does not hide the table name in Ferrite v1; both work.
    pub fn qualifiers(&self) -> Vec<String> {
        let mut out = vec![self.name.clone()];
        out.extend(self.alias.clone());
        out
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinType {
    Inner,
    Left,
    Right,
    Full,
    Cross,
}

impl JoinType {
    /// `true` when a row from this side survives without a match on the
    /// other, and therefore may be padded with nulls.
    pub fn preserves_left(self) -> bool {
        matches!(self, JoinType::Left | JoinType::Full)
    }

    pub fn preserves_right(self) -> bool {
        matches!(self, JoinType::Right | JoinType::Full)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SortKey {
    pub expr: Expr,
    pub asc: bool,
    pub nulls_first: bool,
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
    Join {
        left: Box<LogicalPlan>,
        right: Box<LogicalPlan>,
        join_type: JoinType,
        on: Option<Expr>,
    },
    Filter {
        input: Box<LogicalPlan>,
        predicate: Expr,
    },
    /// One output row per distinct `group_by` tuple, or exactly one row
    /// when `group_by` is empty. The output row is the group keys in
    /// order, then the aggregate results.
    Aggregate {
        input: Box<LogicalPlan>,
        group_by: Vec<Expr>,
        aggregates: Vec<AggregateCall>,
    },
    Projection {
        input: Box<LogicalPlan>,
        items: Vec<ProjectionItem>,
    },
    Sort {
        input: Box<LogicalPlan>,
        keys: Vec<SortKey>,
    },
    Distinct {
        input: Box<LogicalPlan>,
    },
    Limit {
        input: Box<LogicalPlan>,
        count: Option<u64>,
        offset: u64,
    },
    Insert {
        source: TableSource,
        /// One entry per table column, in schema order.
        rows: Vec<Vec<Expr>>,
        on_conflict: Option<OnConflict>,
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
            LogicalPlan::Join { left, right, .. } => vec![left.as_ref(), right.as_ref()],
            LogicalPlan::Filter { input, .. }
            | LogicalPlan::Aggregate { input, .. }
            | LogicalPlan::Projection { input, .. }
            | LogicalPlan::Sort { input, .. }
            | LogicalPlan::Distinct { input, .. }
            | LogicalPlan::Limit { input, .. }
            | LogicalPlan::Update { input, .. }
            | LogicalPlan::Delete { input, .. } => vec![input.as_ref()],
        }
    }

    /// The names a predicate sitting directly above this node may use.
    ///
    /// `None` for the nodes that reshape their output — a projection or an
    /// aggregate — and for statement roots. The rule engine only ever asks
    /// this of the `FROM` tree, which is scans and joins, and treats
    /// everything else as a barrier anyway.
    pub fn scope(&self) -> Option<Scope> {
        match self {
            LogicalPlan::Scan { source, .. } => {
                Some(Scope::for_relation(&source.schema, &source.qualifiers()))
            }
            LogicalPlan::Join {
                left,
                right,
                join_type,
                ..
            } => {
                let mut left = left.scope()?;
                let mut right = right.scope()?;
                if join_type.preserves_right() {
                    left = left.nullable();
                }
                if join_type.preserves_left() {
                    right = right.nullable();
                }
                Some(Scope::concat(left, right))
            }
            LogicalPlan::Filter { input, .. }
            | LogicalPlan::Sort { input, .. }
            | LogicalPlan::Distinct { input }
            | LogicalPlan::Limit { input, .. } => input.scope(),
            _ => None,
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

/// What to do with a row that collides with an existing one on
/// [`OnConflict::target`].
#[derive(Debug, Clone, PartialEq)]
pub struct OnConflict {
    /// Column positions, in schema order, that decide whether two rows
    /// collide.
    pub target: Vec<usize>,
    pub action: ConflictAction,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ConflictAction {
    Nothing,
    /// Assignments and the optional `WHERE` are expressed over a row made
    /// of the existing row followed by the row the insert would have
    /// written, so `excluded.col` is just a column reference into the
    /// second half — the same shape a join produces.
    Update {
        assignments: Vec<(usize, Expr)>,
        selection: Option<Expr>,
    },
}
