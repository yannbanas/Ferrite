//! The `StorageEngine` implementation: transaction bookkeeping, snapshot
//! visibility, and the mapping from tables and rows onto B-trees.
//!
//! # Concurrency
//!
//! Every operation takes one engine-wide lock. Ferrite v1 runs a
//! single-threaded executor (see `docs/architecture.md`), so page-level
//! latching would add contention machinery with nothing to contend for.
//! What the lock does *not* do is serialise transactions: several may be
//! open at once and interleave their statements freely, which is the case
//! MVCC actually has to get right. Replacing the lock with per-page
//! latching later changes no visibility logic, only who may touch a page.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use ferrite_common::{
    FerriteError, Row, RowId, ScanIter, Snapshot, StorageEngine, TableId, TxnId, UniqueKey,
};

use crate::btree;
use crate::clog;
use crate::clog::TXNS_PER_CLOG_PAGE;
use crate::codec::{decode_row, encode_row};
use crate::lockfile::DirectoryLock;
use crate::page::{PageId, PageKind, NO_PAGE};
use crate::pager::{Pager, CLOG_DIRECTORY_CAPACITY, DEFAULT_CACHE_PAGES};
use crate::unique::{hash_key, KeyFilters, DEFAULT_FILTER_CAPACITY};
use crate::version::{decode_chain, encode_chain, Version, NO_TXN};

pub const DATA_FILE: &str = "ferrite.db";
pub const JOURNAL_FILE: &str = "ferrite.wal";

/// The highest transaction id this format can address.
///
/// Transaction ids are `u64` and never wrap (see `docs/architecture.md`),
/// but the commit bitmap that records their outcome is finite: a directory
/// of [`CLOG_DIRECTORY_CAPACITY`] segments in the meta page, each covering
/// 65 344 transactions. Running out is a hard stop, not a slowdown, so
/// `ferrite-server` watches the distance to it.
pub const MAX_TXN_ID: TxnId = (CLOG_DIRECTORY_CAPACITY * TXNS_PER_CLOG_PAGE) as TxnId;

/// The counters an operator has to watch, read under the engine lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StorageStats {
    /// The id the next transaction will get, i.e. how far into
    /// [`MAX_TXN_ID`] this database has travelled.
    pub next_txn_id: TxnId,
    /// Transactions open right now, from storage's own bookkeeping rather
    /// than the server's session state.
    pub active_txns: usize,
    /// Pages the data file uses, meta page included.
    pub page_count: u32,
    /// The longest-running open transaction, and how long it has been
    /// open. `None` when nothing is open.
    pub oldest_txn: Option<(TxnId, Duration)>,
}

/// Journal size that triggers an automatic checkpoint. 64 MiB is roughly
/// eight thousand page images: often enough that the journal never becomes
/// the largest thing on the disk, rare enough that the fsync it costs is
/// amortised over thousands of commits.
pub const DEFAULT_CHECKPOINT_JOURNAL_BYTES: u64 = 64 * 1024 * 1024;

/// Tunables for [`FerriteStorage::open_with`].
#[derive(Debug, Clone)]
pub struct StorageConfig {
    /// Page-cache size, in 8 KiB pages.
    pub cache_pages: usize,
    /// Whether `commit` fsyncs the journal before returning. Turning this
    /// off trades durability for speed and is only appropriate for
    /// throwaway data; it is on by default.
    pub fsync: bool,
    /// Hashes kept per unique constraint before the negative cache stops
    /// trying to prove absence — see `src/unique.rs`. Lowering it trades
    /// memory for scans; it never affects which writes are accepted.
    pub unique_filter_capacity: usize,
    /// Journal size past which a commit also checkpoints, in bytes.
    ///
    /// Without this the journal only shrinks on an orderly shutdown, and a
    /// physical journal of *full page images* grows fast: replaying a real
    /// application schema — 72 tables, 938 rows, a few hundred statements —
    /// left a 13 MiB database behind a **5.8 GiB** journal. A long-running
    /// server fills the disk, and filling the disk is a crash, and the
    /// recovery that follows has to replay all of it.
    ///
    /// `0` disables the automatic checkpoint, which is only reasonable for
    /// a process that will not run long.
    pub checkpoint_journal_bytes: u64,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            cache_pages: DEFAULT_CACHE_PAGES,
            fsync: true,
            unique_filter_capacity: DEFAULT_FILTER_CAPACITY,
            checkpoint_journal_bytes: DEFAULT_CHECKPOINT_JOURNAL_BYTES,
        }
    }
}

