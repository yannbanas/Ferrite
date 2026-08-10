//! Shared vocabulary for every Ferrite crate.
//!
//! This crate defines no behavior — only the value model, schema model,
//! identity model, and the `StorageEngine`/`Catalog` trait contracts that
//! the storage, catalog, planner/executor and protocol crates are built
//! against. Treat the traits here as a v0 contract: crates implementing
//! them may propose changes, but changes must be coordinated here first
//! since every other crate depends on this one compiling stably.

mod catalog;
mod error;
mod identity;
mod schema;
mod storage;
mod txn;
mod value;

pub use catalog::Catalog;
pub use error::FerriteError;
pub use identity::{Identity, Permission, Role};
pub use schema::{ColumnDef, Schema, TableId};
pub use storage::{RowId, ScanIter, StorageEngine};
pub use txn::{Snapshot, TxnId};
pub use value::{DataType, Row, Value};
