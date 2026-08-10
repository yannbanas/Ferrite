//! Scratch databases for the unit tests in this crate.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::engine::{DATA_FILE, JOURNAL_FILE};
use crate::pager::Pager;

static COUNTER: AtomicU64 = AtomicU64::new(0);

pub struct TempDb {
    dir: PathBuf,
}

impl TempDb {
    pub fn new(tag: &str) -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("ferrite-storage-{}-{tag}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create scratch dir");
        Self { dir }
    }

    pub fn path(&self) -> &Path {
        &self.dir
    }

    pub fn pager(&self) -> Pager {
        Pager::open(
            &self.dir.join(DATA_FILE),
            &self.dir.join(JOURNAL_FILE),
            64,
            false,
        )
        .expect("open pager")
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}
