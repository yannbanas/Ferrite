use crate::{FerriteError, Schema, TableId};

/// System catalog contract: name resolution and schema lookup. The
/// catalog owns table/column/type metadata; `ferrite-storage` owns row
/// data. `ferrite-catalog` implements this on top of a `StorageEngine`
/// (the catalog is itself stored as regular tables — no separate
/// metadata file format).
pub trait Catalog: Send + Sync {
    fn table_id(&self, schema: &str, name: &str) -> Result<Option<TableId>, FerriteError>;
    fn table_schema(&self, table: TableId) -> Result<Schema, FerriteError>;
    fn create_table(
        &self,
        schema: &str,
        name: &str,
        columns: Schema,
    ) -> Result<TableId, FerriteError>;
    fn drop_table(&self, table: TableId) -> Result<(), FerriteError>;
    fn list_tables(&self, schema: &str) -> Result<Vec<(TableId, String)>, FerriteError>;
}
