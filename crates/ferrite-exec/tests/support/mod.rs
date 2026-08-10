//! Minimal in-memory `StorageEngine`/`Catalog`/index implementations.
//!
//! Test scaffolding only: `ferrite-storage` (Agent 1) and `ferrite-catalog`
//! (Agent 2) provide the real ones. There is no MVCC visibility here —
//! writes are immediately visible to everyone — because these tests are
//! about the executor's control flow, not about isolation.
//!
//! Each integration test binary compiles this module separately, so
//! whatever that one binary does not use looks dead to the compiler.
#![allow(dead_code)]

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use ferrite_common::{
    Catalog, ColumnDef, FerriteError, IndexCatalog, IndexDef, IndexId, Row, RowId, ScanIter,
    Schema, Snapshot, StorageEngine, TableId, TxnId, Value,
};
use ferrite_exec::IndexProvider;

#[derive(Default)]
struct StorageInner {
    next_txn: TxnId,
    next_rid: RowId,
    tables: HashMap<TableId, BTreeMap<RowId, Row>>,
}

#[derive(Default)]
pub struct MemStorage {
    inner: Mutex<StorageInner>,
}

impl MemStorage {
    pub fn new() -> Self {
        Self::default()
    }

    /// Everything currently stored in `table`, ordered by `RowId`.
    pub fn dump(&self, table: TableId) -> Vec<Row> {
        let inner = self.inner.lock().unwrap();
        inner
            .tables
            .get(&table)
            .map(|rows| rows.values().cloned().collect())
            .unwrap_or_default()
    }
}

impl StorageEngine for MemStorage {
    fn begin(&self) -> Result<TxnId, FerriteError> {
        let mut inner = self.inner.lock().unwrap();
        inner.next_txn += 1;
        Ok(inner.next_txn)
    }

    fn commit(&self, _txn: TxnId) -> Result<(), FerriteError> {
        Ok(())
    }

    fn abort(&self, _txn: TxnId) -> Result<(), FerriteError> {
        Ok(())
    }

    fn snapshot(&self, txn: TxnId) -> Result<Snapshot, FerriteError> {
        Ok(Snapshot {
            txn_id: txn,
            xmin: txn,
            active_at_start: Vec::new(),
        })
    }

    fn insert(&self, _txn: TxnId, table: TableId, row: Row) -> Result<RowId, FerriteError> {
        let mut inner = self.inner.lock().unwrap();
        inner.next_rid += 1;
        let rid = inner.next_rid;
        inner
            .tables
            .get_mut(&table)
            .ok_or_else(|| FerriteError::TableNotFound(table.to_string()))?
            .insert(rid, row);
        Ok(rid)
    }

    fn update(
        &self,
        _txn: TxnId,
        table: TableId,
        row: RowId,
        new: Row,
    ) -> Result<(), FerriteError> {
        let mut inner = self.inner.lock().unwrap();
        let rows = inner
            .tables
            .get_mut(&table)
            .ok_or_else(|| FerriteError::TableNotFound(table.to_string()))?;
        if !rows.contains_key(&row) {
            return Err(FerriteError::RowNotFound);
        }
        rows.insert(row, new);
        Ok(())
    }

    fn delete(&self, _txn: TxnId, table: TableId, row: RowId) -> Result<(), FerriteError> {
        let mut inner = self.inner.lock().unwrap();
        inner
            .tables
            .get_mut(&table)
            .ok_or_else(|| FerriteError::TableNotFound(table.to_string()))?
            .remove(&row)
            .map(|_| ())
            .ok_or(FerriteError::RowNotFound)
    }

    fn get(&self, _txn: TxnId, table: TableId, row: RowId) -> Result<Row, FerriteError> {
        let inner = self.inner.lock().unwrap();
        inner
            .tables
            .get(&table)
            .and_then(|rows| rows.get(&row))
            .cloned()
            .ok_or(FerriteError::RowNotFound)
    }

    fn scan<'a>(&'a self, _txn: TxnId, table: TableId) -> Result<ScanIter<'a>, FerriteError> {
        let inner = self.inner.lock().unwrap();
        let rows: Vec<(RowId, Row)> = inner
            .tables
            .get(&table)
            .ok_or_else(|| FerriteError::TableNotFound(table.to_string()))?
            .iter()
            .map(|(rid, row)| (*rid, row.clone()))
            .collect();
        drop(inner);
        Ok(Box::new(rows.into_iter().map(Ok)))
    }

    fn create_table(&self, _txn: TxnId, table: TableId) -> Result<(), FerriteError> {
        let mut inner = self.inner.lock().unwrap();
        inner.tables.entry(table).or_default();
        Ok(())
    }

    fn drop_table(&self, _txn: TxnId, table: TableId) -> Result<(), FerriteError> {
        let mut inner = self.inner.lock().unwrap();
        inner.tables.remove(&table);
        Ok(())
    }
}

#[derive(Default)]
struct CatalogInner {
    next_id: TableId,
    by_name: HashMap<String, TableId>,
    schemas: HashMap<TableId, Schema>,
    indexes: Vec<IndexDef>,
    next_index_id: IndexId,
}

#[derive(Default)]
pub struct MemCatalog {
    inner: Mutex<CatalogInner>,
}

impl MemCatalog {
    pub fn new() -> Self {
        Self::default()
    }

