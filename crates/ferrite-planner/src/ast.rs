//! Provisional AST — **to be replaced by `ferrite-sql`'s AST at integration**.
//!
//! `ferrite-sql` (Agent 2) is being written in parallel and does not yet
//! expose a usable AST, so the planner defines the smallest shape it needs
//! to be developed and tested against. Nothing outside this module encodes
//! the AST's identity: swapping it out means rewriting
//! [`crate::planner::Planner::build_logical`] and deleting this file.
//!
//! Scope is deliberately narrow: single-table `SELECT`/`INSERT`/`UPDATE`/
//! `DELETE` plus an explicit `CALL`. No joins, no subqueries, no grouping,
//! no ordering — the planner rules under test (predicate pushdown,
//! index-vs-scan) do not need them.

use ferrite_common::Value;

/// A single parsed statement.
#[derive(Debug, Clone, PartialEq)]
pub enum Statement {
    Select(SelectStmt),
    Insert(InsertStmt),
    Update(UpdateStmt),
    Delete(DeleteStmt),
    Call(CallStmt),
}

/// A schema-qualified table name. `schema` defaults to `public` when absent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableRef {
    pub schema: Option<String>,
    pub name: String,
}

impl TableRef {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            schema: None,
            name: name.into(),
        }
    }

    pub fn qualified(schema: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            schema: Some(schema.into()),
            name: name.into(),
        }
    }

    /// Schema name to resolve against, applying the `public` default.
    pub fn schema_or_default(&self) -> &str {
        self.schema.as_deref().unwrap_or("public")
    }
}

/// One element of a `SELECT` list.
#[derive(Debug, Clone, PartialEq)]
pub enum SelectItem {
    /// `SELECT *`
    Wildcard,
    Expr {
        expr: Expr,
        alias: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct SelectStmt {
    pub projection: Vec<SelectItem>,
    pub from: TableRef,
    pub filter: Option<Expr>,
    pub limit: Option<u64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct InsertStmt {
    pub table: TableRef,
    /// Empty means "every column, in schema order".
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Expr>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Assignment {
    pub column: String,
    pub value: Expr,
}

#[derive(Debug, Clone, PartialEq)]
pub struct UpdateStmt {
    pub table: TableRef,
    pub assignments: Vec<Assignment>,
    pub filter: Option<Expr>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DeleteStmt {
    pub table: TableRef,
    pub filter: Option<Expr>,
}

/// Explicit invocation of a stored procedure registered in `ferrite-proc`.
#[derive(Debug, Clone, PartialEq)]
pub struct CallStmt {
    pub name: String,
    pub args: Vec<Expr>,
}

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
