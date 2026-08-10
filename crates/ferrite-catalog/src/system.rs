use std::collections::HashMap;
use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};

use ferrite_common::{
    Catalog, ColumnDef, DataType, FerriteError, Row, RowId, Schema, StorageEngine, TableId, TxnId,
    Value,
};

/// `TableId` of the table listing every table, including itself.
pub const TABLES_TABLE_ID: TableId = 1;
/// `TableId` of the table listing every column of every table.
pub const COLUMNS_TABLE_ID: TableId = 2;
/// Schema the catalog's own tables live in.
pub const CATALOG_SCHEMA: &str = "ferrite_catalog";
/// Schema unqualified names resolve to.
pub const DEFAULT_SCHEMA: &str = "public";
/// Ids below this are reserved for catalog tables; user tables start here.
pub const FIRST_USER_TABLE_ID: TableId = 16;

const TABLES_TABLE_NAME: &str = "ferrite_tables";
const COLUMNS_TABLE_NAME: &str = "ferrite_columns";

fn catalog_error(message: impl Into<String>) -> FerriteError {
    FerriteError::Storage(format!("catalog: {}", message.into()))
}

fn tables_schema() -> Schema {
    Schema {
        columns: vec![
            column("table_id", DataType::Int8),
            column("schema_name", DataType::Text),
            column("table_name", DataType::Text),
        ],
    }
}

fn columns_schema() -> Schema {
    Schema {
        columns: vec![
            column("table_id", DataType::Int8),
            column("ordinal", DataType::Int4),
            column("column_name", DataType::Text),
            column("data_type", DataType::Text),
            column("nullable", DataType::Boolean),
        ],
    }
}

fn column(name: &str, data_type: DataType) -> ColumnDef {
    ColumnDef {
        name: name.to_string(),
        data_type,
        nullable: false,
    }
}

fn type_name(data_type: DataType) -> &'static str {
    match data_type {
        DataType::Boolean => "boolean",
        DataType::Int4 => "int4",
        DataType::Int8 => "int8",
        DataType::Float8 => "float8",
        DataType::Text => "text",
        DataType::Timestamp => "timestamp",
        DataType::Uuid => "uuid",
        DataType::Json => "json",
    }
}

fn type_from_name(name: &str) -> Result<DataType, FerriteError> {
    match name {
        "boolean" => Ok(DataType::Boolean),
        "int4" => Ok(DataType::Int4),
        "int8" => Ok(DataType::Int8),
        "float8" => Ok(DataType::Float8),
        "text" => Ok(DataType::Text),
        "timestamp" => Ok(DataType::Timestamp),
        "uuid" => Ok(DataType::Uuid),
        "json" => Ok(DataType::Json),
        other => Err(catalog_error(format!("unknown stored type `{other}`"))),
    }
}

fn text(row: &Row, index: usize) -> Result<String, FerriteError> {
    match row.values.get(index) {
        Some(Value::Text(s)) => Ok(s.clone()),
        _ => Err(catalog_error("corrupt catalog row: expected text")),
    }
}

fn table_id(row: &Row, index: usize) -> Result<TableId, FerriteError> {
    match row.values.get(index) {
        Some(Value::Int8(v)) => TableId::try_from(*v)
            .map_err(|_| catalog_error(format!("corrupt catalog row: bad table id {v}"))),
        _ => Err(catalog_error("corrupt catalog row: expected int8")),
    }
}

fn ordinal(row: &Row, index: usize) -> Result<usize, FerriteError> {
    match row.values.get(index) {
        Some(Value::Int4(v)) => usize::try_from(*v)
            .map_err(|_| catalog_error(format!("corrupt catalog row: bad ordinal {v}"))),
        _ => Err(catalog_error("corrupt catalog row: expected int4")),
    }
}

fn boolean(row: &Row, index: usize) -> Result<bool, FerriteError> {
    match row.values.get(index) {
        Some(Value::Boolean(b)) => Ok(*b),
        _ => Err(catalog_error("corrupt catalog row: expected boolean")),
    }
}

