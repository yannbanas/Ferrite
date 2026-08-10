use std::collections::HashMap;
use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};

use ferrite_common::{
    Catalog, ColumnDef, ColumnDefault, DataType, FerriteError, IndexCatalog, IndexDef, IndexId,
    Row, RowId, Schema, StorageEngine, TableId, TxnId, UniqueKey, Value,
};

/// `TableId` of the table listing every table, including itself.
pub const TABLES_TABLE_ID: TableId = 1;
/// `TableId` of the table listing every column of every table.
pub const COLUMNS_TABLE_ID: TableId = 2;
/// `TableId` of the table listing every index.
pub const INDEXES_TABLE_ID: TableId = 3;
/// Schema the catalog's own tables live in.
pub const CATALOG_SCHEMA: &str = "ferrite_catalog";
/// Schema unqualified names resolve to.
pub const DEFAULT_SCHEMA: &str = "public";
/// Ids below this are reserved for catalog tables; user objects start here.
pub const FIRST_USER_TABLE_ID: TableId = 16;

const TABLES_TABLE_NAME: &str = "ferrite_tables";
const COLUMNS_TABLE_NAME: &str = "ferrite_columns";
const INDEXES_TABLE_NAME: &str = "ferrite_indexes";

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
            ColumnDef::new("default_expr", DataType::Text, true),
        ],
    }
}

fn indexes_schema() -> Schema {
    Schema {
        columns: vec![
            column("index_id", DataType::Int8),
            column("table_id", DataType::Int8),
            column("index_name", DataType::Text),
            column("ordinal", DataType::Int4),
            column("column_name", DataType::Text),
            column("is_unique", DataType::Boolean),
        ],
    }
}

fn column(name: &str, data_type: DataType) -> ColumnDef {
    ColumnDef::new(name, data_type, false)
}

/// The keys the catalog's own tables must not hold twice.
///
/// The catalog is stored as ordinary tables, so nothing distinguishes its
/// rows from anyone else's — and until these were declared, nothing stopped
/// it from holding two rows naming one table. The in-memory index is no
/// defence: it is a `HashMap` keyed by name, so it can only ever see one of
/// a pair, and `reload` rebuilds it from storage, where the duplicate
/// silently wins or loses on iteration order. The observable symptom is a
/// `DROP TABLE` that appears to work followed by a `CREATE TABLE` refused
/// as already existing, because the drop removed one row and the reload
/// found the other.
///
/// These are enforced by storage, atomically with the write, exactly like a
/// user table's `PRIMARY KEY` — see `StorageEngine::insert_unique`.
fn tables_keys() -> Vec<UniqueKey> {
    vec![
        UniqueKey::new("ferrite_tables_pkey", vec![0]),
        UniqueKey::new("ferrite_tables_name_key", vec![1, 2]),
    ]
}

fn columns_keys() -> Vec<UniqueKey> {
    vec![
        UniqueKey::new("ferrite_columns_pkey", vec![0, 1]),
        UniqueKey::new("ferrite_columns_name_key", vec![0, 2]),
    ]
}

fn indexes_keys() -> Vec<UniqueKey> {
    vec![
        UniqueKey::new("ferrite_indexes_pkey", vec![0, 3]),
        UniqueKey::new("ferrite_indexes_name_key", vec![2, 3]),
    ]
}

/// Encode a `DEFAULT` for the `default_expr` column of `ferrite_columns`.
///
/// One tagged string rather than a column per shape: the tag says how to
/// read the rest, so a new kind of default (a sequence, say) adds a tag
/// instead of a column, and a row written by an older Ferrite still reads
/// back. The constant's own type is not stored — it is always the
/// column's declared type, which sits two columns away in the same row.
///
/// ```text
/// n            DEFAULT NULL
/// f:now        CURRENT_TIMESTAMP
/// v:<text>     a constant, spelled per the column's data type
/// ```
fn encode_default(default: &ColumnDefault, data_type: DataType) -> Result<String, FerriteError> {
    Ok(match default {
        ColumnDefault::CurrentTimestamp => "f:now".to_string(),
        ColumnDefault::Constant(Value::Null) => "n".to_string(),
        ColumnDefault::Constant(value) => format!("v:{}", encode_constant(value, data_type)?),
    })
}

