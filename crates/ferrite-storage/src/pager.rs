//! Paged file access: the data file, a bounded page cache, the page
//! allocator, and the write ordering that keeps the journal ahead of the
//! data file.
//!
//! Callers reach pages through [`Pager::with_page`] / [`Pager::with_page_mut`]
//! rather than being handed long-lived references. Keeping the borrow
//! inside a closure is what allows the cache to evict freely between
//! accesses; algorithms that need to hold a whole page while touching
//! another one — B-tree splits — copy the contents out first.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use ferrite_common::{FerriteError, TxnId};

use crate::page::{Page, PageId, PageKind, HEADER_SIZE, META_PAGE, NO_PAGE, PAGE_SIZE};
use crate::wal::Journal;

const MAGIC: &[u8; 8] = b"FERRITE1";
const FORMAT_VERSION: u32 = 1;
const META_CLOG_BASE: usize = 40;

/// Commit-log segments the directory in the meta page can address. Each
/// segment covers 65 344 transactions, so this
/// is what bounds the number of transactions a database can ever run — see
/// [`crate::MAX_TXN_ID`].
pub const CLOG_DIRECTORY_CAPACITY: usize = (PAGE_SIZE - HEADER_SIZE - META_CLOG_BASE) / 4;

/// Default page-cache capacity, in pages: 1024 pages is 8 MiB.
pub const DEFAULT_CACHE_PAGES: usize = 1024;

fn io_err(context: &str, e: std::io::Error) -> FerriteError {
    FerriteError::Storage(format!("{context}: {e}"))
}

/// Contents of page 0. Held decoded so the hot fields (allocator, txn
/// counter) do not need re-parsing on every access.
#[derive(Debug, Clone)]
pub struct Meta {
    pub page_count: u32,
    pub free_list_head: PageId,
    pub next_txn_id: TxnId,
    /// Root of the B-tree mapping `TableId` to its table-header page.
    pub catalog_root: PageId,
    pub clog_pages: Vec<PageId>,
}

impl Meta {
    fn fresh() -> Self {
        Self {
            // Page 0 is the meta page itself.
            page_count: 1,
            free_list_head: NO_PAGE,
            next_txn_id: 1,
            catalog_root: NO_PAGE,
            clog_pages: Vec::new(),
        }
    }

    fn encode(&self, page: &mut Page) -> Result<(), FerriteError> {
        let capacity = CLOG_DIRECTORY_CAPACITY;
        if self.clog_pages.len() > capacity {
            return Err(FerriteError::Storage(format!(
                "commit-log directory is full ({capacity} segments); \
                 v1 supports at most {} transactions per database",
                capacity * crate::clog::TXNS_PER_CLOG_PAGE
            )));
        }
        let body = page.body_mut();
        body[0..8].copy_from_slice(MAGIC);
        body[8..12].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
        body[12..16].copy_from_slice(&(PAGE_SIZE as u32).to_le_bytes());
        body[16..20].copy_from_slice(&self.page_count.to_le_bytes());
        body[20..24].copy_from_slice(&self.free_list_head.to_le_bytes());
        body[24..32].copy_from_slice(&self.next_txn_id.to_le_bytes());
        body[32..36].copy_from_slice(&self.catalog_root.to_le_bytes());
        body[36..40].copy_from_slice(&(self.clog_pages.len() as u32).to_le_bytes());
        for (i, id) in self.clog_pages.iter().enumerate() {
            let at = META_CLOG_BASE + i * 4;
            body[at..at + 4].copy_from_slice(&id.to_le_bytes());
        }
        Ok(())
    }

    fn decode(page: &Page) -> Result<Self, FerriteError> {
        let body = page.body();
        if &body[0..8] != MAGIC {
            return Err(FerriteError::Storage(
                "not a Ferrite database file (bad magic)".into(),
            ));
        }
        let version = u32::from_le_bytes(body[8..12].try_into().unwrap());
        if version != FORMAT_VERSION {
            return Err(FerriteError::Storage(format!(
                "unsupported storage format version {version}, expected {FORMAT_VERSION}"
            )));
        }
        let page_size = u32::from_le_bytes(body[12..16].try_into().unwrap()) as usize;
        if page_size != PAGE_SIZE {
            return Err(FerriteError::Storage(format!(
                "database was created with {page_size}-byte pages, this build uses {PAGE_SIZE}"
            )));
        }
        let clog_count = u32::from_le_bytes(body[36..40].try_into().unwrap()) as usize;
        if clog_count > CLOG_DIRECTORY_CAPACITY {
            return Err(FerriteError::Storage(
                "corrupt meta page: commit-log directory length out of range".into(),
            ));
        }
        let mut clog_pages = Vec::with_capacity(clog_count);
        for i in 0..clog_count {
            let at = META_CLOG_BASE + i * 4;
            clog_pages.push(u32::from_le_bytes(body[at..at + 4].try_into().unwrap()));
        }
        Ok(Self {
            page_count: u32::from_le_bytes(body[16..20].try_into().unwrap()),
            free_list_head: u32::from_le_bytes(body[20..24].try_into().unwrap()),
            next_txn_id: u64::from_le_bytes(body[24..32].try_into().unwrap()),
            catalog_root: u32::from_le_bytes(body[32..36].try_into().unwrap()),
            clog_pages,
        })
    }
}