#[derive(Debug, Clone)]
struct Entry {
    id: TableId,
    schema: String,
    name: String,
    columns: Schema,
}

#[derive(Default)]
struct Cache {
    by_id: HashMap<TableId, Entry>,
    by_name: HashMap<(String, String), TableId>,
    next_id: TableId,
}

impl Cache {
    fn insert(&mut self, entry: Entry) {
        self.by_name
            .insert((entry.schema.clone(), entry.name.clone()), entry.id);
        if entry.id >= self.next_id {
            self.next_id = entry.id + 1;
        }
        self.by_id.insert(entry.id, entry);
    }

    fn remove(&mut self, id: TableId) -> Option<Entry> {
        let entry = self.by_id.remove(&id)?;
        self.by_name
            .remove(&(entry.schema.clone(), entry.name.clone()));
        Some(entry)
    }
}

/// The system catalog: name resolution and schema lookup, stored as two
/// ordinary tables through a [`StorageEngine`].
///
/// `ferrite_catalog.ferrite_tables` holds one row per table and
/// `ferrite_catalog.ferrite_columns` one row per column; both describe
/// themselves, so a bootstrapped database has no metadata that lives
/// outside the normal storage path. Storage is the source of truth — an
/// in-memory index is kept alongside it purely so name lookups on the hot
/// path do not scan, and [`SystemCatalog::open`] rebuilds that index from
/// storage alone.
pub struct SystemCatalog {
    storage: Arc<dyn StorageEngine>,
    cache: RwLock<Cache>,
}

impl SystemCatalog {
    /// Create the catalog tables in an empty database and return a
    /// catalog over them. Fails if they already exist — use
    /// [`SystemCatalog::open`] for an existing database.
    pub fn bootstrap(storage: Arc<dyn StorageEngine>) -> Result<Self, FerriteError> {
        let catalog = Self {
            storage,
            cache: RwLock::new(Cache {
                next_id: FIRST_USER_TABLE_ID,
                ..Cache::default()
            }),
        };

        let txn = catalog.storage.begin()?;
        let result = catalog.bootstrap_in(txn);
        catalog.finish(txn, result)?;
        catalog.reload()?;
        Ok(catalog)
    }

    /// Open the catalog of a database that has already been bootstrapped,
    /// rebuilding the in-memory index by reading the catalog tables.
    pub fn open(storage: Arc<dyn StorageEngine>) -> Result<Self, FerriteError> {
        let catalog = Self {
            storage,
            cache: RwLock::new(Cache {
                next_id: FIRST_USER_TABLE_ID,
                ..Cache::default()
            }),
        };
        catalog.reload()?;
        Ok(catalog)
    }

    /// Discard the in-memory index and rebuild it from the catalog
    /// tables.
    pub fn reload(&self) -> Result<(), FerriteError> {
        let txn = self.storage.begin()?;
        let result = self.read_all(txn);
        let entries = self.finish(txn, result)?;

        let mut cache = self.write_cache()?;
        *cache = Cache {
            next_id: FIRST_USER_TABLE_ID,
            ..Cache::default()
        };
        for entry in entries {
            cache.insert(entry);
        }
        Ok(())
    }

    /// The tables the catalog itself is made of, for callers that need to
    /// treat them specially (`ferrite-exec` refusing writes, for example).
    pub fn is_system_table(table: TableId) -> bool {
        table < FIRST_USER_TABLE_ID
    }

    fn bootstrap_in(&self, txn: TxnId) -> Result<(), FerriteError> {
        self.storage.create_table(txn, TABLES_TABLE_ID)?;
        self.storage.create_table(txn, COLUMNS_TABLE_ID)?;
        self.record_table(txn, TABLES_TABLE_ID, CATALOG_SCHEMA, TABLES_TABLE_NAME)?;
        self.record_columns(txn, TABLES_TABLE_ID, &tables_schema())?;
        self.record_table(txn, COLUMNS_TABLE_ID, CATALOG_SCHEMA, COLUMNS_TABLE_NAME)?;
        self.record_columns(txn, COLUMNS_TABLE_ID, &columns_schema())?;
        Ok(())
    }

