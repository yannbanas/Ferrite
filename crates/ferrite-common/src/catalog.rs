use crate::{FerriteError, Schema, TableId};

/// System catalog contract: name resolution and schema lookup. The
/// catalog owns table/column/type metadata; `ferrite-storage` owns row
/// data. `ferrite-catalog` implements this on top of a `StorageEngine`
/// (the catalog is itself stored as regular tables — no separate
/// metadata file format).
///
/// `namespace` is a schema *name* (Postgres sense — a namespace tables
/// live in, e.g. `"public"`), not a `ferrite_common::Schema` (a column
/// list). The parameter used to be called `schema` in both trait methods
/// and the `Schema` type below, which read as the same thing twice in one
/// signature; it isn't.
pub trait Catalog: Send + Sync {
    fn table_id(&self, namespace: &str, name: &str) -> Result<Option<TableId>, FerriteError>;
    fn table_schema(&self, table: TableId) -> Result<Schema, FerriteError>;
    fn create_table(
        &self,
        namespace: &str,
        name: &str,
        columns: Schema,
    ) -> Result<TableId, FerriteError>;
    fn drop_table(&self, table: TableId) -> Result<(), FerriteError>;
    fn list_tables(&self, namespace: &str) -> Result<Vec<(TableId, String)>, FerriteError>;
}

/// Identifier of an index. Indexes are allocated from the same id space as
/// tables, so an index id can later become a storage object id without
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

/// Index metadata: definitions only. This trait does not let anything
/// probe an index for row ids — that's `ferrite-exec`'s `IndexProvider`,
/// which storage backs once secondary indexes have real on-disk structure
/// (not yet true as of the v1 integration: `ferrite-storage` has no
/// secondary-index support yet, only the primary per-table B-tree, so
/// `CREATE INDEX` today only records metadata here — see
/// `docs/architecture.md` §Reste à faire).
///
/// Index metadata belongs in the catalog rather than the planner or
/// executor: it is persistent schema, it must be dropped with its table,
/// and both the planner (choosing an access path) and the executor
/// (maintaining the index on write, once that exists) need the same
/// answer.
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
    /// Look an index up by name; index names are scoped to the namespace
    /// of the table they belong to.
    fn index_by_name(&self, namespace: &str, name: &str) -> Result<Option<IndexDef>, FerriteError>;
    /// Every index on `table`, sorted by name.
    fn indexes_for(&self, table: TableId) -> Result<Vec<IndexDef>, FerriteError>;
}
