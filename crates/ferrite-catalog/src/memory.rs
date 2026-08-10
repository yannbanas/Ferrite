//! A minimal in-memory [`StorageEngine`], written so `ferrite-catalog`
//! could be built and tested before `ferrite-storage` existed.
//!
//! It is deliberately *not* a stand-in for the real engine: rows live in
//! a `BTreeMap`, there are no pages, no B-tree and no MVCC. Writes are
//! applied immediately and reversed from a per-transaction undo log on
//! abort, which gives correct all-or-nothing behaviour for a single
//! writer but no isolation between concurrent transactions — a
//! transaction sees other transactions' uncommitted writes.
//!
//! Enable with the `test-util` feature to reuse it from another crate's
//! tests; it is always available inside this crate's own tests.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Mutex, MutexGuard};

use ferrite_common::{FerriteError, Row, RowId, ScanIter, Snapshot, StorageEngine, TableId, TxnId};

#[derive(Debug)]
enum Undo {
    Insert {
        table: TableId,
        row: RowId,
    },
    Restore {
        table: TableId,
        row: RowId,
        old: Row,
    },
    CreateTable(TableId),
    DropTable(TableId, BTreeMap<RowId, Row>),
}

#[derive(Default)]
struct State {
    tables: HashMap<TableId, BTreeMap<RowId, Row>>,
    active: HashMap<TxnId, Vec<Undo>>,
    next_txn: TxnId,
    next_row: RowId,
    committed: Vec<TxnId>,
}

/// See the module documentation for what this does and does not promise.
#[derive(Default)]
pub struct MemoryStorage {
    state: Mutex<State>,
}

impl MemoryStorage {
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of transactions that reached `commit`. Handy for asserting
    /// that the catalog really does open and close a transaction per
    /// operation.
    pub fn committed_transactions(&self) -> Result<usize, FerriteError> {
        Ok(self.lock()?.committed.len())
    }

    /// Rows currently stored in `table`, in `RowId` order.
    pub fn rows(&self, table: TableId) -> Result<Vec<Row>, FerriteError> {
        let state = self.lock()?;
        match state.tables.get(&table) {
            Some(rows) => Ok(rows.values().cloned().collect()),
            None => Err(FerriteError::TableNotFound(table.to_string())),
        }
    }

    pub fn table_exists(&self, table: TableId) -> Result<bool, FerriteError> {
        Ok(self.lock()?.tables.contains_key(&table))
    }

    fn lock(&self) -> Result<MutexGuard<'_, State>, FerriteError> {
        self.state
            .lock()
            .map_err(|_| FerriteError::Storage("in-memory storage lock poisoned".into()))
    }
}

impl State {
    fn check_active(&self, txn: TxnId) -> Result<(), FerriteError> {
        if self.active.contains_key(&txn) {
            Ok(())
        } else {
            Err(FerriteError::TxnNotActive(txn))
        }
    }

    fn table_mut(&mut self, table: TableId) -> Result<&mut BTreeMap<RowId, Row>, FerriteError> {
        self.tables
            .get_mut(&table)
            .ok_or_else(|| FerriteError::TableNotFound(table.to_string()))
    }

    fn record(&mut self, txn: TxnId, undo: Undo) {
        if let Some(log) = self.active.get_mut(&txn) {
            log.push(undo);
        }
    }
}

impl StorageEngine for MemoryStorage {
    fn begin(&self) -> Result<TxnId, FerriteError> {
        let mut state = self.lock()?;
        state.next_txn += 1;
        let txn = state.next_txn;
        state.active.insert(txn, Vec::new());
        Ok(txn)
    }

    fn commit(&self, txn: TxnId) -> Result<(), FerriteError> {
        let mut state = self.lock()?;
        state.check_active(txn)?;
        state.active.remove(&txn);
        state.committed.push(txn);
        Ok(())
    }