fn encode_constant(value: &Value, data_type: DataType) -> Result<String, FerriteError> {
    match (value, data_type) {
        (Value::Boolean(b), DataType::Boolean) => Ok(b.to_string()),
        (Value::Int4(n), DataType::Int4) => Ok(n.to_string()),
        (Value::Int8(n), DataType::Int8) => Ok(n.to_string()),
        (Value::Float8(f), DataType::Float8) => Ok(format!("{f:?}")),
        (Value::Text(s), DataType::Text) => Ok(s.clone()),
        (Value::Json(s), DataType::Json) => Ok(s.clone()),
        (Value::Timestamp(n), DataType::Timestamp) => Ok(n.to_string()),
        (Value::Uuid(u), DataType::Uuid) => Ok(format!("{u:032x}")),
        (value, data_type) => Err(FerriteError::InvalidDefinition(format!(
            "a DEFAULT of {value:?} does not fit a {data_type:?} column"
        ))),
    }
}

fn decode_default(encoded: &str, data_type: DataType) -> Result<ColumnDefault, FerriteError> {
    match encoded {
        "n" => Ok(ColumnDefault::Constant(Value::Null)),
        "f:now" => Ok(ColumnDefault::CurrentTimestamp),
        _ => match encoded.strip_prefix("v:") {
            Some(body) => Ok(ColumnDefault::Constant(decode_constant(body, data_type)?)),
            None => Err(catalog_error(format!(
                "corrupt catalog row: unknown default encoding `{encoded}`"
            ))),
        },
    }
}

fn decode_constant(body: &str, data_type: DataType) -> Result<Value, FerriteError> {
    let bad = || catalog_error(format!("corrupt catalog row: bad {data_type:?} default"));
    Ok(match data_type {
        DataType::Boolean => Value::Boolean(body.parse().map_err(|_| bad())?),
        DataType::Int4 => Value::Int4(body.parse().map_err(|_| bad())?),
        DataType::Int8 => Value::Int8(body.parse().map_err(|_| bad())?),
        DataType::Float8 => Value::Float8(body.parse().map_err(|_| bad())?),
        DataType::Text => Value::Text(body.to_string()),
        DataType::Json => Value::Json(body.to_string()),
        DataType::Timestamp => Value::Timestamp(body.parse().map_err(|_| bad())?),
        DataType::Uuid => Value::Uuid(u128::from_str_radix(body, 16).map_err(|_| bad())?),
    })
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

/// A nullable text column. A value missing from the row entirely reads as
/// `None` too: `ferrite_columns` gained `default_expr` after the fact, so
/// rows written by an earlier Ferrite are one value short — the same
/// short-row reconciliation `ALTER TABLE ADD COLUMN` needs for user
/// tables, applied to the catalog's own.
fn optional_text(row: &Row, index: usize) -> Result<Option<String>, FerriteError> {
    match row.values.get(index) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Text(s)) => Ok(Some(s.clone())),
        _ => Err(catalog_error("corrupt catalog row: expected text or null")),
    }
}

