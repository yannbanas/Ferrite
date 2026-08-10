use std::sync::Arc;

use ferrite_catalog::memory::MemoryStorage;
use ferrite_catalog::{
    SystemCatalog, CATALOG_SCHEMA, COLUMNS_TABLE_ID, FIRST_USER_TABLE_ID, TABLES_TABLE_ID,
};
use ferrite_common::{Catalog, ColumnDef, DataType, FerriteError, Schema, StorageEngine};

fn schema(columns: &[(&str, DataType, bool)]) -> Schema {
    Schema {
        columns: columns
            .iter()
            .map(|(name, data_type, nullable)| ColumnDef {
                name: (*name).to_string(),
                data_type: *data_type,
                nullable: *nullable,
            })
            .collect(),
    }
}

fn users() -> Schema {
    schema(&[
        ("id", DataType::Uuid, false),
        ("email", DataType::Text, false),
        ("age", DataType::Int4, true),
        ("profile", DataType::Json, true),
    ])
}

fn fresh() -> (Arc<MemoryStorage>, SystemCatalog) {
    let storage = Arc::new(MemoryStorage::new());
    let catalog = SystemCatalog::bootstrap(storage.clone()).expect("bootstrap");
    (storage, catalog)
}

#[test]
fn bootstrap_creates_self_describing_catalog_tables() {
    let (storage, catalog) = fresh();

    assert!(storage.table_exists(TABLES_TABLE_ID).unwrap());
    assert!(storage.table_exists(COLUMNS_TABLE_ID).unwrap());

    assert_eq!(
        catalog.table_id(CATALOG_SCHEMA, "ferrite_tables").unwrap(),
        Some(TABLES_TABLE_ID)
    );
    assert_eq!(
        catalog.table_id(CATALOG_SCHEMA, "ferrite_columns").unwrap(),
        Some(COLUMNS_TABLE_ID)
    );

    let tables = catalog.table_schema(TABLES_TABLE_ID).unwrap();
    assert_eq!(
        tables
            .columns
            .iter()
            .map(|c| c.name.as_str())
            .collect::<Vec<_>>(),
        vec!["table_id", "schema_name", "table_name"]
    );
    let columns = catalog.table_schema(COLUMNS_TABLE_ID).unwrap();
    assert_eq!(columns.columns.len(), 5);
    assert_eq!(columns.columns[4].data_type, DataType::Boolean);

    // Two tables, and one row per column of each.
    assert_eq!(storage.rows(TABLES_TABLE_ID).unwrap().len(), 2);
    assert_eq!(storage.rows(COLUMNS_TABLE_ID).unwrap().len(), 8);

    assert_eq!(
        catalog.list_tables(CATALOG_SCHEMA).unwrap(),
        vec![
            (COLUMNS_TABLE_ID, "ferrite_columns".to_string()),
            (TABLES_TABLE_ID, "ferrite_tables".to_string()),
        ]
    );
}

#[test]
fn bootstrap_twice_fails() {
    let storage = Arc::new(MemoryStorage::new());
    SystemCatalog::bootstrap(storage.clone()).unwrap();
    assert!(SystemCatalog::bootstrap(storage).is_err());
}

#[test]
fn create_then_lookup() {
    let (_, catalog) = fresh();

    assert_eq!(catalog.table_id("public", "users").unwrap(), None);

    let id = catalog.create_table("public", "users", users()).unwrap();
    assert!(
        id >= FIRST_USER_TABLE_ID,
        "user tables start above the reserved range"
    );
    assert_eq!(catalog.table_id("public", "users").unwrap(), Some(id));
    assert_eq!(catalog.table_schema(id).unwrap(), users());

    // Schema-qualified: the same name in another schema is a different table.
    let other = catalog.create_table("app", "users", users()).unwrap();
    assert_ne!(other, id);
    assert_eq!(catalog.table_id("app", "users").unwrap(), Some(other));
    assert_eq!(catalog.table_id("nope", "users").unwrap(), None);
}

#[test]
fn create_allocates_distinct_ids_and_a_storage_table() {
    let (storage, catalog) = fresh();
    let a = catalog.create_table("public", "a", users()).unwrap();
    let b = catalog.create_table("public", "b", users()).unwrap();
    let c = catalog.create_table("public", "c", users()).unwrap();
    assert_ne!(a, b);
    assert_ne!(b, c);
    for id in [a, b, c] {
        assert!(storage.table_exists(id).unwrap());
    }
    assert_eq!(storage.rows(TABLES_TABLE_ID).unwrap().len(), 5);
}

