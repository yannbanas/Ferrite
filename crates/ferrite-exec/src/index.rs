//! Runtime side of index access.
//!
//! `ferrite-planner` decides *whether* to use an index
//! (`ferrite_planner::IndexCatalog`); this trait is how the executor
//! actually probes one. It is separate from `ferrite_common::StorageEngine`
//! because the v0 shared contract has no index vocabulary at all — see the
//! crate README for the change that would fold it in.

use ferrite_common::{FerriteError, RowId, TableId, TxnId, Value};

/// Equality probe into a single-column B-tree index.
///
/// Returns the `RowId`s whose indexed column equals `key`, visible under
/// `txn`. A unique index returns at most one.
pub trait IndexProvider: Send + Sync {
    fn lookup(
        &self,
        txn: TxnId,
        table: TableId,
        index: &str,
        key: &Value,
    ) -> Result<Vec<RowId>, FerriteError>;
}