struct Entry {
    page: Page,
    /// Differs from what the data file holds.
    dirty_vs_file: bool,
    /// Contains changes the journal has not yet been told about.
    dirty_vs_journal: bool,
    used: u64,
}

pub struct Pager {
    file: File,
    journal: Journal,
    cache: HashMap<PageId, Entry>,
    capacity: usize,
    clock: u64,
    meta: Meta,
    meta_dirty_vs_journal: bool,
}

impl Pager {
    /// Opens (or creates) `data_path` with its journal at `journal_path`,
    /// replaying the journal if the previous run did not shut down
    /// cleanly.
    pub fn open(
        data_path: &Path,
        journal_path: &Path,
        capacity: usize,
        sync_on_commit: bool,
    ) -> Result<Self, FerriteError> {
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(data_path)
            .map_err(|e| io_err("opening data file", e))?;

        let recovery = recover(&mut file, journal_path)?;
        let mut journal = Journal::open(journal_path, sync_on_commit)?;
        // Every intact record has now been applied to the data file, which
        // was fsynced before we got here, so the journal has served its
        // purpose. Discarding it is what keeps the file from being appended
        // to at offset zero and leaving older records stranded behind the
        // new ones, where a later replay could resurrect them.
        journal.truncate()?;
        // Keep LSNs monotonic across restarts so a page header's LSN always
        // orders correctly against the journal that produced it.
        journal.set_next_lsn(recovery.max_lsn + 1);

        let len = file
            .metadata()
            .map_err(|e| io_err("stat data file", e))?
            .len();
        let mut pager = Pager {
            file,
            journal,
            cache: HashMap::new(),
            capacity: capacity.max(8),
            clock: 0,
            meta: Meta::fresh(),
            meta_dirty_vs_journal: false,
        };

        if len == 0 {
            let mut page = Page::new(PageKind::Meta);
            pager.meta.encode(&mut page)?;
            pager.write_page_to_file(META_PAGE, &page)?;
            pager
                .file
                .sync_data()
                .map_err(|e| io_err("syncing new data file", e))?;
        } else {
            let page = pager.load_from_file(META_PAGE)?;
            pager.meta = Meta::decode(&page)?;
        }
        if recovery.pages > 0 {
            // The replayed images are already in the data file; the
            // journal stays on disk until the next checkpoint truncates it,
            // so a crash during recovery just replays again.
            tracing::info!(
                pages = recovery.pages,
                committed = recovery.commits,
                rolled_back = recovery.aborts,
                torn_tail = recovery.torn_tail,
                page_count = pager.meta.page_count,
                "recovered storage from journal"
            );
        }
        Ok(pager)
    }

    pub fn meta(&self) -> &Meta {
        &self.meta
    }

    pub fn meta_mut(&mut self) -> &mut Meta {
        self.meta_dirty_vs_journal = true;
        &mut self.meta
    }

    fn load_from_file(&mut self, page_id: PageId) -> Result<Page, FerriteError> {
        let offset = page_id as u64 * PAGE_SIZE as u64;
        self.file
            .seek(SeekFrom::Start(offset))
            .map_err(|e| io_err("seeking data file", e))?;
        let mut bytes = [0u8; PAGE_SIZE];
        let mut filled = 0;
        while filled < PAGE_SIZE {
            match self.file.read(&mut bytes[filled..]) {
                Ok(0) => {
                    return Err(FerriteError::Storage(format!(
                        "page {page_id} is past the end of the data file"
                    )))
                }
                Ok(n) => filled += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(io_err("reading data file", e)),
            }
        }
        Page::from_bytes(bytes, page_id)
    }

    fn write_page_to_file(&mut self, page_id: PageId, page: &Page) -> Result<(), FerriteError> {
        let offset = page_id as u64 * PAGE_SIZE as u64;
        self.file
            .seek(SeekFrom::Start(offset))
            .map_err(|e| io_err("seeking data file", e))?;
        self.file
            .write_all(&page.to_bytes())
            .map_err(|e| io_err("writing data file", e))
    }