/// Where a table's data lives: the header page that never moves, and the
/// current B-tree root, which does move when the root splits.
#[derive(Debug, Clone, Copy)]
struct TableRef {
    header: PageId,
    root: PageId,
}

struct TxnState {
    snapshot: Snapshot,
    /// Header pages of tables this transaction dropped, so their space can
    /// be considered for reclamation at commit.
    dropped: Vec<PageId>,
    /// When it opened. A transaction left open holds the pruning horizon
    /// back for every other one, so its age is something an operator has to
    /// be able to see.
    started: Instant,
}

struct Inner {
    pager: Pager,
    active: HashMap<TxnId, TxnState>,
    filters: KeyFilters,
}

/// Page-based, journalled, MVCC storage engine.
pub struct FerriteStorage {
    inner: Mutex<Inner>,
    dir: PathBuf,
    checkpoint_journal_bytes: u64,
    /// Held for the life of the engine. Two processes writing one data
    /// directory is corruption nothing else in this crate defends
    /// against — see `src/lockfile.rs`.
    _lock: DirectoryLock,
}

impl FerriteStorage {
    /// Opens the database under `dir`, creating it if absent and replaying
    /// the journal if the previous run did not check point cleanly.
    pub fn open(dir: impl AsRef<Path>) -> Result<Self, FerriteError> {
        Self::open_with(dir, StorageConfig::default())
    }

    pub fn open_with(dir: impl AsRef<Path>, config: StorageConfig) -> Result<Self, FerriteError> {
        let dir = dir.as_ref().to_path_buf();
        std::fs::create_dir_all(&dir)
            .map_err(|e| FerriteError::Storage(format!("creating {}: {e}", dir.display())))?;
        // Before the files are touched, not after: recovery replays the
        // journal on open, and two processes replaying it at once is
        // exactly the case this refuses.
        let lock = DirectoryLock::acquire(&dir)?;
        let mut pager = Pager::open(
            &dir.join(DATA_FILE),
            &dir.join(JOURNAL_FILE),
            config.cache_pages,
            config.fsync,
        )?;
        if pager.meta().catalog_root == NO_PAGE {
            let root = btree::create(&mut pager)?;
            pager.meta_mut().catalog_root = root;
        }
        Ok(Self {
            inner: Mutex::new(Inner {
                pager,
                active: HashMap::new(),
                filters: KeyFilters::new(config.unique_filter_capacity),
            }),
            dir,
            checkpoint_journal_bytes: config.checkpoint_journal_bytes,
            _lock: lock,
        })
    }

    pub fn path(&self) -> &Path {
        &self.dir
    }

    /// Reads the operational counters. Takes the engine lock, so a caller
    /// that must not block — a metrics scrape, a health probe — should run
    /// it off the async runtime like any other storage call.
    pub fn stats(&self) -> Result<StorageStats, FerriteError> {
        let inner = self.lock()?;
        let now = Instant::now();
        let oldest = inner
            .active
            .iter()
            .max_by_key(|(_, state)| now.saturating_duration_since(state.started))
            .map(|(txn, state)| (*txn, now.saturating_duration_since(state.started)));
        Ok(StorageStats {
            next_txn_id: inner.pager.meta().next_txn_id,
            active_txns: inner.active.len(),
            page_count: inner.pager.meta().page_count,
            oldest_txn: oldest,
        })
    }

    /// Writes everything to the data file, fsyncs it, and truncates the
    /// journal. Optional — recovery reaches the same state from the
    /// journal — but it bounds both recovery time and journal size.
    pub fn checkpoint(&self) -> Result<(), FerriteError> {
        self.lock()?.pager.checkpoint()
    }

