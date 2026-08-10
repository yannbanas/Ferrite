#![allow(dead_code)]

//! Scratch directories for the integration tests.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use ferrite_storage::{FerriteStorage, StorageConfig};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A directory that deletes itself when the test finishes.
pub struct Scratch {
    dir: PathBuf,
    keep: bool,
}

impl Scratch {
    pub fn new(tag: &str) -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "ferrite-it-{}-{tag}-{n}-{nanos}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create scratch dir");
        Self { dir, keep: false }
    }

    /// Wraps a directory owned by someone else (a child process, say)
    /// without taking responsibility for deleting it.
    pub fn borrowed(dir: PathBuf) -> Self {
        Self { dir, keep: true }
    }

    pub fn path(&self) -> &Path {
        &self.dir
    }

    /// Opens the database. Tests turn fsync off: they exercise the
    /// *ordering* the journal enforces, and every crash they simulate is a
    /// process death rather than a power cut, so paying for a real disk
    /// flush on every commit would only make them slow.
    pub fn open(&self) -> FerriteStorage {
        self.try_open().expect("open storage")
    }

    pub fn try_open(&self) -> Result<FerriteStorage, ferrite_common::FerriteError> {
        FerriteStorage::open_with(
            &self.dir,
            StorageConfig {
                cache_pages: 64,
                fsync: false,
                ..Default::default()
            },
        )
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if !self.keep {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }
}

pub fn int_row(v: i64) -> ferrite_common::Row {
    ferrite_common::Row::new(vec![ferrite_common::Value::Int8(v)])
}

pub fn row_int(row: &ferrite_common::Row) -> i64 {
    match row.values.first() {
        Some(ferrite_common::Value::Int8(v)) => *v,
        other => panic!("expected an Int8 row, got {other:?}"),
    }
}
