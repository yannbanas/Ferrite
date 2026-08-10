//! The planner's expression IR.
//!
//! `ferrite-sql`'s [`Expr`](ferrite_sql::ast::Expr) covers the whole parsed
//! dialect — subqueries, `CASE`, function calls. What the v1 executor can
//! actually evaluate is narrower, so [`crate::lower`] projects the parsed
//! expression onto this type and rejects the rest with a
//! [`FerriteError::Plan`](ferrite_common::FerriteError::Plan). Keeping the
//! two apart means the physical plan and the executor never have to carry a
//! variant nothing can run.

use std::fmt;

use ferrite_common::Value;

/// A column reference, optionally qualified by a relation name or alias.
/// The qualifier only starts to matter once a statement has more than one
/// relation in scope, where `users.id` and `posts.id` are different
/// columns.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ColumnRef {
    pub qualifier: Option<String>,
    pub name: String,
}

impl ColumnRef {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            qualifier: None,
            name: name.into(),
        }
    }

    pub fn qualified(qualifier: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            qualifier: Some(qualifier.into()),
            name: name.into(),
        }
    }
}

impl fmt::Display for ColumnRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.qualifier {
            Some(qualifier) => write!(f, "{qualifier}.{}", self.name),
            None => f.write_str(&self.name),
        }
    }
}

/// Scalar expression over column references and literals.
#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Column(ColumnRef),
    /// A column of the input row already resolved to a position. Produced
    /// when a projection, `HAVING` or `ORDER BY` is rewritten over an
    /// aggregate's output, where the input names no longer exist.
    Slot(usize),
    Literal(Value),
    Binary {
        left: Box<Expr>,
        op: BinaryOp,
        right: Box<Expr>,
    },
    Not(Box<Expr>),
    IsNull(Box<Expr>),
    Like {
        expr: Box<Expr>,
        pattern: Box<Expr>,
        negated: bool,
    },
}

impl Expr {
    pub fn column(name: impl Into<String>) -> Self {
        Expr::Column(ColumnRef::new(name))
    }

    pub fn qualified_column(qualifier: impl Into<String>, name: impl Into<String>) -> Self {
        Expr::Column(ColumnRef::qualified(qualifier, name))
    }

    pub fn binary(left: Expr, op: BinaryOp, right: Expr) -> Self {
        Expr::Binary {
            left: Box::new(left),
            op,
            right: Box::new(right),
        }
    }

    pub fn eq(left: Expr, right: Expr) -> Self {
        Expr::binary(left, BinaryOp::Eq, right)
    }

    pub fn and(left: Expr, right: Expr) -> Self {
        Expr::binary(left, BinaryOp::And, right)
    }

    /// Every column reference mentioned anywhere in this expression.
    pub fn referenced_columns(&self) -> Vec<&ColumnRef> {
        let mut out = Vec::new();
        self.collect_columns(&mut out);
        out
    }

    fn collect_columns<'a>(&'a self, out: &mut Vec<&'a ColumnRef>) {
        match self {
            Expr::Column(reference) => out.push(reference),
            Expr::Slot(_) | Expr::Literal(_) => {}
            Expr::Binary { left, right, .. } => {
                left.collect_columns(out);
                right.collect_columns(out);
            }
            Expr::Not(inner) | Expr::IsNull(inner) => inner.collect_columns(out),
            Expr::Like { expr, pattern, .. } => {
                expr.collect_columns(out);
                pattern.collect_columns(out);
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryOp {
    Eq,
    NotEq,
    Lt,
    LtEq,
    Gt,
    GtEq,
    And,
    Or,
    Plus,
    Minus,
    Multiply,
    Divide,
    Modulo,
    Concat,
}

impl BinaryOp {
    pub fn is_comparison(self) -> bool {
        matches!(
            self,
            BinaryOp::Eq
                | BinaryOp::NotEq
                | BinaryOp::Lt
                | BinaryOp::LtEq
                | BinaryOp::Gt
                | BinaryOp::GtEq
        )
    }

    /// `true` for the operators that produce a value rather than a truth
    /// value, i.e. everything the three-valued logic does not apply to.
    pub fn is_arithmetic(self) -> bool {
        matches!(
            self,
            BinaryOp::Plus
                | BinaryOp::Minus
                | BinaryOp::Multiply
                | BinaryOp::Divide
                | BinaryOp::Modulo
                | BinaryOp::Concat
        )
    }
}

/// The five aggregate functions Ferrite v1 recognises.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AggregateFunc {
    Count,
    Sum,
    Avg,
    Min,
    Max,
}

impl AggregateFunc {
    pub fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "count" => AggregateFunc::Count,
            "sum" => AggregateFunc::Sum,
            "avg" => AggregateFunc::Avg,
            "min" => AggregateFunc::Min,
            "max" => AggregateFunc::Max,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            AggregateFunc::Count => "count",
            AggregateFunc::Sum => "sum",
            AggregateFunc::Avg => "avg",
            AggregateFunc::Min => "min",
            AggregateFunc::Max => "max",
        }
    }
}

/// One aggregate in a `GROUP BY`. `arg` is `None` for `count(*)`, which
/// counts rows rather than non-null values.
#[derive(Debug, Clone, PartialEq)]
pub struct AggregateCall {
    pub func: AggregateFunc,
    pub arg: Option<Expr>,
    pub distinct: bool,
}