    /// The engine lock, or an error once a panic has poisoned it.
    ///
    /// Poisoning is not cleared. A panic inside a page mutation can leave
    /// a half-updated page in the cache, and flushing that later is
    /// exactly the corruption this engine exists to prevent — so the
    /// database refuses to serve anything more, and a restart replays the
    /// journal onto a known-good state. That is a deliberate trade of
    /// availability for correctness, and the log line says so, because
    /// "storage lock poisoned" on every subsequent query is otherwise a
    /// mystery.
    fn lock(&self) -> Result<MutexGuard<'_, Inner>, FerriteError> {
        self.inner.lock().map_err(|_| {
            tracing::error!(
                "the storage engine's lock is poisoned: a panic left a page mid-update, so                  this process will refuse every further statement. Restart it — recovery                  replays the journal onto the last consistent state."
            );
            FerriteError::Storage("storage lock poisoned by a previous panic".into())
        })
    }

    fn scan_step(
        &self,
        txn: TxnId,
        table: TableId,
        from: RowId,
    ) -> Result<Option<(RowId, Row)>, FerriteError> {
        let mut inner = self.lock()?;
        let snapshot = inner.snapshot_of(txn)?;
        let table_ref = inner.table_ref(&snapshot, table)?;
        let mut cursor = from;
        loop {
            let Some((key, payload)) = btree::seek(&mut inner.pager, table_ref.root, cursor)?
            else {
                return Ok(None);
            };
            let chain = decode_chain(&payload)?;
            if let Some(bytes) = inner.visible_version(&snapshot, &chain)? {
                return Ok(Some((key, decode_row(&bytes)?)));
            }
            match key.checked_add(1) {
                Some(next) => cursor = next,
                None => return Ok(None),
            }
        }
    }
}

impl Inner {
    fn snapshot_of(&self, txn: TxnId) -> Result<Snapshot, FerriteError> {
        self.active
            .get(&txn)
            .map(|s| s.snapshot.clone())
            .ok_or(FerriteError::TxnNotActive(txn))
    }

    fn take_snapshot(&mut self, txn: TxnId) -> Snapshot {
        let mut active_at_start: Vec<TxnId> = self
            .active
            .keys()
            .copied()
            .filter(|id| *id != txn)
            .collect();
        active_at_start.sort_unstable();
        Snapshot {
            txn_id: txn,
            xmin: self.pager.meta().next_txn_id,
            active_at_start,
        }
    }

    /// Visibility of a single transaction id under `snap`.
    ///
    /// Follows the rule spelled out on `ferrite_common::Snapshot`: visible
    /// if it committed before `xmin` and is not in-flight. Note that this
    /// makes `xmin` an *exclusive upper bound* — see the crate README for
    /// why, and the agent report for the naming concern.
    fn visible_txn(&mut self, snap: &Snapshot, xid: TxnId) -> Result<bool, FerriteError> {
        if xid == NO_TXN {
            return Ok(false);
        }
        if xid == snap.txn_id {
            return Ok(true);
        }
        if xid >= snap.xmin || snap.active_at_start.binary_search(&xid).is_ok() {
            return Ok(false);
        }
        clog::is_committed(&mut self.pager, xid)
    }

    /// A transaction is aborted once it is neither running nor committed.
    /// Crashed transactions land here without any undo work, which is the
    /// whole point of keeping a commit bitmap.
    fn is_aborted(&mut self, xid: TxnId) -> Result<bool, FerriteError> {
        if xid == NO_TXN || self.active.contains_key(&xid) {
            return Ok(false);
        }
        Ok(!clog::is_committed(&mut self.pager, xid)?)
    }

    fn visible_version(
        &mut self,
        snap: &Snapshot,
        chain: &[Version],
    ) -> Result<Option<Vec<u8>>, FerriteError> {
        for version in chain {
            if !self.visible_txn(snap, version.xmin)? {
                continue;
            }
            if version.xmax != NO_TXN && self.visible_txn(snap, version.xmax)? {
                continue;
            }
            return Ok(Some(version.bytes.clone()));
        }
        Ok(None)
    }

