use crate::{FerriteError, Row, Snapshot, TableId, TxnId};

pub type RowId = u64;

pub type ScanIter<'a> = Box<dyn Iterator<Item = Result<(RowId, Row), FerriteError>> + Send + 'a>;

/// The contract the query executor is built against. `ferrite-storage`
/// provides the implementation (pages, B-tree, MVCC row versions, crash
/// recovery); nothing above this trait should know about page layout.
///
/// All methods take an explicit `TxnId` — there is no ambient/thread-local
/// transaction context. Implementations must be safe to call from the
/// async executor across `.await` points, hence `Send + Sync`.
pub trait StorageEngine: Send + Sync {
    fn begin(&self) -> Result<TxnId, FerriteError>;
    fn commit(&self, txn: TxnId) -> Result<(), FerriteError>;
    fn abort(&self, txn: TxnId) -> Result<(), FerriteError>;

    /// (Re-)establishes `txn`'s active read snapshot and returns it. This
    /// has a side effect, not just a read: the first call pins the
    /// snapshot `get`/`scan` will use for the rest of the transaction;
    /// calling it again *advances* that pinned snapshot to the engine's
    /// current state. `get`/`scan` never take a `Snapshot` argument of
    /// their own — they always read under whatever `txn` currently has
    /// pinned. So: call once per transaction for repeatable-read
    /// semantics, or once per statement for read-committed semantics —
    /// the executor decides the cadence, storage just remembers whichever
    /// snapshot was pinned last.
    fn snapshot(&self, txn: TxnId) -> Result<Snapshot, FerriteError>;

    fn insert(&self, txn: TxnId, table: TableId, row: Row) -> Result<RowId, FerriteError>;
    fn update(&self, txn: TxnId, table: TableId, row: RowId, new: Row) -> Result<(), FerriteError>;
    fn delete(&self, txn: TxnId, table: TableId, row: RowId) -> Result<(), FerriteError>;
    fn get(&self, txn: TxnId, table: TableId, row: RowId) -> Result<Row, FerriteError>;

    /// Full scan of `table` visible under `txn`'s snapshot. Index-based
    /// access paths are a `ferrite-planner`/`ferrite-exec` concern layered
    /// on top of this — v1 storage only promises a correct full scan.
    fn scan<'a>(&'a self, txn: TxnId, table: TableId) -> Result<ScanIter<'a>, FerriteError>;

    fn create_table(&self, txn: TxnId, table: TableId) -> Result<(), FerriteError>;
    fn drop_table(&self, txn: TxnId, table: TableId) -> Result<(), FerriteError>;
}
