use crate::{FerriteError, Row, Snapshot, TableId, TxnId};

pub type RowId = u64;

pub type ScanIter<'a> = Box<dyn Iterator<Item = Result<(RowId, Row), FerriteError>> + 'a>;

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

    /// Snapshot the storage engine will use to decide row visibility for
    /// reads performed under `txn`. Call once per transaction (or once per
    /// statement, for read-committed-style semantics) — the executor owns
    /// when a fresh snapshot is taken, storage only serves reads against
    /// whichever snapshot it's given.
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