    /// Oldest transaction id whose outcome any live snapshot still cares
    /// about. Versions deleted by a transaction older than this are
    /// invisible everywhere and can be dropped.
    fn prune_horizon(&self) -> TxnId {
        let mut horizon = self.pager.meta().next_txn_id;
        for state in self.active.values() {
            let snap = &state.snapshot;
            let oldest = snap
                .active_at_start
                .first()
                .copied()
                .unwrap_or(snap.xmin)
                .min(snap.xmin);
            horizon = horizon.min(oldest);
        }
        horizon
    }

    /// Drops versions no snapshot can reach and undoes the effect of
    /// aborted deletes. Called on the write path, which is where a chain
    /// grows, so garbage never accumulates without something to trigger
    /// its removal.
    fn prune(&mut self, chain: Vec<Version>) -> Result<Vec<Version>, FerriteError> {
        let horizon = self.prune_horizon();
        let mut out = Vec::with_capacity(chain.len());
        for mut version in chain {
            if self.is_aborted(version.xmin)? {
                continue;
            }
            if version.xmax != NO_TXN && self.is_aborted(version.xmax)? {
                version.xmax = NO_TXN;
            }
            if version.xmax != NO_TXN
                && version.xmax < horizon
                && clog::is_committed(&mut self.pager, version.xmax)?
            {
                continue;
            }
            out.push(version);
        }
        Ok(out)
    }

    fn catalog_root(&self) -> PageId {
        self.pager.meta().catalog_root
    }

    fn read_table_ref(&mut self, header: PageId) -> Result<TableRef, FerriteError> {
        let root = self.pager.with_page(header, |p| {
            if p.kind() != PageKind::TableHeader {
                return Err(FerriteError::Storage(
                    "catalog entry does not point at a table header".into(),
                ));
            }
            Ok(u32::from_le_bytes(p.body()[..4].try_into().unwrap()))
        })??;
        Ok(TableRef { header, root })
    }

    fn set_root(&mut self, header: PageId, root: PageId) -> Result<(), FerriteError> {
        self.pager.with_page_mut(header, |p| {
            p.body_mut()[..4].copy_from_slice(&root.to_le_bytes())
        })
    }

    fn alloc_row_id(&mut self, header: PageId) -> Result<RowId, FerriteError> {
        self.pager.with_page_mut(header, |p| {
            let body = p.body_mut();
            let next = u64::from_le_bytes(body[4..12].try_into().unwrap());
            body[4..12].copy_from_slice(&(next + 1).to_le_bytes());
            next
        })
    }

    /// Catalog entry for `table` as this snapshot sees it.
    fn table_ref(&mut self, snap: &Snapshot, table: TableId) -> Result<TableRef, FerriteError> {
        let root = self.catalog_root();
        let payload = btree::lookup(&mut self.pager, root, table as u64)?
            .ok_or_else(|| FerriteError::TableNotFound(table.to_string()))?;
        let chain = decode_chain(&payload)?;
        let bytes = self
            .visible_version(snap, &chain)?
            .ok_or_else(|| FerriteError::TableNotFound(table.to_string()))?;
        if bytes.len() != 4 {
            return Err(FerriteError::Storage("corrupt catalog entry".into()));
        }
        let header = u32::from_le_bytes(bytes[..4].try_into().unwrap());
        self.read_table_ref(header)
    }

    fn load_chain(&mut self, root: PageId, key: u64) -> Result<Vec<Version>, FerriteError> {
        match btree::lookup(&mut self.pager, root, key)? {
            Some(payload) => decode_chain(&payload),
            None => Ok(Vec::new()),
        }
    }

    /// Writes a chain back, removing the key entirely when nothing is left.
    /// `owner` is the header page whose root pointer must follow a root
    /// split, or `None` for the catalog, whose root lives in the meta page.
    fn store_chain(
        &mut self,
        owner: Option<PageId>,
        root: PageId,
        key: u64,
        chain: &[Version],
    ) -> Result<(), FerriteError> {
        if chain.is_empty() {
            btree::remove(&mut self.pager, root, key)?;
            return Ok(());
        }
        let new_root = btree::upsert(&mut self.pager, root, key, &encode_chain(chain))?;
        if new_root != root {
            match owner {
                Some(header) => self.set_root(header, new_root)?,
                None => self.pager.meta_mut().catalog_root = new_root,
            }
        }
        Ok(())
    }