fn object_id(row: &Row, index: usize) -> Result<TableId, FerriteError> {
    match row.values.get(index) {
        Some(Value::Int8(v)) => TableId::try_from(*v)
            .map_err(|_| catalog_error(format!("corrupt catalog row: bad object id {v}"))),
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

fn to_ordinal(index: usize) -> Result<i32, FerriteError> {
    i32::try_from(index).map_err(|_| catalog_error("too many columns"))
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
    indexes: HashMap<IndexId, IndexDef>,
    index_by_name: HashMap<(String, String), IndexId>,
    next_id: TableId,
}

impl Cache {
    fn empty() -> Self {
        Self {
            next_id: FIRST_USER_TABLE_ID,
            ..Self::default()
        }
    }

    fn bump(&mut self, id: TableId) {
        if id >= self.next_id {
            self.next_id = id + 1;
        }
    }

    fn insert_table(&mut self, entry: Entry) {
        self.by_name
            .insert((entry.schema.clone(), entry.name.clone()), entry.id);
        self.bump(entry.id);
        self.by_id.insert(entry.id, entry);
    }

    fn remove_table(&mut self, id: TableId) -> Option<Entry> {
        let entry = self.by_id.remove(&id)?;
        self.by_name
            .remove(&(entry.schema.clone(), entry.name.clone()));
        let doomed: Vec<IndexId> = self
            .indexes
            .values()
            .filter(|index| index.table == id)
            .map(|index| index.id)
            .collect();
        for index in doomed {
            self.remove_index(index);
        }
        Some(entry)
    }

    fn insert_index(&mut self, def: IndexDef, schema: &str) {
        self.index_by_name
            .insert((schema.to_string(), def.name.clone()), def.id);
        self.bump(def.id);
        self.indexes.insert(def.id, def);
    }

    fn remove_index(&mut self, id: IndexId) -> Option<IndexDef> {
        let def = self.indexes.remove(&id)?;
        let schema = self
            .by_id
            .get(&def.table)
            .map(|entry| entry.schema.clone())
            .unwrap_or_default();
        self.index_by_name.remove(&(schema, def.name.clone()));
        Some(def)
    }
}

/// The system catalog: name resolution, schema lookup and index metadata,
/// stored as ordinary tables through a [`StorageEngine`].
///
/// `ferrite_catalog.ferrite_tables`, `ferrite_columns` and
/// `ferrite_indexes` hold the metadata and describe themselves, so a
/// bootstrapped database has no metadata living outside the normal
/// storage path. Storage is the source of truth — an in-memory
/// index is kept alongside it purely so name lookups on the hot path do
/// not scan, and [`SystemCatalog::open`] rebuilds that index from storage
/// alone.
///
/// Every method takes `&self` and uses interior mutability, matching the
/// [`Catalog`] contract: the executor shares one catalog behind an
/// `Arc`/`&dyn Catalog` across statements and connections.
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
            cache: RwLock::new(Cache::empty()),
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
            cache: RwLock::new(Cache::empty()),
        };
        catalog.reload()?;
        Ok(catalog)
    }

    /// Discard the in-memory index and rebuild it from the catalog
    /// tables. Also the recovery path after a transaction that ran DDL
    /// through one of the `*_in` methods was aborted.
    pub fn reload(&self) -> Result<(), FerriteError> {
        let txn = self.storage.begin()?;
        let result = self.read_all(txn);
        let (tables, indexes) = self.finish(txn, result)?;

        let mut cache = self.write_cache()?;
        *cache = Cache::empty();
        for entry in tables {
            cache.insert_table(entry);
        }
        for def in indexes {
            let schema = cache
                .by_id
                .get(&def.table)
                .map(|entry| entry.schema.clone())
                .unwrap_or_default();
            cache.insert_index(def, &schema);
        }
        Ok(())
    }

    /// Whether an id falls in the range reserved for the catalog's own
    /// objects, for callers that need to treat them specially
    /// (`ferrite-exec` refusing writes, for example).
    pub fn is_system_table(table: TableId) -> bool {
        table < FIRST_USER_TABLE_ID
    }

    /// [`Catalog::create_table`], but joining a transaction the caller
    /// owns instead of opening one.
    ///
    /// This is the transactional form of catalog DDL: the trait method
    /// cannot take a `TxnId` (see the crate README), so it wraps this in
    /// its own transaction. The in-memory index is updated optimistically
    /// — if `txn` is later aborted, call [`SystemCatalog::reload`].
    pub fn create_table_in(
        &self,
        txn: TxnId,
        schema: &str,
        name: &str,
        columns: Schema,
    ) -> Result<TableId, FerriteError> {
        Self::validate_table(schema, name, &columns)?;
        let id = self.reserve_id(schema, name)?;
        self.storage.create_table(txn, id)?;
        self.record_table(txn, id, schema, name)?;
        self.record_columns(txn, id, &columns)?;
        self.write_cache()?.insert_table(Entry {
            id,
            schema: schema.to_string(),
            name: name.to_string(),
            columns,
        });
        Ok(id)
    }

    /// Append a column to an existing table's stored schema, joining a
    /// caller-owned transaction. This is the catalog half of
    /// `ALTER TABLE … ADD COLUMN`. See [`SystemCatalog::create_table_in`]
    /// for the abort caveat.
    ///
    /// One row is added to `ferrite_columns`, at the next free ordinal —
    /// the column goes on the end and nothing already stored moves.
    /// **Rows already written keep the arity they had**: the catalog does
    /// not touch table data, and the reader is what reconciles the two
    /// (`ferrite-exec` fills the missing trailing values with the column's
    /// default). That is Postgres's behaviour too, and it is what makes
    /// this an `O(1)` operation on a table of any size.
    pub fn add_column_in(
        &self,
        txn: TxnId,
        table: TableId,
        column: ColumnDef,
    ) -> Result<(), FerriteError> {
        if Self::is_system_table(table) {
            return Err(FerriteError::PermissionDenied(format!(
                "table {table} belongs to the system catalog and cannot be altered"
            )));
        }
        if column.name.is_empty() {
            return Err(catalog_error("column names must not be empty"));
        }
        let position = {
            let cache = self.read_cache()?;
            let entry = cache
                .by_id
                .get(&table)
                .ok_or_else(|| FerriteError::TableNotFound(table.to_string()))?;
            if entry.columns.column_index(&column.name).is_some() {
                return Err(FerriteError::ObjectAlreadyExists(format!(
                    "column `{}.{}.{}`",
                    entry.schema, entry.name, column.name
                )));
            }
            entry.columns.columns.len()
        };

        self.record_column(txn, table, position, &column)?;
        if let Some(entry) = self.write_cache()?.by_id.get_mut(&table) {
            entry.columns.columns.push(column);
        }
        Ok(())
    }

    /// [`SystemCatalog::add_column_in`] in a transaction of the catalog's
    /// own. [`Catalog`] has no `add_column`, so this is an inherent method
    /// the same way the `*_in` primitives are.
    pub fn add_column(&self, table: TableId, column: ColumnDef) -> Result<(), FerriteError> {
        self.in_own_txn(|txn| self.add_column_in(txn, table, column))
    }

    /// [`Catalog::drop_table`], joining a caller-owned transaction. Drops
    /// the table's indexes with it. See [`SystemCatalog::create_table_in`]
    /// for the abort caveat.
    pub fn drop_table_in(&self, txn: TxnId, table: TableId) -> Result<(), FerriteError> {
        self.check_droppable(table)?;
        for index in self.indexes_for(table)? {
            self.drop_index_in(txn, index.id)?;
        }
        self.delete_rows(txn, COLUMNS_TABLE_ID, 0, table)?;
        self.delete_rows(txn, TABLES_TABLE_ID, 0, table)?;
        self.storage.drop_table(txn, table)?;
        self.write_cache()?.remove_table(table);
        Ok(())
    }

    /// [`IndexCatalog::create_index`], joining a caller-owned
    /// transaction. See [`SystemCatalog::create_table_in`] for the abort
    /// caveat.
    pub fn create_index_in(
        &self,
        txn: TxnId,
        name: &str,
        table: TableId,
        columns: &[String],
        unique: bool,
    ) -> Result<IndexId, FerriteError> {
        let schema = self.validate_index(name, table, columns)?;
        let id = {
            let mut cache = self.write_cache()?;
            let id = cache.next_id;
            cache.next_id += 1;
            id
        };
        for (position, column) in columns.iter().enumerate() {
            self.storage.insert_unique(
                txn,
                INDEXES_TABLE_ID,
                Row::new(vec![
                    Value::Int8(i64::from(id)),
                    Value::Int8(i64::from(table)),
                    Value::Text(name.to_string()),
                    Value::Int4(to_ordinal(position)?),
                    Value::Text(column.clone()),
                    Value::Boolean(unique),
                ]),
                &indexes_keys(),
            )?;
        }
        self.write_cache()?.insert_index(
            IndexDef {
                id,
                name: name.to_string(),
                table,
                columns: columns.to_vec(),
                unique,
            },
            &schema,
        );
        Ok(id)
    }

    /// [`IndexCatalog::drop_index`], joining a caller-owned transaction.
    /// See [`SystemCatalog::create_table_in`] for the abort caveat.
    pub fn drop_index_in(&self, txn: TxnId, index: IndexId) -> Result<(), FerriteError> {
        {
            let cache = self.read_cache()?;
            if !cache.indexes.contains_key(&index) {
                return Err(catalog_error(format!("index {index} does not exist")));
            }
        }
        self.delete_rows(txn, INDEXES_TABLE_ID, 0, index)?;
        self.write_cache()?.remove_index(index);
        Ok(())
    }

    fn bootstrap_in(&self, txn: TxnId) -> Result<(), FerriteError> {
        let catalog_tables = [
            (TABLES_TABLE_ID, TABLES_TABLE_NAME, tables_schema()),
            (COLUMNS_TABLE_ID, COLUMNS_TABLE_NAME, columns_schema()),
            (INDEXES_TABLE_ID, INDEXES_TABLE_NAME, indexes_schema()),
        ];
        // Every table must exist before anything is written into them:
        // the first table's own metadata already needs the columns table.
        for (id, _, _) in &catalog_tables {
            self.storage.create_table(txn, *id)?;
        }
        for (id, name, schema) in &catalog_tables {
            self.record_table(txn, *id, CATALOG_SCHEMA, name)?;
            self.record_columns(txn, *id, schema)?;
        }
        Ok(())
    }

    fn record_table(
        &self,
        txn: TxnId,
        id: TableId,
        schema: &str,
        name: &str,
    ) -> Result<RowId, FerriteError> {
        self.storage.insert_unique(
            txn,
            TABLES_TABLE_ID,
            Row::new(vec![
                Value::Int8(i64::from(id)),
                Value::Text(schema.to_string()),
                Value::Text(name.to_string()),
            ]),
            &tables_keys(),
        )
    }

    fn record_columns(&self, txn: TxnId, id: TableId, schema: &Schema) -> Result<(), FerriteError> {
        for (position, col) in schema.columns.iter().enumerate() {
            self.record_column(txn, id, position, col)?;
        }
        Ok(())
    }

    fn record_column(
        &self,
        txn: TxnId,
        id: TableId,
        position: usize,
        col: &ColumnDef,
    ) -> Result<(), FerriteError> {
        let default = match &col.default {
            Some(default) => Value::Text(encode_default(default, col.data_type)?),
            None => Value::Null,
        };
        self.storage.insert_unique(
            txn,
            COLUMNS_TABLE_ID,
            Row::new(vec![
                Value::Int8(i64::from(id)),
                Value::Int4(to_ordinal(position)?),
                Value::Text(col.name.clone()),
                Value::Text(type_name(col.data_type).to_string()),
                Value::Boolean(col.nullable),
                default,
            ]),
            &columns_keys(),
        )?;
        Ok(())
    }

    #[allow(clippy::type_complexity)]
    fn read_all(&self, txn: TxnId) -> Result<(Vec<Entry>, Vec<IndexDef>), FerriteError> {
        let mut columns: HashMap<TableId, Vec<(usize, ColumnDef)>> = HashMap::new();
        for row in self.storage.scan(txn, COLUMNS_TABLE_ID)? {
            let (_, row) = row?;
            let id = object_id(&row, 0)?;
            let position = ordinal(&row, 1)?;
            let data_type = type_from_name(&text(&row, 3)?)?;
            let def = ColumnDef {
                name: text(&row, 2)?,
                data_type,
                nullable: boolean(&row, 4)?,
                default: optional_text(&row, 5)?
                    .map(|encoded| decode_default(&encoded, data_type))
                    .transpose()?,
            };
            columns.entry(id).or_default().push((position, def));
        }

        let mut tables = Vec::new();
        for row in self.storage.scan(txn, TABLES_TABLE_ID)? {
            let (_, row) = row?;
            let id = object_id(&row, 0)?;
            let mut cols = columns.remove(&id).unwrap_or_default();
            cols.sort_by_key(|(position, _)| *position);
            tables.push(Entry {
                id,
                schema: text(&row, 1)?,
                name: text(&row, 2)?,
                columns: Schema {
                    columns: cols.into_iter().map(|(_, def)| def).collect(),
                },
            });
        }

        let mut parts: HashMap<IndexId, (String, TableId, bool, Vec<(usize, String)>)> =
            HashMap::new();
        for row in self.storage.scan(txn, INDEXES_TABLE_ID)? {
            let (_, row) = row?;
            let id = object_id(&row, 0)?;
            let entry = parts
                .entry(id)
                .or_insert_with(|| (String::new(), 0, false, Vec::new()));
            entry.0 = text(&row, 2)?;
            entry.1 = object_id(&row, 1)?;
            entry.2 = boolean(&row, 5)?;
            entry.3.push((ordinal(&row, 3)?, text(&row, 4)?));
        }
        let indexes = parts
            .into_iter()
            .map(|(id, (name, table, unique, mut cols))| {
                cols.sort_by_key(|(position, _)| *position);
                IndexDef {
                    id,
                    name,
                    table,
                    columns: cols.into_iter().map(|(_, name)| name).collect(),
                    unique,
                }
            })
            .collect();

        Ok((tables, indexes))
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

    /// Run `body` in a transaction of the catalog's own, rolling the
    /// in-memory index back by reloading if it fails.
    fn in_own_txn<T>(
        &self,
        body: impl FnOnce(TxnId) -> Result<T, FerriteError>,
    ) -> Result<T, FerriteError> {
        let txn = self.storage.begin()?;
        match self.finish(txn, body(txn)) {
            Ok(value) => Ok(value),
            Err(err) => {
                let _ = self.reload();
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

    /// Claim the next object id while holding the write lock, so two
    /// concurrent creates cannot be handed the same one.
    fn reserve_id(&self, schema: &str, name: &str) -> Result<TableId, FerriteError> {
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
        cache.next_id += 1;
        Ok(id)
    }

    fn check_droppable(&self, table: TableId) -> Result<(), FerriteError> {
        if Self::is_system_table(table) {
            return Err(FerriteError::PermissionDenied(format!(
                "table {table} belongs to the system catalog and cannot be dropped"
            )));
        }
        if !self.read_cache()?.by_id.contains_key(&table) {
            return Err(FerriteError::TableNotFound(table.to_string()));
        }
        Ok(())
    }

    fn validate_table(schema: &str, name: &str, columns: &Schema) -> Result<(), FerriteError> {
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
        for (position, col) in columns.columns.iter().enumerate() {
            if col.name.is_empty() {
                return Err(catalog_error("column names must not be empty"));
            }
            if columns.columns[..position]
                .iter()
                .any(|c| c.name == col.name)
            {
                return Err(catalog_error(format!("duplicate column `{}`", col.name)));
            }
        }
        Ok(())
    }

    /// Returns the schema the index will live in (its table's).
    fn validate_index(
        &self,
        name: &str,
        table: TableId,
        columns: &[String],
    ) -> Result<String, FerriteError> {
        if name.is_empty() {
            return Err(catalog_error("index names must not be empty"));
        }
        if columns.is_empty() {
            return Err(catalog_error("an index needs at least one column"));
        }
        let cache = self.read_cache()?;
        let entry = cache
            .by_id
            .get(&table)
            .ok_or_else(|| FerriteError::TableNotFound(table.to_string()))?;
        if Self::is_system_table(table) {
            return Err(FerriteError::PermissionDenied(format!(
                "table {table} belongs to the system catalog and cannot be indexed"
            )));
        }
        for (position, column) in columns.iter().enumerate() {
            if entry.columns.column_index(column).is_none() {
                return Err(FerriteError::ColumnNotFound(column.clone()));
            }
            if columns[..position].contains(column) {
                return Err(catalog_error(format!(
                    "duplicate column `{column}` in index `{name}`"
                )));
            }
        }
        if cache
            .index_by_name
            .contains_key(&(entry.schema.clone(), name.to_string()))
        {
            return Err(catalog_error(format!(
                "index `{}.{name}` already exists",
                entry.schema
            )));
        }
        Ok(entry.schema.clone())
    }

    /// Delete every row of `table` whose column `column` holds `wanted`.
    fn delete_rows(
        &self,
        txn: TxnId,
        table: TableId,
        column: usize,
        wanted: TableId,
    ) -> Result<(), FerriteError> {
        let mut doomed = Vec::new();
        for row in self.storage.scan(txn, table)? {
            let (row_id, row) = row?;
            if object_id(&row, column)? == wanted {
                doomed.push(row_id);
            }
        }
        for row_id in doomed {
            self.storage.delete(txn, table, row_id)?;
        }
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
        self.in_own_txn(|txn| self.create_table_in(txn, schema, name, columns))
    }

    fn drop_table(&self, table: TableId) -> Result<(), FerriteError> {
        self.in_own_txn(|txn| self.drop_table_in(txn, table))
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

impl IndexCatalog for SystemCatalog {
    fn create_index(
        &self,
        name: &str,
        table: TableId,
        columns: &[String],
        unique: bool,
    ) -> Result<IndexId, FerriteError> {
        self.in_own_txn(|txn| self.create_index_in(txn, name, table, columns, unique))
    }

    fn drop_index(&self, index: IndexId) -> Result<(), FerriteError> {
        self.in_own_txn(|txn| self.drop_index_in(txn, index))
    }

    fn index(&self, index: IndexId) -> Result<Option<IndexDef>, FerriteError> {
        Ok(self.read_cache()?.indexes.get(&index).cloned())
    }

    fn index_by_name(&self, schema: &str, name: &str) -> Result<Option<IndexDef>, FerriteError> {
        let cache = self.read_cache()?;
        Ok(cache
            .index_by_name
            .get(&(schema.to_string(), name.to_string()))
            .and_then(|id| cache.indexes.get(id))
            .cloned())
    }

    fn indexes_for(&self, table: TableId) -> Result<Vec<IndexDef>, FerriteError> {
        let cache = self.read_cache()?;
        let mut found: Vec<IndexDef> = cache
            .indexes
            .values()
            .filter(|def| def.table == table)
            .cloned()
            .collect();
        found.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(found)
    }
}
