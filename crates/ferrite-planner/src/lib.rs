//! Owned by Agent 3, alongside `ferrite-exec` and `ferrite-proc`. Turns a
//! `ferrite-sql` AST into a logical plan, then a physical plan, using a
//! small set of fixed rules (predicate pushdown, index-vs-scan choice by
//! simple heuristic) rather than a statistics-driven cost model — see
//! `docs/architecture.md` for why that's the v1 scope.
//!
//! Pipeline:
//!
//! ```text
//! Statement -> build_logical -> LogicalPlan -> optimize -> LogicalPlan -> to_physical -> PhysicalPlan
//! ```
//!
//! `ferrite-sql` parses more than `ferrite-exec` can run. Everything
//! outside the executable subset — subqueries, set operations, DDL,
//! transaction control — leaves
//! [`Planner::build_logical`] as a
//! [`FerriteError::Plan`](ferrite_common::FerriteError::Plan) rather than
//! as a plan that would be silently wrong; the `lower` module holds every
//! one of those rejections.
//!
//! ```
//! # use ferrite_common::FerriteError;
//! use ferrite_planner::Planner;
//!
//! # fn plan(catalog: &dyn ferrite_common::Catalog, indexes: &dyn ferrite_common::IndexCatalog)
//! # -> Result<(), FerriteError> {
//! let statement = ferrite_sql::parse_statement("SELECT id FROM users WHERE id = 1")?;
//! let plan = Planner::new(catalog, indexes).plan(&statement)?;
//! # let _ = plan;
//! # Ok(())
//! # }
//! ```

pub mod expr;
pub mod logical;
mod lower;
pub mod physical;
pub mod planner;
pub mod rules;
pub mod scalar;
pub mod scope;

pub use expr::{AggregateCall, AggregateFunc, BinaryOp, ColumnRef, Expr};
pub use logical::{
    ConflictAction, JoinType, LogicalPlan, OnConflict, ProjectionItem, SortKey, TableSource,
};
pub use lower::typecheck_defaults;
pub use physical::{
    bind, bind_with, PhysAggregate, PhysConflictAction, PhysExpr, PhysOnConflict, PhysSortKey,
    PhysicalPlan,
};
pub use planner::{Planner, DEFAULT_NAMESPACE};
pub use rules::optimize;
pub use scalar::ScalarFunc;
pub use scope::{Scope, ScopeColumn};