    fn abort(&self, txn: TxnId) -> Result<(), FerriteError> {
        let mut state = self.lock()?;
        state.check_active(txn)?;
        let log = state.active.remove(&txn).unwrap_or_default();
        for undo in log.into_iter().rev() {
            match undo {
                Undo::Insert { table, row } => {
                    if let Some(rows) = state.tables.get_mut(&table) {
                        rows.remove(&row);
                    }
                }
                Undo::Restore { table, row, old } => {
                    if let Some(rows) = state.tables.get_mut(&table) {
                        rows.insert(row, old);
                    }
                }
                Undo::CreateTable(table) => {
                    state.tables.remove(&table);
                }
                Undo::DropTable(table, rows) => {
                    state.tables.insert(table, rows);
                }
            }
        }
        Ok(())
    }

    fn snapshot(&self, txn: TxnId) -> Result<Snapshot, FerriteError> {
        let state = self.lock()?;
        state.check_active(txn)?;
        let mut active_at_start: Vec<TxnId> = state.active.keys().copied().collect();
        active_at_start.sort_unstable();
        let xmin = active_at_start.first().copied().unwrap_or(txn);
        Ok(Snapshot {
            txn_id: txn,
            xmin,
            active_at_start,
        })
    }

    fn insert(&self, txn: TxnId, table: TableId, row: Row) -> Result<RowId, FerriteError> {
        let mut state = self.lock()?;
        state.check_active(txn)?;
        state.next_row += 1;
        let id = state.next_row;
        state.table_mut(table)?.insert(id, row);
        state.record(txn, Undo::Insert { table, row: id });
        Ok(id)
    }

    fn update(&self, txn: TxnId, table: TableId, row: RowId, new: Row) -> Result<(), FerriteError> {
        let mut state = self.lock()?;
        state.check_active(txn)?;
        let rows = state.table_mut(table)?;
        let old = rows.insert(row, new).ok_or(FerriteError::RowNotFound)?;
        state.record(txn, Undo::Restore { table, row, old });
        Ok(())
    }

    fn delete(&self, txn: TxnId, table: TableId, row: RowId) -> Result<(), FerriteError> {
        let mut state = self.lock()?;
        state.check_active(txn)?;
        let old = state
            .table_mut(table)?
            .remove(&row)
            .ok_or(FerriteError::RowNotFound)?;
        state.record(txn, Undo::Restore { table, row, old });
        Ok(())
    }

    fn get(&self, txn: TxnId, table: TableId, row: RowId) -> Result<Row, FerriteError> {
        let state = self.lock()?;
        state.check_active(txn)?;
        state
            .tables
            .get(&table)
            .ok_or_else(|| FerriteError::TableNotFound(table.to_string()))?
            .get(&row)
            .cloned()
            .ok_or(FerriteError::RowNotFound)
    }

    fn scan<'a>(&'a self, txn: TxnId, table: TableId) -> Result<ScanIter<'a>, FerriteError> {
        let state = self.lock()?;
        state.check_active(txn)?;
        let rows: Vec<(RowId, Row)> = state
            .tables
            .get(&table)
            .ok_or_else(|| FerriteError::TableNotFound(table.to_string()))?
            .iter()
            .map(|(id, row)| (*id, row.clone()))
            .collect();
        Ok(Box::new(rows.into_iter().map(Ok)))
    }

    fn create_table(&self, txn: TxnId, table: TableId) -> Result<(), FerriteError> {
        let mut state = self.lock()?;
        state.check_active(txn)?;
        if state.tables.contains_key(&table) {
            return Err(FerriteError::Storage(format!(
                "table {table} already exists"
            )));
        }
        state.tables.insert(table, BTreeMap::new());
        state.record(txn, Undo::CreateTable(table));
        Ok(())
    }

    fn drop_table(&self, txn: TxnId, table: TableId) -> Result<(), FerriteError> {
        let mut state = self.lock()?;
        state.check_active(txn)?;
        let rows = state
            .tables
            .remove(&table)
            .ok_or_else(|| FerriteError::TableNotFound(table.to_string()))?;
        state.record(txn, Undo::DropTable(table, rows));
        Ok(())
    }
}