    fn tick(&mut self) -> u64 {
        self.clock += 1;
        self.clock
    }

    fn ensure_cached(&mut self, page_id: PageId) -> Result<(), FerriteError> {
        if self.cache.contains_key(&page_id) {
            return Ok(());
        }
        self.evict_if_needed()?;
        let page = self.load_from_file(page_id)?;
        let used = self.tick();
        self.cache.insert(
            page_id,
            Entry {
                page,
                dirty_vs_file: false,
                dirty_vs_journal: false,
                used,
            },
        );
        Ok(())
    }

    fn evict_if_needed(&mut self) -> Result<(), FerriteError> {
        while self.cache.len() >= self.capacity {
            let victim = self
                .cache
                .iter()
                .min_by_key(|(_, e)| e.used)
                .map(|(id, _)| *id);
            let Some(victim) = victim else { break };
            if self.cache.get(&victim).is_some_and(|e| e.dirty_vs_journal) {
                // Write-ahead rule, applied to the *whole* cache rather
                // than to the victim alone.
                //
                // Journalling one page in isolation is not enough: a B-tree
                // split dirties a parent and its new child together, and
                // recovering the parent without the child would leave a
                // node pointing at a page the data file never received.
                // Flushing every dirty page — including the meta page,
                // whose `next_txn_id` must not lag behind the transaction
                // ids appearing in those images — keeps what the journal
                // holds a consistent snapshot of the whole database.
                self.journal_dirty_pages()?;
            }
            let entry = self.cache.remove(&victim).expect("victim was just found");
            if entry.dirty_vs_file {
                self.write_page_to_file(victim, &entry.page)?;
            }
        }
        Ok(())
    }

    pub fn with_page<R>(
        &mut self,
        page_id: PageId,
        f: impl FnOnce(&Page) -> R,
    ) -> Result<R, FerriteError> {
        self.ensure_cached(page_id)?;
        let used = self.tick();
        let entry = self.cache.get_mut(&page_id).expect("just cached");
        entry.used = used;
        Ok(f(&entry.page))
    }

    pub fn with_page_mut<R>(
        &mut self,
        page_id: PageId,
        f: impl FnOnce(&mut Page) -> R,
    ) -> Result<R, FerriteError> {
        self.ensure_cached(page_id)?;
        let used = self.tick();
        let entry = self.cache.get_mut(&page_id).expect("just cached");
        entry.used = used;
        entry.dirty_vs_file = true;
        entry.dirty_vs_journal = true;
        Ok(f(&mut entry.page))
    }

    pub fn write_page(&mut self, page_id: PageId, page: Page) -> Result<(), FerriteError> {
        self.ensure_cached(page_id)?;
        let used = self.tick();
        let entry = self.cache.get_mut(&page_id).expect("just cached");
        entry.page = page;
        entry.used = used;
        entry.dirty_vs_file = true;
        entry.dirty_vs_journal = true;
        Ok(())
    }

    pub fn alloc_page(&mut self, kind: PageKind) -> Result<PageId, FerriteError> {
        let head = self.meta.free_list_head;
        let page_id = if head != NO_PAGE {
            let next = self.with_page(head, |p| p.extra())?;
            self.meta_mut().free_list_head = next;
            head
        } else {
            let id = self.meta.page_count;
            self.meta_mut().page_count += 1;
            id
        };
        self.evict_if_needed()?;
        let used = self.tick();
        self.cache.insert(
            page_id,
            Entry {
                page: Page::new(kind),
                dirty_vs_file: true,
                dirty_vs_journal: true,
                used,
            },
        );
        Ok(page_id)
    }

    pub fn free_page(&mut self, page_id: PageId) -> Result<(), FerriteError> {
        let head = self.meta.free_list_head;
        let mut page = Page::new(PageKind::Free);
        page.set_extra(head);
        self.write_page(page_id, page)?;
        self.meta_mut().free_list_head = page_id;
        Ok(())
    }

    fn flush_meta_to_journal(&mut self) -> Result<(), FerriteError> {
        if !self.meta_dirty_vs_journal {
            return Ok(());
        }
        let mut page = self.encoded_meta_page()?;
        let lsn = self.journal.next_lsn();
        page.set_lsn(lsn);
        self.journal
            .log_page_image(lsn, META_PAGE, &page.to_bytes())?;
        self.meta_dirty_vs_journal = false;
        Ok(())
    }

    fn encoded_meta_page(&self) -> Result<Page, FerriteError> {
        let mut page = Page::new(PageKind::Meta);
        self.meta.encode(&mut page)?;
        Ok(page)
    }