    /// The version of a chain a unique constraint has to reckon with, or
    /// `None` when the row is gone as far as constraints are concerned.
    ///
    /// This deliberately ignores the reader's snapshot. A unique key is a
    /// property of the *table*, not of one transaction's view of it: a row
    /// committed after our snapshot, or one a concurrent transaction has
    /// written but not yet committed, would be invisible to `scan` and
    /// would let a duplicate through. PostgreSQL's unique index does the
    /// same thing for the same reason.
    ///
    /// The rules, newest version first:
    ///
    /// - a version whose creator aborted never existed; keep looking;
    /// - a version deleted by *this* transaction is gone for us, so a
    ///   delete-then-insert of the same key inside one transaction works;
    /// - a version whose deleter aborted is alive again;
    /// - a version whose deleter committed is gone;
    /// - a version being deleted by a transaction still in flight is
    ///   treated as **present**. PostgreSQL would wait for that
    ///   transaction and let the insert through if the delete commits;
    ///   without a lock manager to wait on, refusing is the answer that
    ///   cannot produce a duplicate. It is over-strict, never wrong.
    fn constraint_version(
        &mut self,
        txn: TxnId,
        chain: &[Version],
    ) -> Result<Option<Vec<u8>>, FerriteError> {
        for version in chain {
            if self.is_aborted(version.xmin)? {
                continue;
            }
            if version.xmax == NO_TXN || self.is_aborted(version.xmax)? {
                return Ok(Some(version.bytes.clone()));
            }
            if version.xmax == txn || !self.active.contains_key(&version.xmax) {
                return Ok(None);
            }
            return Ok(Some(version.bytes.clone()));
        }
        Ok(None)
    }

    /// Walks every row of a table as a unique constraint sees it. `visit`
    /// returns `false` to stop early.
    fn walk_for_constraints(
        &mut self,
        txn: TxnId,
        root: PageId,
        visit: &mut dyn FnMut(RowId, &Row) -> Result<bool, FerriteError>,
    ) -> Result<(), FerriteError> {
        let mut cursor = 0u64;
        loop {
            let Some((key, payload)) = btree::seek(&mut self.pager, root, cursor)? else {
                return Ok(());
            };
            let chain = decode_chain(&payload)?;
            if let Some(bytes) = self.constraint_version(txn, &chain)? {
                if !visit(key, &decode_row(&bytes)?)? {
                    return Ok(());
                }
            }
            match key.checked_add(1) {
                Some(next) => cursor = next,
                None => return Ok(()),
            }
        }
    }

    /// Refuses `row` if any constraint in `unique` is already matched.
    /// `exclude` is the row being updated, which does not conflict with
    /// itself.
    ///
    /// Called with the engine lock held and immediately before the write
    /// it guards, so nothing can slip a colliding row in between.
    fn enforce_unique(
        &mut self,
        txn: TxnId,
        table: TableId,
        root: PageId,
        unique: &[UniqueKey],
        row: &Row,
        exclude: Option<RowId>,
    ) -> Result<(), FerriteError> {
        for constraint in unique {
            let Some(wanted) = constraint.extract(row) else {
                continue;
            };
            let hash = hash_key(&wanted);

            if !self.filters.is_built(table, &constraint.columns) {
                let mut hashes = HashSet::new();
                let columns = constraint.clone();
                self.walk_for_constraints(txn, root, &mut |_, existing| {
                    if let Some(key) = columns.extract(existing) {
                        hashes.insert(hash_key(&key));
                    }
                    Ok(true)
                })?;
                self.filters.build(table, &constraint.columns, hashes);
            }

            if self.filters.may_contain(table, &constraint.columns, hash) {
                let mut clash = false;
                let columns = constraint.clone();
                self.walk_for_constraints(txn, root, &mut |rid, existing| {
                    if Some(rid) != exclude && columns.extract(existing).as_ref() == Some(&wanted) {
                        clash = true;
                        return Ok(false);
                    }
                    Ok(true)
                })?;
                if clash {
                    return Err(constraint.violation(&wanted));
                }
            }
            self.filters.note(table, &constraint.columns, hash);
        }
        Ok(())
    }

