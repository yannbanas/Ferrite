//! Unique-constraint enforcement, which is the one storage guarantee that
//! cannot be reconstructed above the engine lock.
//!
//! Two properties are checked separately because they fail for different
//! reasons. A duplicate must be refused even when the row holding the key
//! is *invisible* to the writer's snapshot — that is a visibility bug, and
//! a plain `scan` cannot see past it. And two writers racing on the same
//! key must not both win — that is a time-of-check/time-of-use bug, and no
//! amount of checking before the write fixes it.

mod support;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};

use ferrite_common::{FerriteError, Row, StorageEngine, UniqueKey, Value};
use support::Scratch;

const T: ferrite_common::TableId = 1;

fn key() -> Vec<UniqueKey> {
    vec![UniqueKey::new("users_pkey", vec![0])]
}

fn user(id: &str) -> Row {
    Row::new(vec![Value::Text(id.to_owned()), Value::Text("demo".into())])
}

fn with_table(scratch: &Scratch) -> ferrite_storage::FerriteStorage {
    let storage = scratch.open();
    let txn = storage.begin().unwrap();
    storage.create_table(txn, T).unwrap();
    storage.commit(txn).unwrap();
    storage
}

/// The bug the PawChat replay reproduced, at the storage layer: inserting
/// the same `users` key twice used to produce two rows.
#[test]
fn a_duplicate_key_is_refused_in_series() {
    let scratch = Scratch::new("unique_series");
    let storage = with_table(&scratch);

    let txn = storage.begin().unwrap();
    storage
        .insert_unique(txn, T, user("demo-1"), &key())
        .expect("the first row is fine");
    let again = storage.insert_unique(txn, T, user("demo-1"), &key());
    assert!(
        matches!(again, Err(FerriteError::UniqueViolation { .. })),
        "a duplicate key must be refused, got {again:?}"
    );
    storage.commit(txn).unwrap();

    let reader = storage.begin().unwrap();
    assert_eq!(storage.scan(reader, T).unwrap().count(), 1);
    storage.commit(reader).unwrap();
}

/// The same, across transactions, which is how an application actually
/// hits it: register, then register again.
#[test]
fn a_duplicate_key_is_refused_across_transactions() {
    let scratch = Scratch::new("unique_across");
    let storage = with_table(&scratch);

    let first = storage.begin().unwrap();
    storage
        .insert_unique(first, T, user("demo-1"), &key())
        .unwrap();
    storage.commit(first).unwrap();

    let second = storage.begin().unwrap();
    assert!(matches!(
        storage.insert_unique(second, T, user("demo-1"), &key()),
        Err(FerriteError::UniqueViolation { .. })
    ));
    storage.commit(second).unwrap();
}

/// A snapshot taken before the conflicting row committed cannot see it.
/// Checking uniqueness through an ordinary scan would therefore find
/// nothing and let the duplicate in; the check has to look past the
/// snapshot.
#[test]
fn a_row_invisible_to_the_writers_snapshot_still_conflicts() {
    let scratch = Scratch::new("unique_invisible");
    let storage = with_table(&scratch);

    // Opened first, so its snapshot predates everything below.
    let late = storage.begin().unwrap();

    let early = storage.begin().unwrap();
    storage
        .insert_unique(early, T, user("demo-1"), &key())
        .unwrap();
    storage.commit(early).unwrap();

    assert_eq!(
        storage.scan(late, T).unwrap().count(),
        0,
        "the row must be invisible to this snapshot, or the test proves nothing"
    );
    assert!(
        matches!(
            storage.insert_unique(late, T, user("demo-1"), &key()),
            Err(FerriteError::UniqueViolation { .. })
        ),
        "uniqueness is a property of the table, not of one snapshot"
    );
    storage.abort(late).unwrap();
}

/// Neither transaction can see the other's uncommitted row, so without an
/// enforcement that looks past visibility both would commit and the table
/// would end up with two rows carrying one key.
#[test]
fn two_in_flight_transactions_cannot_both_take_the_key() {
    let scratch = Scratch::new("unique_inflight");
    let storage = with_table(&scratch);

    let a = storage.begin().unwrap();
    let b = storage.begin().unwrap();

    storage.insert_unique(a, T, user("demo-1"), &key()).unwrap();
    let second = storage.insert_unique(b, T, user("demo-1"), &key());
    assert!(
        matches!(second, Err(FerriteError::UniqueViolation { .. })),
        "got {second:?}"
    );

    storage.commit(a).unwrap();
    storage.abort(b).unwrap();

    let reader = storage.begin().unwrap();
    assert_eq!(storage.scan(reader, T).unwrap().count(), 1);
    storage.commit(reader).unwrap();
}

