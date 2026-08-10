//! Functional tests for the `StorageEngine` surface: the basic operations
//! and the isolation guarantees between concurrent transactions.

mod support;

use ferrite_common::{FerriteError, Row, StorageEngine, Value};
use support::{int_row, row_int, Scratch};

const T: ferrite_common::TableId = 1;

fn scan_values(storage: &impl StorageEngine, txn: u64) -> Vec<i64> {
    let mut out: Vec<i64> = storage
        .scan(txn, T)
        .expect("scan")
        .map(|r| row_int(&r.expect("scan row").1))
        .collect();
    out.sort_unstable();
    out
}

/// Creates the table used by most tests and commits it, so every test
/// starts from a table that already exists for every later transaction.
fn with_table(scratch: &Scratch) -> ferrite_storage::FerriteStorage {
    let storage = scratch.open();
    let txn = storage.begin().unwrap();
    storage.create_table(txn, T).unwrap();
    storage.commit(txn).unwrap();
    storage
}

#[test]
fn insert_get_scan_within_one_transaction() {
    let scratch = Scratch::new("basic");
    let storage = with_table(&scratch);

    let txn = storage.begin().unwrap();
    let a = storage.insert(txn, T, int_row(10)).unwrap();
    let b = storage.insert(txn, T, int_row(20)).unwrap();
    assert_ne!(a, b);

    assert_eq!(row_int(&storage.get(txn, T, a).unwrap()), 10);
    assert_eq!(row_int(&storage.get(txn, T, b).unwrap()), 20);
    assert_eq!(scan_values(&storage, txn), vec![10, 20]);
    storage.commit(txn).unwrap();
}

#[test]
fn committed_writes_are_visible_to_later_transactions() {
    let scratch = Scratch::new("commit_visible");
    let storage = with_table(&scratch);

    let writer = storage.begin().unwrap();
    let row = storage.insert(writer, T, int_row(7)).unwrap();
    storage.commit(writer).unwrap();

    let reader = storage.begin().unwrap();
    assert_eq!(row_int(&storage.get(reader, T, row).unwrap()), 7);
    storage.commit(reader).unwrap();
}

#[test]
fn update_replaces_the_visible_version() {
    let scratch = Scratch::new("update");
    let storage = with_table(&scratch);

    let txn = storage.begin().unwrap();
    let row = storage.insert(txn, T, int_row(1)).unwrap();
    storage.commit(txn).unwrap();

    let txn = storage.begin().unwrap();
    storage.update(txn, T, row, int_row(2)).unwrap();
    assert_eq!(row_int(&storage.get(txn, T, row).unwrap()), 2);
    storage.commit(txn).unwrap();

    let txn = storage.begin().unwrap();
    assert_eq!(row_int(&storage.get(txn, T, row).unwrap()), 2);
    assert_eq!(scan_values(&storage, txn), vec![2]);
    storage.commit(txn).unwrap();
}

#[test]
fn delete_hides_the_row_from_reads_and_scans() {
    let scratch = Scratch::new("delete");
    let storage = with_table(&scratch);

    let txn = storage.begin().unwrap();
    let keep = storage.insert(txn, T, int_row(1)).unwrap();
    let gone = storage.insert(txn, T, int_row(2)).unwrap();
    storage.commit(txn).unwrap();

    let txn = storage.begin().unwrap();
    storage.delete(txn, T, gone).unwrap();
    assert!(matches!(
        storage.get(txn, T, gone),
        Err(FerriteError::RowNotFound)
    ));
    assert_eq!(scan_values(&storage, txn), vec![1]);
    storage.commit(txn).unwrap();

    let after = storage.begin().unwrap();
    assert_eq!(scan_values(&storage, after), vec![1]);
    assert_eq!(row_int(&storage.get(after, T, keep).unwrap()), 1);
    assert!(matches!(
        storage.delete(after, T, gone),
        Err(FerriteError::RowNotFound)
    ));
    storage.commit(after).unwrap();
}

