//! The planner's expression IR.
//!
//! `ferrite-sql`'s [`Expr`](ferrite_sql::ast::Expr) covers the whole parsed
//! dialect — subqueries, `CASE`, function calls, arithmetic. What the v1
//! executor can actually evaluate is much narrower, so [`crate::lower`]
//! projects the parsed expression onto this type and rejects the rest with
//! a [`FerriteError::Plan`](ferrite_common::FerriteError::Plan). Keeping
//! the two apart means the physical plan and the executor never have to
//! carry a variant nothing can run.

use ferrite_common::Value;

/// Scalar expression over column references and literals.
#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Column(String),
    Literal(Value),
    Binary {
        left: Box<Expr>,
        op: BinaryOp,
        right: Box<Expr>,
    },
    Not(Box<Expr>),
    IsNull(Box<Expr>),
}

impl Expr {
    pub fn column(name: impl Into<String>) -> Self {
        Expr::Column(name.into())
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

    /// Every column name mentioned anywhere in this expression.
    pub fn referenced_columns(&self) -> Vec<&str> {
        let mut out = Vec::new();
        self.collect_columns(&mut out);
        out
    }

    fn collect_columns<'a>(&'a self, out: &mut Vec<&'a str>) {
        match self {
            Expr::Column(name) => out.push(name.as_str()),
            Expr::Literal(_) => {}
            Expr::Binary { left, right, .. } => {
                left.collect_columns(out);
                right.collect_columns(out);
            }
            Expr::Not(inner) | Expr::IsNull(inner) => inner.collect_columns(out),
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
}

impl BinaryOp {
    pub fn is_comparison(self) -> bool {
        !matches!(self, BinaryOp::And | BinaryOp::Or)
    }
}