    /// Shared write-conflict check for update, delete and drop table.
    ///
    /// Returns the live version this transaction is allowed to supersede.
    /// A concurrent writer produces `SerializationFailure` rather than a
    /// silent overwrite; a version already deleted in our own timeline
    /// produces `RowNotFound`.
    fn writable_version(
        &mut self,
        snap: &Snapshot,
        chain: &[Version],
    ) -> Result<usize, FerriteError> {
        let Some(newest) = chain.first() else {
            return Err(FerriteError::RowNotFound);
        };
        if !self.visible_txn(snap, newest.xmin)? {
            // Written by a transaction we cannot see: either still running
            // or committed after our snapshot. Both are lost-update risks.
            return Err(FerriteError::SerializationFailure);
        }
        if newest.xmax != NO_TXN {
            if newest.xmax == snap.txn_id {
                return Err(FerriteError::RowNotFound);
            }
            if self.visible_txn(snap, newest.xmax)? {
                return Err(FerriteError::RowNotFound);
            }
            if !self.is_aborted(newest.xmax)? {
                return Err(FerriteError::SerializationFailure);
            }
        }
        Ok(0)
    }
}

impl StorageEngine for FerriteStorage {
    fn begin(&self) -> Result<TxnId, FerriteError> {
        let mut inner = self.lock()?;
        let txn = inner.pager.meta().next_txn_id;
        inner.pager.meta_mut().next_txn_id = txn + 1;
        let snapshot = inner.take_snapshot(txn);
        inner.active.insert(
            txn,
            TxnState {
                snapshot,
                dropped: Vec::new(),
                started: Instant::now(),
            },
        );
        tracing::debug!(txn, "transaction begun");
        Ok(txn)
    }

    fn commit(&self, txn: TxnId) -> Result<(), FerriteError> {
        let mut inner = self.lock()?;
        if !inner.active.contains_key(&txn) {
            return Err(FerriteError::TxnNotActive(txn));
        }
        // Order matters: the commit bit is set first so that its page is
        // among the images the journal receives, and only then is the
        // commit record appended and fsynced. A crash anywhere before the
        // fsync leaves the transaction looking aborted, which is correct.
        clog::mark_committed(&mut inner.pager, txn)?;
        inner.pager.commit_to_journal(txn)?;
        let state = inner.active.remove(&txn).expect("presence checked above");

        // Fold the journal into the data file once it has grown past its
        // budget. A checkpoint is safe with other transactions still open:
        // uncommitted work is allowed to reach the data file, and the
        // commit bitmap is what makes it invisible.
        if self.checkpoint_journal_bytes > 0
            && inner.pager.journal_len() >= self.checkpoint_journal_bytes
        {
            let before = inner.pager.journal_len();
            inner.pager.checkpoint()?;
            tracing::debug!(before, "journal checkpointed automatically");
        }

        if !state.dropped.is_empty() && inner.active.is_empty() {
            // Reclaiming a dropped table's pages is only safe with no other
            // snapshot around, since v1 has no lock manager to keep a
            // reader out of a table that is disappearing under it.
            for header in state.dropped {
                let table_ref = inner.read_table_ref(header)?;
                btree::destroy(&mut inner.pager, table_ref.root)?;
                inner.pager.free_page(header)?;
            }
        }
        tracing::debug!(txn, "transaction committed");
        Ok(())
    }

    fn abort(&self, txn: TxnId) -> Result<(), FerriteError> {
        let mut inner = self.lock()?;
        if inner.active.remove(&txn).is_none() {
            return Err(FerriteError::TxnNotActive(txn));
        }
        // No undo pass: the commit bitmap never gets a bit for this id, so
        // every version it wrote is already invisible. The space comes back
        // the next time something prunes those chains.
        inner.pager.abort_to_journal(txn)?;
        tracing::debug!(txn, "transaction aborted");
        Ok(())
    }