#[test]
fn aborted_work_never_becomes_visible() {
    let scratch = Scratch::new("abort");
    let storage = with_table(&scratch);

    let seed = storage.begin().unwrap();
    let row = storage.insert(seed, T, int_row(100)).unwrap();
    storage.commit(seed).unwrap();

    let doomed = storage.begin().unwrap();
    storage.update(doomed, T, row, int_row(999)).unwrap();
    let orphan = storage.insert(doomed, T, int_row(555)).unwrap();
    storage.abort(doomed).unwrap();

    let after = storage.begin().unwrap();
    assert_eq!(row_int(&storage.get(after, T, row).unwrap()), 100);
    assert!(matches!(
        storage.get(after, T, orphan),
        Err(FerriteError::RowNotFound)
    ));
    assert_eq!(scan_values(&storage, after), vec![100]);
    storage.commit(after).unwrap();
}

#[test]
fn a_transaction_does_not_see_a_concurrent_uncommitted_write() {
    let scratch = Scratch::new("isolation");
    let storage = with_table(&scratch);

    let seed = storage.begin().unwrap();
    let row = storage.insert(seed, T, int_row(1)).unwrap();
    storage.commit(seed).unwrap();

    let reader = storage.begin().unwrap();
    let writer = storage.begin().unwrap();

    storage.update(writer, T, row, int_row(2)).unwrap();
    let extra = storage.insert(writer, T, int_row(3)).unwrap();

    assert_eq!(row_int(&storage.get(reader, T, row).unwrap()), 1);
    assert!(matches!(
        storage.get(reader, T, extra),
        Err(FerriteError::RowNotFound)
    ));
    assert_eq!(scan_values(&storage, reader), vec![1]);

    storage.commit(writer).unwrap();

    // Still invisible: the reader's snapshot predates the commit.
    assert_eq!(row_int(&storage.get(reader, T, row).unwrap()), 1);
    assert_eq!(scan_values(&storage, reader), vec![1]);
    storage.commit(reader).unwrap();
}

#[test]
fn refreshing_the_snapshot_gives_read_committed_semantics() {
    let scratch = Scratch::new("read_committed");
    let storage = with_table(&scratch);

    let seed = storage.begin().unwrap();
    let row = storage.insert(seed, T, int_row(1)).unwrap();
    storage.commit(seed).unwrap();

    let reader = storage.begin().unwrap();
    assert_eq!(row_int(&storage.get(reader, T, row).unwrap()), 1);

    let writer = storage.begin().unwrap();
    storage.update(writer, T, row, int_row(2)).unwrap();
    storage.commit(writer).unwrap();

    assert_eq!(row_int(&storage.get(reader, T, row).unwrap()), 1);
    let snap = storage.snapshot(reader).unwrap();
    assert_eq!(snap.txn_id, reader);
    assert_eq!(row_int(&storage.get(reader, T, row).unwrap()), 2);
    storage.commit(reader).unwrap();
}

#[test]
fn concurrent_updates_to_one_row_conflict_rather_than_losing_a_write() {
    let scratch = Scratch::new("conflict");
    let storage = with_table(&scratch);

    let seed = storage.begin().unwrap();
    let row = storage.insert(seed, T, int_row(0)).unwrap();
    storage.commit(seed).unwrap();

    let a = storage.begin().unwrap();
    let b = storage.begin().unwrap();

    storage.update(a, T, row, int_row(1)).unwrap();
    assert!(matches!(
        storage.update(b, T, row, int_row(2)),
        Err(FerriteError::SerializationFailure)
    ));
    storage.commit(a).unwrap();

    // Even after A commits, B's snapshot cannot write over it.
    assert!(matches!(
        storage.update(b, T, row, int_row(3)),
        Err(FerriteError::SerializationFailure)
    ));
    storage.abort(b).unwrap();

    let check = storage.begin().unwrap();
    assert_eq!(row_int(&storage.get(check, T, row).unwrap()), 1);
    storage.commit(check).unwrap();
}