#[test]
fn duplicate_table_is_rejected() {
    let (storage, catalog) = fresh();
    catalog.create_table("public", "users", users()).unwrap();
    let before = storage.rows(TABLES_TABLE_ID).unwrap().len();

    let err = catalog
        .create_table("public", "users", users())
        .unwrap_err();
    assert!(err.to_string().contains("already exists"), "{err}");
    assert_eq!(storage.rows(TABLES_TABLE_ID).unwrap().len(), before);
}

#[test]
fn invalid_definitions_are_rejected() {
    let (_, catalog) = fresh();

    assert!(catalog.create_table("", "t", users()).is_err());
    assert!(catalog.create_table("public", "", users()).is_err());
    assert!(catalog
        .create_table("public", "t", Schema { columns: vec![] })
        .is_err());
    assert!(catalog
        .create_table(
            "public",
            "t",
            schema(&[("a", DataType::Int4, true), ("a", DataType::Text, true)])
        )
        .is_err());
    assert!(catalog
        .create_table("public", "t", schema(&[("", DataType::Int4, true)]))
        .is_err());

    // The catalog's own schema is reserved.
    let err = catalog
        .create_table(CATALOG_SCHEMA, "sneaky", users())
        .unwrap_err();
    assert!(matches!(err, FerriteError::PermissionDenied(_)), "{err}");

    // A rejected create must leave nothing behind.
    assert_eq!(catalog.list_tables("public").unwrap(), vec![]);
}

#[test]
fn drop_removes_table_rows_and_columns() {
    let (storage, catalog) = fresh();
    let id = catalog.create_table("public", "users", users()).unwrap();
    let keep = catalog.create_table("public", "keep", users()).unwrap();

    let tables_before = storage.rows(TABLES_TABLE_ID).unwrap().len();
    let columns_before = storage.rows(COLUMNS_TABLE_ID).unwrap().len();

    catalog.drop_table(id).unwrap();

    assert_eq!(catalog.table_id("public", "users").unwrap(), None);
    assert!(matches!(
        catalog.table_schema(id),
        Err(FerriteError::TableNotFound(_))
    ));
    assert!(!storage.table_exists(id).unwrap());
    assert_eq!(
        storage.rows(TABLES_TABLE_ID).unwrap().len(),
        tables_before - 1
    );
    assert_eq!(
        storage.rows(COLUMNS_TABLE_ID).unwrap().len(),
        columns_before - users().columns.len()
    );

    // The surviving table is untouched.
    assert_eq!(catalog.table_schema(keep).unwrap(), users());
    assert_eq!(
        catalog.list_tables("public").unwrap(),
        vec![(keep, "keep".to_string())]
    );
}

#[test]
fn drop_unknown_or_system_table_is_rejected() {
    let (_, catalog) = fresh();

    assert!(matches!(
        catalog.drop_table(9999),
        Err(FerriteError::TableNotFound(_))
    ));
    for id in [TABLES_TABLE_ID, COLUMNS_TABLE_ID] {
        assert!(
            matches!(
                catalog.drop_table(id),
                Err(FerriteError::PermissionDenied(_))
            ),
            "dropping catalog table {id} must be refused"
        );
    }
    assert_eq!(
        catalog.table_schema(TABLES_TABLE_ID).unwrap().columns.len(),
        3
    );
}

#[test]
fn name_can_be_reused_after_drop() {
    let (_, catalog) = fresh();
    let first = catalog.create_table("public", "users", users()).unwrap();
    catalog.drop_table(first).unwrap();

    let second = catalog
        .create_table("public", "users", schema(&[("only", DataType::Text, true)]))
        .unwrap();
    assert_ne!(first, second, "ids are not recycled");
    assert_eq!(catalog.table_schema(second).unwrap().columns.len(), 1);
}

#[test]
fn list_tables_is_scoped_and_sorted() {
    let (_, catalog) = fresh();
    catalog.create_table("public", "zebra", users()).unwrap();
    catalog.create_table("public", "alpha", users()).unwrap();
    catalog.create_table("app", "mid", users()).unwrap();

    let names: Vec<String> = catalog
        .list_tables("public")
        .unwrap()
        .into_iter()
        .map(|(_, name)| name)
        .collect();
    assert_eq!(names, vec!["alpha".to_string(), "zebra".to_string()]);
    assert_eq!(catalog.list_tables("app").unwrap().len(), 1);
    assert!(catalog.list_tables("missing").unwrap().is_empty());
}