    fn record_table(
        &self,
        txn: TxnId,
        id: TableId,
        schema: &str,
        name: &str,
    ) -> Result<RowId, FerriteError> {
        self.storage.insert(
            txn,
            TABLES_TABLE_ID,
            Row::new(vec![
                Value::Int8(i64::from(id)),
                Value::Text(schema.to_string()),
                Value::Text(name.to_string()),
            ]),
        )
    }

    fn record_columns(&self, txn: TxnId, id: TableId, schema: &Schema) -> Result<(), FerriteError> {
        for (index, col) in schema.columns.iter().enumerate() {
            let ordinal = i32::try_from(index)
                .map_err(|_| catalog_error("a table cannot have that many columns"))?;
            self.storage.insert(
                txn,
                COLUMNS_TABLE_ID,
                Row::new(vec![
                    Value::Int8(i64::from(id)),
                    Value::Int4(ordinal),
                    Value::Text(col.name.clone()),
                    Value::Text(type_name(col.data_type).to_string()),
                    Value::Boolean(col.nullable),
                ]),
            )?;
        }
        Ok(())
    }

    fn read_all(&self, txn: TxnId) -> Result<Vec<Entry>, FerriteError> {
        let mut columns: HashMap<TableId, Vec<(usize, ColumnDef)>> = HashMap::new();
        for row in self.storage.scan(txn, COLUMNS_TABLE_ID)? {
            let (_, row) = row?;
            let id = table_id(&row, 0)?;
            let ordinal = ordinal(&row, 1)?;
            let def = ColumnDef {
                name: text(&row, 2)?,
                data_type: type_from_name(&text(&row, 3)?)?,
                nullable: boolean(&row, 4)?,
            };
            columns.entry(id).or_default().push((ordinal, def));
        }

        let mut entries = Vec::new();
        for row in self.storage.scan(txn, TABLES_TABLE_ID)? {
            let (_, row) = row?;
            let id = table_id(&row, 0)?;
            let mut cols = columns.remove(&id).unwrap_or_default();
            cols.sort_by_key(|(ordinal, _)| *ordinal);
            entries.push(Entry {
                id,
                schema: text(&row, 1)?,
                name: text(&row, 2)?,
                columns: Schema {
                    columns: cols.into_iter().map(|(_, def)| def).collect(),
                },
            });
        }
        Ok(entries)
    }

    /// Commit `txn` when `result` is `Ok`, abort it otherwise. Abort
    /// failures never mask the original error.
    fn finish<T>(&self, txn: TxnId, result: Result<T, FerriteError>) -> Result<T, FerriteError> {
        match result {
            Ok(value) => {
                self.storage.commit(txn)?;
                Ok(value)
            }
            Err(err) => {
                let _ = self.storage.abort(txn);
                Err(err)
            }
        }
    }

