//! Owned by Agent 3, alongside `ferrite-exec` and `ferrite-proc`. Turns a
//! SQL AST into a logical plan, then a physical plan, using a small set of
//! fixed rules (predicate pushdown, index-vs-scan choice by simple
//! heuristic) rather than a statistics-driven cost model — see
//! `docs/architecture.md` for why that's the v1 scope.
//!
//! Pipeline:
//!
//! ```text
//! Statement -> build_logical -> LogicalPlan -> optimize -> LogicalPlan -> to_physical -> PhysicalPlan
//! ```
//!
//! The AST in [`ast`] is **provisional** and will be replaced by
//! `ferrite-sql`'s at integration; only [`Planner::build_logical`] depends
//! on its shape.
//!
//! ```
//! use ferrite_planner::ast::{Expr, SelectItem, SelectStmt, Statement, TableRef};
//!
//! let stmt = Statement::Select(SelectStmt {
//!     projection: vec![SelectItem::Wildcard],
//!     from: TableRef::new("users"),
//!     filter: Some(Expr::eq(
//!         Expr::column("id"),
//!         Expr::Literal(ferrite_common::Value::Int8(1)),
//!     )),
//!     limit: None,
//! });
//! assert!(matches!(stmt, Statement::Select(_)));
//! ```

pub mod ast;
pub mod index;
pub mod logical;
pub mod physical;
pub mod planner;
pub mod rules;

pub use index::{IndexCatalog, IndexInfo, NoIndexes};
pub use logical::{LogicalPlan, ProjectionItem, TableSource};
pub use physical::{bind, PhysExpr, PhysicalPlan};
pub use planner::Planner;
pub use rules::optimize;