/// The time-of-check/time-of-use case: eight threads, one key, started
/// together. Exactly one may win, however the scheduler interleaves them.
#[test]
fn concurrent_inserts_of_one_key_produce_exactly_one_row() {
    const THREADS: usize = 8;

    let scratch = Scratch::new("unique_race");
    let storage = Arc::new(with_table(&scratch));
    let barrier = Arc::new(Barrier::new(THREADS));
    let wins = Arc::new(AtomicUsize::new(0));

    let mut handles = Vec::new();
    for _ in 0..THREADS {
        let storage = Arc::clone(&storage);
        let barrier = Arc::clone(&barrier);
        let wins = Arc::clone(&wins);
        handles.push(std::thread::spawn(move || {
            let txn = storage.begin().expect("begin");
            barrier.wait();
            match storage.insert_unique(txn, T, user("demo-1"), &key()) {
                Ok(_) => {
                    storage.commit(txn).expect("commit");
                    wins.fetch_add(1, Ordering::SeqCst);
                }
                Err(FerriteError::UniqueViolation { .. }) => {
                    storage.abort(txn).expect("abort");
                }
                Err(other) => panic!("unexpected error: {other:?}"),
            }
        }));
    }
    for handle in handles {
        handle.join().expect("thread");
    }

    assert_eq!(wins.load(Ordering::SeqCst), 1, "exactly one insert may win");
    let reader = storage.begin().unwrap();
    assert_eq!(storage.scan(reader, T).unwrap().count(), 1);
    storage.commit(reader).unwrap();
}

/// A transaction that rolls back leaves the key free. The negative cache
/// remembers the hash, so this is also the test that the cache never
/// decides an answer on its own.
#[test]
fn an_aborted_insert_releases_the_key() {
    let scratch = Scratch::new("unique_abort");
    let storage = with_table(&scratch);

    let doomed = storage.begin().unwrap();
    storage
        .insert_unique(doomed, T, user("demo-1"), &key())
        .unwrap();
    storage.abort(doomed).unwrap();

    let txn = storage.begin().unwrap();
    storage
        .insert_unique(txn, T, user("demo-1"), &key())
        .expect("the aborted row never existed");
    storage.commit(txn).unwrap();
}

#[test]
fn deleting_a_row_frees_its_key_within_the_same_transaction() {
    let scratch = Scratch::new("unique_delete");
    let storage = with_table(&scratch);

    let first = storage.begin().unwrap();
    let rid = storage
        .insert_unique(first, T, user("demo-1"), &key())
        .unwrap();
    storage.commit(first).unwrap();

    let txn = storage.begin().unwrap();
    storage.delete(txn, T, rid).unwrap();
    storage
        .insert_unique(txn, T, user("demo-1"), &key())
        .expect("the key was released by the delete");
    storage.commit(txn).unwrap();

    let reader = storage.begin().unwrap();
    assert_eq!(storage.scan(reader, T).unwrap().count(), 1);
    storage.commit(reader).unwrap();
}

#[test]
fn nulls_never_collide() {
    let scratch = Scratch::new("unique_null");
    let storage = with_table(&scratch);
    let null_row = || Row::new(vec![Value::Null, Value::Text("x".into())]);

    let txn = storage.begin().unwrap();
    storage.insert_unique(txn, T, null_row(), &key()).unwrap();
    storage
        .insert_unique(txn, T, null_row(), &key())
        .expect("two nulls are not duplicates, as in every SQL unique index");
    storage.commit(txn).unwrap();
}

#[test]
fn an_update_onto_an_existing_key_is_refused_but_onto_its_own_is_not() {
    let scratch = Scratch::new("unique_update");
    let storage = with_table(&scratch);

    let txn = storage.begin().unwrap();
    let a = storage.insert_unique(txn, T, user("a"), &key()).unwrap();
    storage.insert_unique(txn, T, user("b"), &key()).unwrap();
    storage.commit(txn).unwrap();

    let txn = storage.begin().unwrap();
    assert!(matches!(
        storage.update_unique(txn, T, a, user("b"), &key()),
        Err(FerriteError::UniqueViolation { .. })
    ));
    storage
        .update_unique(txn, T, a, user("a"), &key())
        .expect("a row does not conflict with itself");
    storage.commit(txn).unwrap();
}

/// A multi-column key collides only when every column matches.
#[test]
fn a_composite_key_needs_every_column_to_match() {
    let scratch = Scratch::new("unique_composite");
    let storage = with_table(&scratch);
    let composite = vec![UniqueKey::new("both", vec![0, 1])];
    let row = |a: &str, b: &str| Row::new(vec![Value::Text(a.into()), Value::Text(b.into())]);

    let txn = storage.begin().unwrap();
    storage
        .insert_unique(txn, T, row("x", "1"), &composite)
        .unwrap();
    storage
        .insert_unique(txn, T, row("x", "2"), &composite)
        .expect("only the first column matches");
    assert!(matches!(
        storage.insert_unique(txn, T, row("x", "1"), &composite),
        Err(FerriteError::UniqueViolation { .. })
    ));
    storage.commit(txn).unwrap();
}

/// The negative cache is a superset of the live keys or it is wrong. A
/// write that bypassed the check has to invalidate it, which this proves
/// by inserting through the unchecked path and then colliding with it.
#[test]
fn an_unchecked_insert_does_not_blind_the_next_check() {
    let scratch = Scratch::new("unique_bypass");
    let storage = with_table(&scratch);

    // Builds the filter, so that a stale one would answer "absent".
    let warmup = storage.begin().unwrap();
    storage
        .insert_unique(warmup, T, user("other"), &key())
        .unwrap();
    storage.commit(warmup).unwrap();

    let sneaky = storage.begin().unwrap();
    storage.insert(sneaky, T, user("demo-1")).unwrap();
    storage.commit(sneaky).unwrap();

    let txn = storage.begin().unwrap();
    assert!(
        matches!(
            storage.insert_unique(txn, T, user("demo-1"), &key()),
            Err(FerriteError::UniqueViolation { .. })
        ),
        "the filter must be rebuilt after a write it did not see"
    );
    storage.commit(txn).unwrap();
}