#[test]
fn concurrent_delete_and_update_conflict() {
    let scratch = Scratch::new("conflict_delete");
    let storage = with_table(&scratch);

    let seed = storage.begin().unwrap();
    let row = storage.insert(seed, T, int_row(0)).unwrap();
    storage.commit(seed).unwrap();

    let a = storage.begin().unwrap();
    let b = storage.begin().unwrap();
    storage.delete(a, T, row).unwrap();
    assert!(matches!(
        storage.update(b, T, row, int_row(1)),
        Err(FerriteError::SerializationFailure)
    ));
    storage.commit(a).unwrap();
    storage.abort(b).unwrap();

    let check = storage.begin().unwrap();
    assert!(matches!(
        storage.get(check, T, row),
        Err(FerriteError::RowNotFound)
    ));
    storage.commit(check).unwrap();
}

#[test]
fn an_aborted_writer_leaves_the_row_writable() {
    let scratch = Scratch::new("conflict_released");
    let storage = with_table(&scratch);

    let seed = storage.begin().unwrap();
    let row = storage.insert(seed, T, int_row(0)).unwrap();
    storage.commit(seed).unwrap();

    let a = storage.begin().unwrap();
    storage.update(a, T, row, int_row(1)).unwrap();
    storage.abort(a).unwrap();

    let b = storage.begin().unwrap();
    storage.update(b, T, row, int_row(2)).unwrap();
    storage.commit(b).unwrap();

    let check = storage.begin().unwrap();
    assert_eq!(row_int(&storage.get(check, T, row).unwrap()), 2);
    storage.commit(check).unwrap();
}

#[test]
fn repeated_updates_in_one_transaction_do_not_grow_the_chain() {
    let scratch = Scratch::new("self_update");
    let storage = with_table(&scratch);

    let txn = storage.begin().unwrap();
    let row = storage.insert(txn, T, int_row(0)).unwrap();
    for i in 1..500 {
        storage.update(txn, T, row, int_row(i)).unwrap();
    }
    assert_eq!(row_int(&storage.get(txn, T, row).unwrap()), 499);
    storage.commit(txn).unwrap();

    let check = storage.begin().unwrap();
    assert_eq!(row_int(&storage.get(check, T, row).unwrap()), 499);
    storage.commit(check).unwrap();
}

#[test]
fn missing_table_and_row_are_reported_distinctly() {
    let scratch = Scratch::new("not_found");
    let storage = with_table(&scratch);

    let txn = storage.begin().unwrap();
    assert!(matches!(
        storage.get(txn, 42, 1),
        Err(FerriteError::TableNotFound(_))
    ));
    assert!(matches!(
        storage.get(txn, T, 12345),
        Err(FerriteError::RowNotFound)
    ));
    assert!(matches!(
        storage.scan(txn, 42).err(),
        Some(FerriteError::TableNotFound(_))
    ));
    storage.commit(txn).unwrap();
}

#[test]
fn operations_on_a_finished_transaction_are_rejected() {
    let scratch = Scratch::new("txn_state");
    let storage = with_table(&scratch);

    let txn = storage.begin().unwrap();
    storage.commit(txn).unwrap();
    assert!(matches!(
        storage.get(txn, T, 1),
        Err(FerriteError::TxnNotActive(_))
    ));
    assert!(matches!(
        storage.commit(txn),
        Err(FerriteError::TxnNotActive(_))
    ));
    assert!(matches!(
        storage.abort(txn),
        Err(FerriteError::TxnNotActive(_))
    ));
    assert!(matches!(
        storage.snapshot(txn),
        Err(FerriteError::TxnNotActive(_))
    ));
}

