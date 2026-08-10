//! Owned by Agent 2, alongside `ferrite-sql`. Implements
//! [`ferrite_common::Catalog`] — table/column/type metadata stored as
//! regular tables via [`ferrite_common::StorageEngine`], not a separate
//! metadata file format.
//!
//! See this crate's README for the layout of the catalog tables and the
//! transaction semantics the [`Catalog`](ferrite_common::Catalog) trait
//! forces on this implementation.
//!
//! ```
//! use std::sync::Arc;
//!
//! use ferrite_catalog::{memory::MemoryStorage, SystemCatalog};
//! use ferrite_common::{Catalog, ColumnDef, DataType, Schema};
//!
//! let storage = Arc::new(MemoryStorage::new());
//! let catalog = SystemCatalog::bootstrap(storage).unwrap();
//!
//! let id = catalog
//!     .create_table(
//!         "public",
//!         "users",
//!         Schema {
//!             columns: vec![ColumnDef {
//!                 name: "id".into(),
//!                 data_type: DataType::Uuid,
//!                 nullable: false,
//!             }],
//!         },
//!     )
//!     .unwrap();
//!
//! assert_eq!(catalog.table_id("public", "users").unwrap(), Some(id));
//! assert_eq!(catalog.table_schema(id).unwrap().columns.len(), 1);
//! ```

#[cfg(any(test, feature = "test-util"))]
pub mod memory;

mod system;

pub use system::{
    SystemCatalog, CATALOG_SCHEMA, COLUMNS_TABLE_ID, DEFAULT_SCHEMA, FIRST_USER_TABLE_ID,
    TABLES_TABLE_ID,
};
