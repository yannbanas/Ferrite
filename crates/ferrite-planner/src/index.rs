//! Index metadata seen by the planner.
//!
//! `ferrite_common::Catalog` has no index vocabulary in the v0 contract, so
//! the planner declares the narrow view it needs here rather than widening
//! the shared trait unilaterally. `ferrite-catalog` is expected to
//! implement [`IndexCatalog`] once indexes exist there; until then
//! [`NoIndexes`] gives a valid "nothing is indexed" answer that always
//! degrades to a sequential scan.

use ferrite_common::TableId;

/// A B-tree index over exactly one column. Ferrite v1 has no multi-column
/// and no non-B-tree indexes (see `docs/architecture.md`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexInfo {
    pub name: String,
    pub table: TableId,
    /// Position of the indexed column in the table's `Schema`.
    pub column: usize,
    pub unique: bool,
}

/// Read-only view of which indexes exist, consulted while choosing an
/// access path. Returning an empty slice is always correct — it only costs
/// a sequential scan.
pub trait IndexCatalog {
    fn indexes(&self, table: TableId) -> Vec<IndexInfo>;
}

/// `IndexCatalog` that reports no indexes at all.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoIndexes;

impl IndexCatalog for NoIndexes {
    fn indexes(&self, _table: TableId) -> Vec<IndexInfo> {
        Vec::new()
    }
}