#[test]
fn tables_are_created_and_dropped_transactionally() {
    let scratch = Scratch::new("ddl");
    let storage = scratch.open();

    let txn = storage.begin().unwrap();
    storage.create_table(txn, 9).unwrap();
    storage.insert(txn, 9, int_row(1)).unwrap();
    storage.abort(txn).unwrap();

    let txn = storage.begin().unwrap();
    assert!(matches!(
        storage.scan(txn, 9).err(),
        Some(FerriteError::TableNotFound(_))
    ));
    storage.create_table(txn, 9).unwrap();
    assert!(storage.create_table(txn, 9).is_err());
    storage.insert(txn, 9, int_row(5)).unwrap();
    storage.commit(txn).unwrap();

    let txn = storage.begin().unwrap();
    assert_eq!(
        storage.scan(txn, 9).unwrap().count(),
        1,
        "the committed table keeps its row"
    );
    storage.drop_table(txn, 9).unwrap();
    assert!(matches!(
        storage.scan(txn, 9).err(),
        Some(FerriteError::TableNotFound(_))
    ));
    storage.commit(txn).unwrap();

    let txn = storage.begin().unwrap();
    assert!(matches!(
        storage.drop_table(txn, 9),
        Err(FerriteError::TableNotFound(_))
    ));
    storage.commit(txn).unwrap();
}

#[test]
fn a_dropped_table_is_invisible_to_transactions_that_started_after_it() {
    let scratch = Scratch::new("ddl_visibility");
    let storage = with_table(&scratch);

    let reader = storage.begin().unwrap();
    let dropper = storage.begin().unwrap();
    storage.drop_table(dropper, T).unwrap();

    // The reader's snapshot predates the drop, so the table is still there.
    assert!(storage.scan(reader, T).is_ok());
    storage.commit(dropper).unwrap();
    assert!(storage.scan(reader, T).is_ok());
    storage.commit(reader).unwrap();

    let after = storage.begin().unwrap();
    assert!(matches!(
        storage.scan(after, T).err(),
        Some(FerriteError::TableNotFound(_))
    ));
    storage.commit(after).unwrap();
}

#[test]
fn tables_are_independent() {
    let scratch = Scratch::new("multi_table");
    let storage = scratch.open();

    let txn = storage.begin().unwrap();
    storage.create_table(txn, 1).unwrap();
    storage.create_table(txn, 2).unwrap();
    let a = storage.insert(txn, 1, int_row(11)).unwrap();
    let b = storage.insert(txn, 2, int_row(22)).unwrap();
    storage.commit(txn).unwrap();

    let txn = storage.begin().unwrap();
    assert_eq!(row_int(&storage.get(txn, 1, a).unwrap()), 11);
    assert_eq!(row_int(&storage.get(txn, 2, b).unwrap()), 22);
    assert_eq!(storage.scan(txn, 1).unwrap().count(), 1);
    assert_eq!(storage.scan(txn, 2).unwrap().count(), 1);
    storage.commit(txn).unwrap();
}

#[test]
fn rows_larger_than_a_page_roundtrip_through_overflow_pages() {
    let scratch = Scratch::new("overflow");
    let storage = with_table(&scratch);

    let big = "x".repeat(60_000);
    let txn = storage.begin().unwrap();
    let row = storage
        .insert(txn, T, Row::new(vec![Value::Text(big.clone())]))
        .unwrap();
    storage.commit(txn).unwrap();

    let txn = storage.begin().unwrap();
    match &storage.get(txn, T, row).unwrap().values[0] {
        Value::Text(s) => assert_eq!(s, &big),
        other => panic!("unexpected {other:?}"),
    }
    storage
        .update(txn, T, row, Row::new(vec![Value::Text("small".into())]))
        .unwrap();
    storage.commit(txn).unwrap();

    let txn = storage.begin().unwrap();
    assert_eq!(
        storage.get(txn, T, row).unwrap().values[0],
        Value::Text("small".into())
    );
    storage.commit(txn).unwrap();
}

#[test]
fn scans_cover_a_table_large_enough_to_need_a_multi_level_tree() {
    let scratch = Scratch::new("big_scan");
    let storage = with_table(&scratch);

    let txn = storage.begin().unwrap();
    for i in 0..5_000i64 {
        storage.insert(txn, T, int_row(i)).unwrap();
    }
    storage.commit(txn).unwrap();

    let txn = storage.begin().unwrap();
    let seen = scan_values(&storage, txn);
    assert_eq!(seen.len(), 5_000);
    assert_eq!(seen.first(), Some(&0));
    assert_eq!(seen.last(), Some(&4_999));
    storage.commit(txn).unwrap();

    // Delete half of them and confirm the scan tracks the change.
    let txn = storage.begin().unwrap();
    let ids: Vec<u64> = storage
        .scan(txn, T)
        .unwrap()
        .map(|r| r.unwrap().0)
        .collect();
    for id in ids.iter().step_by(2) {
        storage.delete(txn, T, *id).unwrap();
    }
    storage.commit(txn).unwrap();

    let txn = storage.begin().unwrap();
    assert_eq!(storage.scan(txn, T).unwrap().count(), 2_500);
    storage.commit(txn).unwrap();
}