#[test]
fn state_survives_reopening_from_storage_alone() {
    let storage = Arc::new(MemoryStorage::new());
    let ids = {
        let catalog = SystemCatalog::bootstrap(storage.clone()).unwrap();
        let a = catalog.create_table("public", "users", users()).unwrap();
        let b = catalog
            .create_table(
                "app",
                "events",
                schema(&[("at", DataType::Timestamp, false)]),
            )
            .unwrap();
        catalog.drop_table(a).unwrap();
        b
    };

    // A brand-new catalog rebuilt purely by reading the catalog tables.
    let reopened = SystemCatalog::open(storage).unwrap();
    assert_eq!(reopened.table_id("public", "users").unwrap(), None);
    assert_eq!(reopened.table_id("app", "events").unwrap(), Some(ids));
    assert_eq!(
        reopened.table_schema(ids).unwrap(),
        schema(&[("at", DataType::Timestamp, false)])
    );
    assert_eq!(
        reopened
            .table_schema(TABLES_TABLE_ID)
            .unwrap()
            .columns
            .len(),
        3
    );

    // Ids allocated after reopening must not collide with existing ones.
    let next = reopened.create_table("public", "more", users()).unwrap();
    assert!(next > ids);
}

#[test]
fn column_order_and_types_round_trip_through_storage() {
    let storage = Arc::new(MemoryStorage::new());
    let all_types = schema(&[
        ("c_bool", DataType::Boolean, false),
        ("c_int4", DataType::Int4, true),
        ("c_int8", DataType::Int8, false),
        ("c_float8", DataType::Float8, true),
        ("c_text", DataType::Text, false),
        ("c_ts", DataType::Timestamp, true),
        ("c_uuid", DataType::Uuid, false),
        ("c_json", DataType::Json, true),
    ]);
    let id = {
        let catalog = SystemCatalog::bootstrap(storage.clone()).unwrap();
        catalog
            .create_table("public", "every_type", all_types.clone())
            .unwrap()
    };
    let reopened = SystemCatalog::open(storage).unwrap();
    assert_eq!(reopened.table_schema(id).unwrap(), all_types);
}

#[test]
fn reload_picks_up_a_concurrent_writer() {
    let storage = Arc::new(MemoryStorage::new());
    let a = SystemCatalog::bootstrap(storage.clone()).unwrap();
    let b = SystemCatalog::open(storage).unwrap();

    let id = a.create_table("public", "users", users()).unwrap();
    assert_eq!(b.table_id("public", "users").unwrap(), None);
    b.reload().unwrap();
    assert_eq!(b.table_id("public", "users").unwrap(), Some(id));
}

#[test]
fn every_catalog_operation_commits_a_transaction() {
    let (storage, catalog) = fresh();
    let after_bootstrap = storage.committed_transactions().unwrap();
    catalog.create_table("public", "users", users()).unwrap();
    assert_eq!(
        storage.committed_transactions().unwrap(),
        after_bootstrap + 1
    );
    catalog
        .drop_table(catalog.table_id("public", "users").unwrap().unwrap())
        .unwrap();
    assert_eq!(
        storage.committed_transactions().unwrap(),
        after_bootstrap + 2
    );
}

#[test]
fn usable_behind_a_trait_object() {
    let (_, catalog) = fresh();
    let boxed: Box<dyn Catalog> = Box::new(catalog);
    let id = boxed.create_table("public", "users", users()).unwrap();
    assert_eq!(boxed.table_schema(id).unwrap(), users());
}

#[test]
fn memory_storage_rolls_back_on_abort() {
    let storage = MemoryStorage::new();
    let setup = storage.begin().unwrap();
    storage.create_table(setup, 100).unwrap();
    let kept = storage
        .insert(
            setup,
            100,
            ferrite_common::Row::new(vec![ferrite_common::Value::Int4(1)]),
        )
        .unwrap();
    storage.commit(setup).unwrap();

    let txn = storage.begin().unwrap();
    storage
        .insert(
            txn,
            100,
            ferrite_common::Row::new(vec![ferrite_common::Value::Int4(2)]),
        )
        .unwrap();
    storage
        .update(
            txn,
            100,
            kept,
            ferrite_common::Row::new(vec![ferrite_common::Value::Int4(99)]),
        )
        .unwrap();
    storage.create_table(txn, 101).unwrap();
    storage.drop_table(txn, 100).unwrap();
    storage.abort(txn).unwrap();

    assert!(!storage.table_exists(101).unwrap());
    assert_eq!(
        storage.rows(100).unwrap(),
        vec![ferrite_common::Row::new(vec![ferrite_common::Value::Int4(
            1
        )])]
    );

    // Operations outside an active transaction are refused.
    assert!(matches!(
        storage.insert(txn, 100, ferrite_common::Row::new(vec![])),
        Err(FerriteError::TxnNotActive(_))
    ));
}
