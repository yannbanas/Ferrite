use crate::{FerriteError, Row, Snapshot, TableId, TxnId, Value};

pub type RowId = u64;

pub type ScanIter<'a> = Box<dyn Iterator<Item = Result<(RowId, Row), FerriteError>> + Send + 'a>;

/// A uniqueness requirement, expressed the only way the storage layer can
/// understand one: "these value positions, taken together, may appear at
/// most once in this table".
///
/// Storage is schema-blind — it stores a `Row`, never a column list — so
/// the constraint has to arrive as positions. `name` travels with them
/// purely so a violation can say which constraint it broke.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UniqueKey {
    pub name: String,
    /// Positions in the row, in key order.
    pub columns: Vec<usize>,
}

impl UniqueKey {
    pub fn new(name: impl Into<String>, columns: Vec<usize>) -> Self {
        Self {
            name: name.into(),
            columns,
        }
    }

    /// The key values this row carries, or `None` when any of them is
    /// null or missing.
    ///
    /// A null never collides — the rule every SQL unique index follows —
    /// and a row shorter than the constraint's widest position is a row
    /// written before an `ALTER TABLE … ADD COLUMN`, which reads back as
    /// that column's default or null; treating it as null keeps the two
    /// cases identical.
    pub fn extract(&self, row: &Row) -> Option<Vec<Value>> {
        let mut out = Vec::with_capacity(self.columns.len());
        for position in &self.columns {
            match row.values.get(*position) {
                Some(value) if !value.is_null() => out.push(value.clone()),
                _ => return None,
            }
        }
        Some(out)
    }

    /// How a violated key is spelled in an error message.
    pub fn describe(&self, key: &[Value]) -> String {
        key.iter()
            .map(|v| format!("{v:?}"))
            .collect::<Vec<_>>()
            .join(", ")
    }

    pub fn violation(&self, key: &[Value]) -> FerriteError {
        FerriteError::UniqueViolation {
            constraint: self.name.clone(),
            key: self.describe(key),
        }
    }
}

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

    /// [`StorageEngine::insert`], refusing the row with
    /// [`FerriteError::UniqueViolation`] when it would duplicate a key
    /// already present.
    ///
    /// Two guarantees the executor cannot provide for itself, which is why
    /// this lives behind the trait rather than above it:
    ///
    /// - **The check sees more than a snapshot does.** A row committed
    ///   after this transaction's snapshot was taken, or written by a
    ///   transaction still in flight, is invisible to `scan` and would let
    ///   a duplicate through. Uniqueness is checked against every row that
    ///   is not known to be dead, regardless of visibility — the same
    ///   reason PostgreSQL's unique index does not consult the snapshot.
    /// - **The check and the write are one step.** Checking first and
    ///   writing after is a time-of-check/time-of-use race: two
    ///   transactions inserting the same key concurrently would both find
    ///   nothing and both write.
    ///
    /// The default implementation provides neither: it checks with a
    /// snapshot scan and then writes. It exists so that a simple in-memory
    /// engine keeps compiling, and is only sound for one that is
    /// single-threaded and single-transaction. `ferrite-storage` overrides
    /// it.
    fn insert_unique(
        &self,
        txn: TxnId,
        table: TableId,
        row: Row,
        unique: &[UniqueKey],
    ) -> Result<RowId, FerriteError> {
        check_unique_by_scan(self, txn, table, &row, unique, None)?;
        self.insert(txn, table, row)
    }

    /// [`StorageEngine::update`] under the same guarantees as
    /// [`StorageEngine::insert_unique`]. The row being updated is not a
    /// conflict with itself.
    fn update_unique(
        &self,
        txn: TxnId,
        table: TableId,
        row: RowId,
        new: Row,
        unique: &[UniqueKey],
    ) -> Result<(), FerriteError> {
        check_unique_by_scan(self, txn, table, &new, unique, Some(row))?;
        self.update(txn, table, row, new)
    }

    /// Full scan of `table` visible under `txn`'s snapshot. Index-based
    /// access paths are a `ferrite-planner`/`ferrite-exec` concern layered
    /// on top of this — v1 storage only promises a correct full scan.
    fn scan<'a>(&'a self, txn: TxnId, table: TableId) -> Result<ScanIter<'a>, FerriteError>;

    fn create_table(&self, txn: TxnId, table: TableId) -> Result<(), FerriteError>;
    fn drop_table(&self, txn: TxnId, table: TableId) -> Result<(), FerriteError>;
}

/// The snapshot-scoped, non-atomic uniqueness check backing the default
/// [`StorageEngine::insert_unique`]. Separate so an implementation that
/// only wants the visibility part can reuse it; see that method for what
/// it does not promise.
pub fn check_unique_by_scan<E: StorageEngine + ?Sized>(
    engine: &E,
    txn: TxnId,
    table: TableId,
    row: &Row,
    unique: &[UniqueKey],
    exclude: Option<RowId>,
) -> Result<(), FerriteError> {
    let wanted: Vec<(&UniqueKey, Vec<Value>)> = unique
        .iter()
        .filter_map(|key| key.extract(row).map(|values| (key, values)))
        .collect();
    if wanted.is_empty() {
        return Ok(());
    }
    for entry in engine.scan(txn, table)? {
        let (rid, existing) = entry?;
        if Some(rid) == exclude {
            continue;
        }
        for (key, values) in &wanted {
            if key.extract(&existing).as_ref() == Some(values) {
                return Err(key.violation(values));
            }
        }
    }
    Ok(())
}