    fn snapshot(&self, txn: TxnId) -> Result<Snapshot, FerriteError> {
        let mut inner = self.lock()?;
        if !inner.active.contains_key(&txn) {
            return Err(FerriteError::TxnNotActive(txn));
        }
        // Re-taking the snapshot here is what gives the executor the choice
        // the trait documents: call once per transaction for repeatable
        // reads, once per statement for read-committed.
        let snapshot = inner.take_snapshot(txn);
        if let Some(state) = inner.active.get_mut(&txn) {
            state.snapshot = snapshot.clone();
        }
        Ok(snapshot)
    }

    fn insert(&self, txn: TxnId, table: TableId, row: Row) -> Result<RowId, FerriteError> {
        self.insert_unique(txn, table, row, &[])
    }

    fn update(&self, txn: TxnId, table: TableId, row: RowId, new: Row) -> Result<(), FerriteError> {
        self.update_unique(txn, table, row, new, &[])
    }

    /// Checks every constraint and writes the row under a single
    /// acquisition of the engine lock, which is what makes two concurrent
    /// inserts of the same key unable to both succeed.
    ///
    /// An empty `unique` is an ordinary insert, and says so to the key
    /// filters: a write that skipped the check cannot be allowed to leave
    /// a filter claiming the key is absent.
    fn insert_unique(
        &self,
        txn: TxnId,
        table: TableId,
        row: Row,
        unique: &[UniqueKey],
    ) -> Result<RowId, FerriteError> {
        let mut inner = self.lock()?;
        let snapshot = inner.snapshot_of(txn)?;
        let table_ref = inner.table_ref(&snapshot, table)?;
        if unique.is_empty() {
            inner.filters.invalidate(table);
        } else {
            inner.enforce_unique(txn, table, table_ref.root, unique, &row, None)?;
        }
        let row_id = inner.alloc_row_id(table_ref.header)?;
        let chain = vec![Version::live(txn, encode_row(&row))];
        inner.store_chain(Some(table_ref.header), table_ref.root, row_id, &chain)?;
        Ok(row_id)
    }

    fn update_unique(
        &self,
        txn: TxnId,
        table: TableId,
        row: RowId,
        new: Row,
        unique: &[UniqueKey],
    ) -> Result<(), FerriteError> {
        let mut inner = self.lock()?;
        let snapshot = inner.snapshot_of(txn)?;
        let table_ref = inner.table_ref(&snapshot, table)?;
        if unique.is_empty() {
            inner.filters.invalidate(table);
        } else {
            inner.enforce_unique(txn, table, table_ref.root, unique, &new, Some(row))?;
        }
        let chain = inner.load_chain(table_ref.root, row)?;
        let mut chain = inner.prune(chain)?;
        inner.writable_version(&snapshot, &chain)?;

        if chain[0].xmin == txn {
            // Our own uncommitted version: rewrite it rather than growing
            // the chain with versions nobody else will ever see.
            chain[0].bytes = encode_row(&new);
        } else {
            chain[0].xmax = txn;
            chain.insert(0, Version::live(txn, encode_row(&new)));
        }
        inner.store_chain(Some(table_ref.header), table_ref.root, row, &chain)
    }

    fn delete(&self, txn: TxnId, table: TableId, row: RowId) -> Result<(), FerriteError> {
        let mut inner = self.lock()?;
        let snapshot = inner.snapshot_of(txn)?;
        let table_ref = inner.table_ref(&snapshot, table)?;
        let chain = inner.load_chain(table_ref.root, row)?;
        let mut chain = inner.prune(chain)?;
        inner.writable_version(&snapshot, &chain)?;
        chain[0].xmax = txn;
        inner.store_chain(Some(table_ref.header), table_ref.root, row, &chain)
    }

    fn get(&self, txn: TxnId, table: TableId, row: RowId) -> Result<Row, FerriteError> {
        let mut inner = self.lock()?;
        let snapshot = inner.snapshot_of(txn)?;
        let table_ref = inner.table_ref(&snapshot, table)?;
        let chain = inner.load_chain(table_ref.root, row)?;
        match inner.visible_version(&snapshot, &chain)? {
            Some(bytes) => decode_row(&bytes),
            None => Err(FerriteError::RowNotFound),
        }
    }

