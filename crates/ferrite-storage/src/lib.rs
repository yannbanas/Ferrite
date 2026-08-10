//! Ferrite's storage engine: fixed-size checksummed pages, a B+-tree per
//! table, MVCC row versions, and a physical redo journal for crash
//! recovery. It is the sole implementor of [`ferrite_common::StorageEngine`]
//! and nothing above that trait needs to know about page layout.
//!
//! ```
//! use ferrite_common::{Row, StorageEngine, Value};
//! use ferrite_storage::FerriteStorage;
//!
//! # fn main() -> Result<(), ferrite_common::FerriteError> {
//! let dir = std::env::temp_dir().join("ferrite-doctest-overview");
//! # let _ = std::fs::remove_dir_all(&dir);
//! let storage = FerriteStorage::open(&dir)?;
//!
//! let txn = storage.begin()?;
//! storage.create_table(txn, 1)?;
//! let row = storage.insert(txn, 1, Row::new(vec![Value::Int8(42)]))?;
//! storage.commit(txn)?;
//!
//! let reader = storage.begin()?;
//! assert_eq!(storage.get(reader, 1, row)?.values[0], Value::Int8(42));
//! storage.commit(reader)?;
//! # let _ = std::fs::remove_dir_all(&dir);
//! # Ok(())
//! # }
//! ```
//!
//! See `README.md` next to this crate for the page format, the journal
//! format, and the reasoning behind the MVCC layout.

mod btree;
mod clog;
mod codec;
mod crc;
mod engine;
mod page;
mod pager;
mod version;
mod wal;

#[cfg(test)]
mod testutil;

pub use engine::{
    FerriteStorage, StorageConfig, StorageStats, DATA_FILE, JOURNAL_FILE, MAX_TXN_ID,
};
pub use page::PAGE_SIZE;
pub use pager::{CLOG_DIRECTORY_CAPACITY, DEFAULT_CACHE_PAGES};