    fn read_cache(&self) -> Result<RwLockReadGuard<'_, Cache>, FerriteError> {
        self.cache
            .read()
            .map_err(|_| catalog_error("index lock poisoned"))
    }

    fn write_cache(&self) -> Result<RwLockWriteGuard<'_, Cache>, FerriteError> {
        self.cache
            .write()
            .map_err(|_| catalog_error("index lock poisoned"))
    }

    fn validate(schema: &str, name: &str, columns: &Schema) -> Result<(), FerriteError> {
        if schema.is_empty() || name.is_empty() {
            return Err(catalog_error("schema and table names must not be empty"));
        }
        if schema == CATALOG_SCHEMA {
            return Err(FerriteError::PermissionDenied(format!(
                "schema `{CATALOG_SCHEMA}` is reserved for the system catalog"
            )));
        }
        if columns.columns.is_empty() {
            return Err(catalog_error("a table needs at least one column"));
        }
        for (index, col) in columns.columns.iter().enumerate() {
            if col.name.is_empty() {
                return Err(catalog_error("column names must not be empty"));
            }
            if columns.columns[..index].iter().any(|c| c.name == col.name) {
                return Err(catalog_error(format!("duplicate column `{}`", col.name)));
            }
        }
        Ok(())
    }

    /// Row ids in `table` whose column `index` holds this table id.
    fn rows_for_table(
        &self,
        txn: TxnId,
        table: TableId,
        wanted: TableId,
    ) -> Result<Vec<RowId>, FerriteError> {
        let mut ids = Vec::new();
        for row in self.storage.scan(txn, table)? {
            let (row_id, row) = row?;
            if table_id(&row, 0)? == wanted {
                ids.push(row_id);
            }
        }
        Ok(ids)
    }

    fn create_in(
        &self,
        txn: TxnId,
        id: TableId,
        schema: &str,
        name: &str,
        columns: &Schema,
    ) -> Result<(), FerriteError> {
        self.storage.create_table(txn, id)?;
        self.record_table(txn, id, schema, name)?;
        self.record_columns(txn, id, columns)?;
        Ok(())
    }

    fn drop_in(&self, txn: TxnId, id: TableId) -> Result<(), FerriteError> {
        for row_id in self.rows_for_table(txn, COLUMNS_TABLE_ID, id)? {
            self.storage.delete(txn, COLUMNS_TABLE_ID, row_id)?;
        }
        for row_id in self.rows_for_table(txn, TABLES_TABLE_ID, id)? {
            self.storage.delete(txn, TABLES_TABLE_ID, row_id)?;
        }
        self.storage.drop_table(txn, id)?;
        Ok(())
    }
}

impl Catalog for SystemCatalog {
    fn table_id(&self, schema: &str, name: &str) -> Result<Option<TableId>, FerriteError> {
        let cache = self.read_cache()?;
        Ok(cache
            .by_name
            .get(&(schema.to_string(), name.to_string()))
            .copied())
    }

    fn table_schema(&self, table: TableId) -> Result<Schema, FerriteError> {
        let cache = self.read_cache()?;
        cache
            .by_id
            .get(&table)
            .map(|entry| entry.columns.clone())
            .ok_or_else(|| FerriteError::TableNotFound(table.to_string()))
    }

    fn create_table(
        &self,
        schema: &str,
        name: &str,
        columns: Schema,
    ) -> Result<TableId, FerriteError> {
        Self::validate(schema, name, &columns)?;

        let id = {
            let mut cache = self.write_cache()?;
            if cache
                .by_name
                .contains_key(&(schema.to_string(), name.to_string()))
            {
                return Err(catalog_error(format!(
                    "table `{schema}.{name}` already exists"
                )));
            }
            let id = cache.next_id;
            // Reserve the id while still holding the lock so two
            // concurrent creates cannot hand out the same one.
            cache.next_id += 1;
            id
        };

        let txn = self.storage.begin()?;
        let result = self.create_in(txn, id, schema, name, &columns);
        self.finish(txn, result)?;

        let mut cache = self.write_cache()?;
        cache.insert(Entry {
            id,
            schema: schema.to_string(),
            name: name.to_string(),
            columns,
        });
        Ok(id)
    }

    fn drop_table(&self, table: TableId) -> Result<(), FerriteError> {
        if Self::is_system_table(table) {
            return Err(FerriteError::PermissionDenied(format!(
                "table {table} belongs to the system catalog and cannot be dropped"
            )));
        }
        {
            let cache = self.read_cache()?;
            if !cache.by_id.contains_key(&table) {
                return Err(FerriteError::TableNotFound(table.to_string()));
            }
        }

        let txn = self.storage.begin()?;
        let result = self.drop_in(txn, table);
        self.finish(txn, result)?;

        self.write_cache()?.remove(table);
        Ok(())
    }

    fn list_tables(&self, schema: &str) -> Result<Vec<(TableId, String)>, FerriteError> {
        let cache = self.read_cache()?;
        let mut tables: Vec<(TableId, String)> = cache
            .by_id
            .values()
            .filter(|entry| entry.schema == schema)
            .map(|entry| (entry.id, entry.name.clone()))
            .collect();
        tables.sort_by(|a, b| a.1.cmp(&b.1));
        Ok(tables)
    }
}