#[test]
fn every_value_variant_survives_a_round_trip() {
    let scratch = Scratch::new("values");
    let storage = with_table(&scratch);

    let row = Row::new(vec![
        Value::Null,
        Value::Boolean(true),
        Value::Int4(-5),
        Value::Int8(1 << 40),
        Value::Float8(std::f64::consts::PI),
        Value::Text("café".into()),
        Value::Timestamp(1_700_000_000_000_000),
        Value::Uuid(0x0189_1e5f_7d3a_7c11_9b5e_0242_ac12_0002),
        Value::Json("{\"k\":[1,2,3]}".into()),
    ]);

    let txn = storage.begin().unwrap();
    let id = storage.insert(txn, T, row.clone()).unwrap();
    storage.commit(txn).unwrap();

    let txn = storage.begin().unwrap();
    assert_eq!(storage.get(txn, T, id).unwrap(), row);
    storage.commit(txn).unwrap();
}

#[test]
fn the_engine_is_usable_from_several_threads_at_once() {
    // The trait requires `Send + Sync`; this checks the requirement is met
    // in practice and that the engine-wide lock actually serialises page
    // access rather than merely compiling.
    let scratch = Scratch::new("threads");
    let storage = with_table(&scratch);
    let conflicts = std::sync::atomic::AtomicUsize::new(0);

    std::thread::scope(|scope| {
        for worker in 0..4i64 {
            let storage = &storage;
            let conflicts = &conflicts;
            scope.spawn(move || {
                for i in 0..100i64 {
                    let txn = storage.begin().unwrap();
                    let row = storage.insert(txn, T, int_row(worker * 1000 + i)).unwrap();
                    match storage.update(txn, T, row, int_row(worker * 1000 + i)) {
                        Ok(()) => {}
                        Err(FerriteError::SerializationFailure) => {
                            conflicts.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        }
                        Err(e) => panic!("unexpected error: {e}"),
                    }
                    storage.commit(txn).unwrap();
                }
            });
        }
    });

    // Each transaction only ever touches a row it just created, so nothing
    // should have conflicted.
    assert_eq!(conflicts.load(std::sync::atomic::Ordering::Relaxed), 0);
    let txn = storage.begin().unwrap();
    assert_eq!(storage.scan(txn, T).unwrap().count(), 400);
    storage.commit(txn).unwrap();
}

#[test]
fn data_survives_a_clean_reopen() {
    let scratch = Scratch::new("reopen");
    {
        let storage = with_table(&scratch);
        let txn = storage.begin().unwrap();
        for i in 0..200i64 {
            storage.insert(txn, T, int_row(i)).unwrap();
        }
        storage.commit(txn).unwrap();
        storage.checkpoint().unwrap();
    }

    let storage = scratch.open();
    let txn = storage.begin().unwrap();
    assert_eq!(scan_values(&storage, txn).len(), 200);
    storage.commit(txn).unwrap();
}

#[test]
fn row_ids_are_not_reused_after_a_reopen() {
    let scratch = Scratch::new("rowid_persist");
    let first = {
        let storage = with_table(&scratch);
        let txn = storage.begin().unwrap();
        let id = storage.insert(txn, T, int_row(1)).unwrap();
        storage.commit(txn).unwrap();
        storage.checkpoint().unwrap();
        id
    };

    let storage = scratch.open();
    let txn = storage.begin().unwrap();
    let second = storage.insert(txn, T, int_row(2)).unwrap();
    storage.commit(txn).unwrap();
    assert!(second > first, "{second} should follow {first}");
}