    /// `column` is a position in the table's schema, which is what the
    /// tests find convenient; the shared `IndexDef` names its columns, so
    /// it is resolved here.
    pub fn add_index(&self, name: &str, table: TableId, column: usize, unique: bool) {
        let mut inner = self.inner.lock().unwrap();
        let column_name = inner.schemas[&table].columns[column].name.clone();
        inner.next_index_id += 1;
        let id = inner.next_index_id;
        inner.indexes.push(IndexDef {
            id,
            name: name.to_string(),
            table,
            columns: vec![column_name],
            unique,
        });
    }

    /// Replace a table's schema behind the planner's back, to exercise the
    /// executor's stale-plan check.
    pub fn replace_schema(&self, table: TableId, schema: Schema) {
        self.inner.lock().unwrap().schemas.insert(table, schema);
    }

    fn index_column(&self, table: TableId, name: &str) -> Option<usize> {
        let inner = self.inner.lock().unwrap();
        let def = inner
            .indexes
            .iter()
            .find(|i| i.table == table && i.name == name)?;
        inner.schemas.get(&table)?.column_index(&def.columns[0])
    }
}

impl Catalog for MemCatalog {
    fn table_id(&self, schema: &str, name: &str) -> Result<Option<TableId>, FerriteError> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .by_name
            .get(&format!("{schema}.{name}"))
            .copied())
    }

    fn table_schema(&self, table: TableId) -> Result<Schema, FerriteError> {
        self.inner
            .lock()
            .unwrap()
            .schemas
            .get(&table)
            .cloned()
            .ok_or_else(|| FerriteError::TableNotFound(table.to_string()))
    }

    fn create_table(
        &self,
        schema: &str,
        name: &str,
        columns: Schema,
    ) -> Result<TableId, FerriteError> {
        let mut inner = self.inner.lock().unwrap();
        inner.next_id += 1;
        let id = inner.next_id;
        inner.by_name.insert(format!("{schema}.{name}"), id);
        inner.schemas.insert(id, columns);
        Ok(id)
    }

    fn drop_table(&self, table: TableId) -> Result<(), FerriteError> {
        let mut inner = self.inner.lock().unwrap();
        inner.schemas.remove(&table);
        inner.by_name.retain(|_, id| *id != table);
        Ok(())
    }

    fn list_tables(&self, schema: &str) -> Result<Vec<(TableId, String)>, FerriteError> {
        let inner = self.inner.lock().unwrap();
        let prefix = format!("{schema}.");
        Ok(inner
            .by_name
            .iter()
            .filter_map(|(key, id)| {
                key.strip_prefix(&prefix)
                    .map(|name| (*id, name.to_string()))
            })
            .collect())
    }
}

impl IndexCatalog for MemCatalog {
    fn create_index(
        &self,
        name: &str,
        table: TableId,
        columns: &[String],
        unique: bool,
    ) -> Result<IndexId, FerriteError> {
        let mut inner = self.inner.lock().unwrap();
        inner.next_index_id += 1;
        let id = inner.next_index_id;
        inner.indexes.push(IndexDef {
            id,
            name: name.to_string(),
            table,
            columns: columns.to_vec(),
            unique,
        });
        Ok(id)
    }

    fn drop_index(&self, index: IndexId) -> Result<(), FerriteError> {
        self.inner.lock().unwrap().indexes.retain(|i| i.id != index);
        Ok(())
    }

    fn index(&self, index: IndexId) -> Result<Option<IndexDef>, FerriteError> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .indexes
            .iter()
            .find(|i| i.id == index)
            .cloned())
    }

    fn index_by_name(
        &self,
        _namespace: &str,
        name: &str,
    ) -> Result<Option<IndexDef>, FerriteError> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .indexes
            .iter()
            .find(|i| i.name == name)
            .cloned())
    }

    fn indexes_for(&self, table: TableId) -> Result<Vec<IndexDef>, FerriteError> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .indexes
            .iter()
            .filter(|i| i.table == table)
            .cloned()
            .collect())
    }
}

/// Fake index: resolves the indexed column then walks the table. Linear,
/// obviously — the point is to exercise the executor's index code path and
/// prove it was taken, not to be fast.
pub struct MemIndexes<'a> {
    storage: &'a MemStorage,
    catalog: &'a MemCatalog,
    lookups: AtomicUsize,
}

impl<'a> MemIndexes<'a> {
    pub fn new(storage: &'a MemStorage, catalog: &'a MemCatalog) -> Self {
        Self {
            storage,
            catalog,
            lookups: AtomicUsize::new(0),
        }
    }

    /// How many times the executor probed an index.
    pub fn lookups(&self) -> usize {
        self.lookups.load(Ordering::SeqCst)
    }
}

impl IndexProvider for MemIndexes<'_> {
    fn lookup(
        &self,
        txn: TxnId,
        table: TableId,
        index: &str,
        key: &Value,
    ) -> Result<Vec<RowId>, FerriteError> {
        self.lookups.fetch_add(1, Ordering::SeqCst);
        let column = self
            .catalog
            .index_column(table, index)
            .ok_or_else(|| FerriteError::Exec(format!("no such index: {index}")))?;

        let mut out = Vec::new();
        for entry in self.storage.scan(txn, table)? {
            let (rid, row) = entry?;
            if row.values.get(column) == Some(key) {
                out.push(rid);
            }
        }
        Ok(out)
    }
}

pub fn column(name: &str, data_type: ferrite_common::DataType, nullable: bool) -> ColumnDef {
    ColumnDef::new(name, data_type, nullable)
}
