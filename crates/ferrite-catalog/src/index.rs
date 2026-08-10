use ferrite_common::{FerriteError, TableId};

/// Identifier of an index. Indexes are allocated from the same id space
/// as tables, so an index id can later become a storage object id without
/// colliding with a table's.
pub type IndexId = TableId;

/// An index as the catalog knows it. Ferrite v1 has B-tree indexes only
/// (`docs/architecture.md` cuts GiST/GIN/BRIN/hash), so there is no access
/// method field to store — adding one is the change to make when a second
/// index type appears.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexDef {
    pub id: IndexId,
    pub name: String,
    pub table: TableId,
    /// Indexed columns, in key order.
    pub columns: Vec<String>,
    pub unique: bool,
}

/// Index metadata, kept separate from [`ferrite_common::Catalog`] because
/// that trait is a frozen v0 contract this crate must not change
/// unilaterally.
///
/// Index metadata belongs in the catalog rather than in the planner: it is
/// persistent schema, it must be dropped with its table, and both the
/// planner (choosing an access path) and the executor (maintaining the
/// index on write) need the same answer. The recommendation is to promote
/// this trait into `ferrite-common` once the workspace agrees on it; until
/// then, `ferrite-planner` can depend on this crate, which the dependency
/// order in `docs/architecture.md` already allows.
pub trait IndexCatalog: Send + Sync {
    fn create_index(
        &self,
        name: &str,
        table: TableId,
        columns: &[String],
        unique: bool,
    ) -> Result<IndexId, FerriteError>;
    fn drop_index(&self, index: IndexId) -> Result<(), FerriteError>;
    fn index(&self, index: IndexId) -> Result<Option<IndexDef>, FerriteError>;
    /// Look an index up by name; index names are scoped to the schema of
    /// the table they belong to.
    fn index_by_name(&self, schema: &str, name: &str) -> Result<Option<IndexDef>, FerriteError>;
    /// Every index on `table`, sorted by name.
    fn indexes_for(&self, table: TableId) -> Result<Vec<IndexDef>, FerriteError>;
}