    /// Journals every change made so far and records `txn` as committed,
    /// then makes the journal durable. Returns once the commit is on
    /// stable storage; the data file catches up lazily.
    pub fn commit_to_journal(&mut self, txn: TxnId) -> Result<(), FerriteError> {
        self.journal_dirty_pages()?;
        self.journal.log_commit(txn)?;
        self.journal.sync()
    }

    /// Records an explicit rollback. Not required for correctness — a
    /// transaction with no commit record is treated as aborted anyway —
    /// but it lets recovery distinguish "rolled back" from "still running
    /// when the power went out" in logs. Deliberately not fsynced: losing
    /// the record changes nothing about the outcome.
    pub fn abort_to_journal(&mut self, txn: TxnId) -> Result<(), FerriteError> {
        self.journal.log_abort(txn)
    }

    fn journal_dirty_pages(&mut self) -> Result<(), FerriteError> {
        self.flush_meta_to_journal()?;
        let ids: Vec<PageId> = self
            .cache
            .iter()
            .filter(|(_, e)| e.dirty_vs_journal)
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            let lsn = self.journal.next_lsn();
            let bytes = {
                let entry = self.cache.get_mut(&id).expect("id came from the cache");
                entry.page.set_lsn(lsn);
                entry.dirty_vs_journal = false;
                entry.page.to_bytes()
            };
            self.journal.log_page_image(lsn, id, &bytes)?;
        }
        Ok(())
    }

    /// Writes every cached change into the data file, fsyncs it, and
    /// discards the journal. After this the data file alone is a complete,
    /// consistent database.
    pub fn checkpoint(&mut self) -> Result<(), FerriteError> {
        let ids: Vec<PageId> = self
            .cache
            .iter()
            .filter(|(_, e)| e.dirty_vs_file)
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            let page = self
                .cache
                .get(&id)
                .expect("id came from the cache")
                .page
                .clone();
            self.write_page_to_file(id, &page)?;
            if let Some(entry) = self.cache.get_mut(&id) {
                entry.dirty_vs_file = false;
                entry.dirty_vs_journal = false;
            }
        }
        let meta_page = self.encoded_meta_page()?;
        self.write_page_to_file(META_PAGE, &meta_page)?;
        self.meta_dirty_vs_journal = false;
        self.file
            .flush()
            .map_err(|e| io_err("flushing data file", e))?;
        self.file
            .sync_data()
            .map_err(|e| io_err("syncing data file", e))?;
        self.journal.log_checkpoint()?;
        self.journal.sync()?;
        self.journal.truncate()
    }
}

#[derive(Debug, Default)]
struct Recovery {
    pages: usize,
    commits: usize,
    aborts: usize,
    max_lsn: u64,
    torn_tail: bool,
}