    fn scan<'a>(&'a self, txn: TxnId, table: TableId) -> Result<ScanIter<'a>, FerriteError> {
        // Validate up front so a bad table or transaction is an error from
        // `scan` rather than a surprise on the first `next()`.
        {
            let mut inner = self.lock()?;
            let snapshot = inner.snapshot_of(txn)?;
            inner.table_ref(&snapshot, table)?;
        }
        Ok(Box::new(Scan {
            engine: self,
            txn,
            table,
            next_key: Some(0),
        }))
    }

    fn create_table(&self, txn: TxnId, table: TableId) -> Result<(), FerriteError> {
        let mut inner = self.lock()?;
        let snapshot = inner.snapshot_of(txn)?;
        let catalog_root = inner.catalog_root();
        let existing = inner.load_chain(catalog_root, table as u64)?;
        let mut chain = inner.prune(existing)?;

        if let Some(newest) = chain.first() {
            if !inner.visible_txn(&snapshot, newest.xmin)? {
                return Err(FerriteError::SerializationFailure);
            }
            if newest.xmax == NO_TXN {
                return Err(FerriteError::Storage(format!(
                    "table {table} already exists"
                )));
            }
            if newest.xmax != txn && !inner.visible_txn(&snapshot, newest.xmax)? {
                return Err(FerriteError::SerializationFailure);
            }
        }

        // A table id can be handed out again after a drop, so anything the
        // filters remember about the previous occupant has to go.
        inner.filters.invalidate(table);
        let header = inner.pager.alloc_page(PageKind::TableHeader)?;
        let root = btree::create(&mut inner.pager)?;
        inner.pager.with_page_mut(header, |p| {
            let body = p.body_mut();
            body[..4].copy_from_slice(&root.to_le_bytes());
            body[4..12].copy_from_slice(&1u64.to_le_bytes());
        })?;
        chain.insert(0, Version::live(txn, header.to_le_bytes().to_vec()));
        let catalog_root = inner.catalog_root();
        inner.store_chain(None, catalog_root, table as u64, &chain)?;
        tracing::info!(txn, table, "table created");
        Ok(())
    }

    fn drop_table(&self, txn: TxnId, table: TableId) -> Result<(), FerriteError> {
        let mut inner = self.lock()?;
        let snapshot = inner.snapshot_of(txn)?;
        let catalog_root = inner.catalog_root();
        let existing = inner.load_chain(catalog_root, table as u64)?;
        let mut chain = inner.prune(existing)?;
        if chain.is_empty() {
            return Err(FerriteError::TableNotFound(table.to_string()));
        }
        match inner.writable_version(&snapshot, &chain) {
            Ok(_) => {}
            Err(FerriteError::RowNotFound) => {
                return Err(FerriteError::TableNotFound(table.to_string()))
            }
            Err(other) => return Err(other),
        }
        chain[0].xmax = txn;
        let header = u32::from_le_bytes(
            chain[0]
                .bytes
                .get(..4)
                .ok_or_else(|| FerriteError::Storage("corrupt catalog entry".into()))?
                .try_into()
                .unwrap(),
        );
        inner.store_chain(None, catalog_root, table as u64, &chain)?;
        inner.filters.invalidate(table);
        if let Some(state) = inner.active.get_mut(&txn) {
            state.dropped.push(header);
        }
        tracing::info!(txn, table, "table dropped");
        Ok(())
    }
}

/// Lazy full-table scan. The engine lock is taken per step rather than
/// held for the life of the iterator, so a long scan does not freeze out
/// every other transaction; the snapshot keeps the results consistent.
struct Scan<'a> {
    engine: &'a FerriteStorage,
    txn: TxnId,
    table: TableId,
    next_key: Option<RowId>,
}

impl Iterator for Scan<'_> {
    type Item = Result<(RowId, Row), FerriteError>;

    fn next(&mut self) -> Option<Self::Item> {
        let from = self.next_key?;
        match self.engine.scan_step(self.txn, self.table, from) {
            Ok(Some((key, row))) => {
                self.next_key = key.checked_add(1);
                Some(Ok((key, row)))
            }
            Ok(None) => {
                self.next_key = None;
                None
            }
            Err(e) => {
                self.next_key = None;
                Some(Err(e))
            }
        }
    }
}