/// Replays the journal into the data file.
///
/// Only page images are applied. Transaction outcomes need no work here:
/// the commit bitmap is itself made of pages, so replaying the images
/// restores it, and any transaction whose commit record never made it to
/// disk simply has no bit set and is therefore aborted.
fn recover(file: &mut File, journal_path: &Path) -> Result<Recovery, FerriteError> {
    let mut stats = Recovery::default();
    stats.torn_tail = Journal::replay(journal_path, |record| {
        match record {
            crate::wal::Record::PageImage {
                lsn,
                page_id,
                bytes,
            } => {
                let offset = page_id as u64 * PAGE_SIZE as u64;
                file.seek(SeekFrom::Start(offset))
                    .map_err(|e| io_err("seeking during recovery", e))?;
                file.write_all(bytes.as_slice())
                    .map_err(|e| io_err("writing during recovery", e))?;
                stats.pages += 1;
                stats.max_lsn = stats.max_lsn.max(lsn);
            }
            crate::wal::Record::Commit(txn) => {
                stats.commits += 1;
                tracing::trace!(txn, "replayed commit record");
            }
            crate::wal::Record::Abort(txn) => {
                stats.aborts += 1;
                tracing::trace!(txn, "replayed abort record");
            }
            crate::wal::Record::Checkpoint => {}
        }
        Ok(())
    })?;
    if stats.pages > 0 {
        file.sync_data()
            .map_err(|e| io_err("syncing after recovery", e))?;
    }
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{DATA_FILE, JOURNAL_FILE};
    use crate::testutil::TempDb;

    fn reopen(db: &TempDb) -> Pager {
        Pager::open(
            &db.path().join(DATA_FILE),
            &db.path().join(JOURNAL_FILE),
            16,
            false,
        )
        .expect("reopen")
    }

    #[test]
    fn allocation_recycles_freed_pages() {
        let db = TempDb::new("pager_alloc");
        let mut pager = db.pager();
        let a = pager.alloc_page(PageKind::Leaf).unwrap();
        let b = pager.alloc_page(PageKind::Leaf).unwrap();
        assert_ne!(a, b);
        let high_water = pager.meta().page_count;

        pager.free_page(a).unwrap();
        pager.free_page(b).unwrap();
        let c = pager.alloc_page(PageKind::Leaf).unwrap();
        let d = pager.alloc_page(PageKind::Leaf).unwrap();
        assert_eq!(pager.meta().page_count, high_water, "no new pages needed");
        assert!([a, b].contains(&c) && [a, b].contains(&d));
        assert_ne!(c, d);
    }

    #[test]
    fn a_cache_smaller_than_the_working_set_still_serves_every_page() {
        let db = TempDb::new("pager_evict");
        let mut pager = db.pager();
        // The cache holds 16 pages; touch far more than that.
        let ids: Vec<PageId> = (0..200)
            .map(|i| {
                let id = pager.alloc_page(PageKind::Leaf).unwrap();
                pager
                    .with_page_mut(id, |p| {
                        assert!(p.insert_item(0, &(i as u64).to_le_bytes()));
                    })
                    .unwrap();
                id
            })
            .collect();
        for (i, id) in ids.iter().enumerate() {
            let seen = pager
                .with_page(*id, |p| p.item(0).to_vec())
                .expect("page still reachable after eviction");
            assert_eq!(seen, (i as u64).to_le_bytes());
        }
    }

    #[test]
    fn a_checkpoint_survives_reopening() {
        let db = TempDb::new("pager_checkpoint");
        let id = {
            let mut pager = db.pager();
            let id = pager.alloc_page(PageKind::Leaf).unwrap();
            pager
                .with_page_mut(id, |p| assert!(p.insert_item(0, b"durable")))
                .unwrap();
            pager.meta_mut().next_txn_id = 77;
            pager.checkpoint().unwrap();
            id
        };
        let mut pager = reopen(&db);
        assert_eq!(pager.meta().next_txn_id, 77);
        assert_eq!(
            pager.with_page(id, |p| p.item(0).to_vec()).unwrap(),
            b"durable"
        );
    }

    #[test]
    fn journalled_changes_come_back_without_a_checkpoint() {
        let db = TempDb::new("pager_journal");
        let id = {
            let mut pager = db.pager();
            let id = pager.alloc_page(PageKind::Leaf).unwrap();
            pager
                .with_page_mut(id, |p| assert!(p.insert_item(0, b"journalled")))
                .unwrap();
            pager.commit_to_journal(1).unwrap();
            id
        };
        let mut pager = reopen(&db);
        assert_eq!(
            pager.with_page(id, |p| p.item(0).to_vec()).unwrap(),
            b"journalled"
        );
    }

    fn open_error(db: &TempDb) -> String {
        match Pager::open(
            &db.path().join(DATA_FILE),
            &db.path().join(JOURNAL_FILE),
            16,
            false,
        ) {
            Err(FerriteError::Storage(message)) => message,
            Err(other) => panic!("expected a storage error, got {other}"),
            Ok(_) => panic!("a foreign file should not open as a database"),
        }
    }

    #[test]
    fn refuses_a_file_that_is_not_a_ferrite_database() {
        // Arbitrary bytes fail the page checksum, which is the first gate.
        let db = TempDb::new("pager_foreign");
        std::fs::write(db.path().join(DATA_FILE), vec![0u8; PAGE_SIZE]).unwrap();
        assert!(open_error(&db).contains("checksum"));

        // A structurally valid page with the wrong magic gets past the
        // checksum and must still be refused.
        let db = TempDb::new("pager_magic");
        let mut page = Page::new(PageKind::Meta);
        page.body_mut()[..8].copy_from_slice(b"SQLite3\0");
        std::fs::write(db.path().join(DATA_FILE), page.to_bytes()).unwrap();
        assert!(open_error(&db).contains("magic"));
    }

    #[test]
    fn refuses_a_database_written_by_a_future_format() {
        let db = TempDb::new("pager_version");
        let mut page = Page::new(PageKind::Meta);
        {
            let body = page.body_mut();
            body[..8].copy_from_slice(MAGIC);
            body[8..12].copy_from_slice(&99u32.to_le_bytes());
            body[12..16].copy_from_slice(&(PAGE_SIZE as u32).to_le_bytes());
        }
        std::fs::write(db.path().join(DATA_FILE), page.to_bytes()).unwrap();
        assert!(open_error(&db).contains("format version 99"));
    }
}
