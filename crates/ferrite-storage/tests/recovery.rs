//! Crash-recovery tests.
//!
//! The engine never writes anything back on `Drop`, so dropping a
//! `FerriteStorage` leaves the data file and journal in exactly the state a
//! power cut would. Most tests here use that; `killed_process_leaves_a_
//! consistent_database` goes further and has the operating system kill a
//! real child process mid-flight, so the result does not depend on that
//! property of `Drop` being true.
//!
//! What is asserted is what the storage engine actually promises: work that
//! committed is still there, work that had not committed is absent, and
//! nothing in between — no torn rows, no unreadable pages, no phantom
//! transactions.

mod support;

use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;

use ferrite_common::{FerriteError, StorageEngine};
use support::{int_row, row_int, Scratch};

const T: ferrite_common::TableId = 1;
const CHILD_ENV: &str = "FERRITE_CRASH_TEST_DIR";
const READY_MARKER: &str = "child-ready";

fn values(storage: &impl StorageEngine, txn: u64) -> Vec<i64> {
    let mut out: Vec<i64> = storage
        .scan(txn, T)
        .expect("scan")
        .map(|r| row_int(&r.expect("scan row").1))
        .collect();
    out.sort_unstable();
    out
}

#[test]
fn committed_work_survives_an_uncommitted_transaction_disappearing() {
    let scratch = Scratch::new("crash_basic");
    {
        let storage = scratch.open();
        let setup = storage.begin().unwrap();
        storage.create_table(setup, T).unwrap();
        storage.commit(setup).unwrap();

        let committed = storage.begin().unwrap();
        for i in 0..50i64 {
            storage.insert(committed, T, int_row(i)).unwrap();
        }
        storage.commit(committed).unwrap();

        // Left in flight when the engine vanishes.
        let doomed = storage.begin().unwrap();
        for i in 1000..1050i64 {
            storage.insert(doomed, T, int_row(i)).unwrap();
        }
        std::mem::drop(storage);
    }

    let storage = scratch.open();
    let txn = storage.begin().unwrap();
    let seen = values(&storage, txn);
    assert_eq!(seen.len(), 50, "only the committed rows should be back");
    assert_eq!(seen, (0..50).collect::<Vec<i64>>());
    storage.commit(txn).unwrap();
}

#[test]
fn an_interrupted_update_leaves_the_previous_version_readable() {
    let scratch = Scratch::new("crash_update");
    let row = {
        let storage = scratch.open();
        let setup = storage.begin().unwrap();
        storage.create_table(setup, T).unwrap();
        let row = storage.insert(setup, T, int_row(1)).unwrap();
        storage.commit(setup).unwrap();

        let doomed = storage.begin().unwrap();
        storage.update(doomed, T, row, int_row(2)).unwrap();
        storage.delete(doomed, T, row).ok();
        row
    };

    let storage = scratch.open();
    let txn = storage.begin().unwrap();
    assert_eq!(row_int(&storage.get(txn, T, row).unwrap()), 1);
    assert_eq!(values(&storage, txn), vec![1]);
    // The row is writable again: the interrupted transaction holds nothing.
    storage.update(txn, T, row, int_row(3)).unwrap();
    storage.commit(txn).unwrap();
}

#[test]
fn recovery_is_idempotent_across_repeated_crashes() {
    let scratch = Scratch::new("crash_repeat");
    {
        let storage = scratch.open();
        let setup = storage.begin().unwrap();
        storage.create_table(setup, T).unwrap();
        storage.commit(setup).unwrap();
    }
    for round in 0..5i64 {
        let storage = scratch.open();
        let txn = storage.begin().unwrap();
        storage.insert(txn, T, int_row(round)).unwrap();
        storage.commit(txn).unwrap();

        let doomed = storage.begin().unwrap();
        storage.insert(doomed, T, int_row(-round - 1)).unwrap();
        std::mem::drop(storage);

        let storage = scratch.open();
        let txn = storage.begin().unwrap();
        assert_eq!(values(&storage, txn), (0..=round).collect::<Vec<i64>>());
        storage.commit(txn).unwrap();
    }
}

#[test]
fn a_crashed_transaction_id_is_never_handed_out_again() {
    // With a small cache, an uncommitted transaction's pages are evicted —
    // and therefore journalled — long before it would have committed. If
    // the allocator's `next_txn_id` were not durable at that moment, the
    // restarted engine would reissue that id and the new transaction would
    // adopt the crashed one's row versions on commit.
    let scratch = Scratch::new("crash_txn_reuse");
    let doomed_id = {
        let storage = ferrite_storage::FerriteStorage::open_with(
            scratch.path(),
            ferrite_storage::StorageConfig {
                cache_pages: 8,
                fsync: false,
                ..Default::default()
            },
        )
        .unwrap();
        let setup = storage.begin().unwrap();
        storage.create_table(setup, T).unwrap();
        storage.insert(setup, T, int_row(1)).unwrap();
        storage.commit(setup).unwrap();

        let doomed = storage.begin().unwrap();
        for i in 0..4_000i64 {
            storage.insert(doomed, T, int_row(100_000 + i)).unwrap();
        }
        doomed
    };

    let storage = scratch.open();
    let fresh = storage.begin().unwrap();
    assert!(
        fresh > doomed_id,
        "transaction {fresh} reuses the crashed id {doomed_id}"
    );
    assert_eq!(values(&storage, fresh), vec![1]);
    storage.insert(fresh, T, int_row(2)).unwrap();
    storage.commit(fresh).unwrap();

    let check = storage.begin().unwrap();
    assert_eq!(
        values(&storage, check),
        vec![1, 2],
        "committing under a reissued id would resurrect the crashed rows"
    );
    storage.commit(check).unwrap();
}

#[test]
fn alternating_crash_sizes_do_not_strand_old_journal_records() {
    // Each cycle rewrites the journal from scratch. If a shorter run left
    // an older, longer run's records behind it, a later replay could apply
    // a stale page image over current data.
    let scratch = Scratch::new("crash_journal_reuse");
    {
        let storage = scratch.open();
        let setup = storage.begin().unwrap();
        storage.create_table(setup, T).unwrap();
        storage.commit(setup).unwrap();
    }

    let mut expected: Vec<i64> = Vec::new();
    for round in 0..6i64 {
        let batch = if round % 2 == 0 { 400 } else { 3 };
        let storage = scratch.open();
        let txn = storage.begin().unwrap();
        for i in 0..batch {
            let value = round * 10_000 + i;
            storage.insert(txn, T, int_row(value)).unwrap();
            expected.push(value);
        }
        storage.commit(txn).unwrap();

        let doomed = storage.begin().unwrap();
        storage.insert(doomed, T, int_row(-1)).unwrap();
        std::mem::drop(storage);

        let storage = scratch.open();
        let txn = storage.begin().unwrap();
        let mut want = expected.clone();
        want.sort_unstable();
        assert_eq!(values(&storage, txn), want, "after round {round}");
        storage.commit(txn).unwrap();
    }
}

#[test]
fn a_checkpoint_makes_the_data_file_self_sufficient() {
    let scratch = Scratch::new("crash_checkpoint");
    {
        let storage = scratch.open();
        let setup = storage.begin().unwrap();
        storage.create_table(setup, T).unwrap();
        for i in 0..300i64 {
            storage.insert(setup, T, int_row(i)).unwrap();
        }
        storage.commit(setup).unwrap();
        storage.checkpoint().unwrap();
    }

    let journal = scratch.path().join(ferrite_storage::JOURNAL_FILE);
    assert_eq!(
        std::fs::metadata(&journal).unwrap().len(),
        0,
        "a checkpoint should leave nothing to replay"
    );

    let storage = scratch.open();
    let txn = storage.begin().unwrap();
    assert_eq!(values(&storage, txn).len(), 300);
    storage.commit(txn).unwrap();
}

#[test]
fn a_torn_journal_tail_is_discarded_without_taking_the_database_with_it() {
    let scratch = Scratch::new("crash_torn");
    {
        let storage = scratch.open();
        let setup = storage.begin().unwrap();
        storage.create_table(setup, T).unwrap();
        for i in 0..40i64 {
            storage.insert(setup, T, int_row(i)).unwrap();
        }
        storage.commit(setup).unwrap();
    }

    // Simulate the write that was in progress when the power went out:
    // a record header promising more bytes than the file actually holds.
    let journal = scratch.path().join(ferrite_storage::JOURNAL_FILE);
    let mut file = OpenOptions::new().append(true).open(&journal).unwrap();
    file.write_all(&4096u32.to_le_bytes()).unwrap();
    file.write_all(&0xdead_beefu32.to_le_bytes()).unwrap();
    file.write_all(&[0x5a; 500]).unwrap();
    drop(file);

    let storage = scratch.open();
    let txn = storage.begin().unwrap();
    assert_eq!(values(&storage, txn), (0..40).collect::<Vec<i64>>());
    storage.commit(txn).unwrap();
}

#[test]
fn a_corrupt_page_is_reported_rather_than_served() {
    let scratch = Scratch::new("crash_corrupt");
    let row = {
        let storage = scratch.open();
        let setup = storage.begin().unwrap();
        storage.create_table(setup, T).unwrap();
        let row = storage.insert(setup, T, int_row(1234)).unwrap();
        storage.commit(setup).unwrap();
        storage.checkpoint().unwrap();
        row
    };

    // Flip bits inside the last page of the data file, which holds table
    // data rather than metadata, so the failure surfaces on read.
    let data = scratch.path().join(ferrite_storage::DATA_FILE);
    let mut bytes = std::fs::read(&data).unwrap();
    let page_size = ferrite_storage::PAGE_SIZE;
    let last = bytes.len() - page_size;
    for byte in bytes[last + 64..last + 96].iter_mut() {
        *byte ^= 0xff;
    }
    std::fs::write(&data, &bytes).unwrap();

    let outcome = std::panic::catch_unwind(|| {
        let storage = scratch.try_open()?;
        let txn = storage.begin()?;
        storage.get(txn, T, row)?;
        Ok::<(), FerriteError>(())
    });

    match outcome {
        Ok(Ok(())) => panic!("a corrupted page was served as if it were intact"),
        Ok(Err(FerriteError::Storage(message))) => {
            assert!(
                message.contains("checksum") || message.contains("corrupt"),
                "expected a checksum complaint, got: {message}"
            );
        }
        Ok(Err(other)) => panic!("expected a storage error, got {other}"),
        Err(_) => panic!("corruption should be an error, not a panic"),
    }
}

proptest::proptest! {
    #![proptest_config(proptest::test_runner::Config {
        cases: 48,
        ..proptest::test_runner::Config::default()
    })]

    /// Crash at arbitrary points in an arbitrary workload, with a cache far
    /// too small to hold the database, and the reopened engine must show
    /// exactly the rows that committed — no more, no less, and no error.
    #[test]
    fn crashing_anywhere_leaves_exactly_the_committed_rows(
        rounds in proptest::collection::vec((1usize..60, proptest::bool::ANY), 1..12)
    ) {
        let scratch = Scratch::new("crash_prop");
        let open = || {
            ferrite_storage::FerriteStorage::open_with(
                scratch.path(),
                ferrite_storage::StorageConfig { cache_pages: 8, fsync: false, ..Default::default() },
            )
            .expect("open storage")
        };

        {
            let storage = open();
            let setup = storage.begin().unwrap();
            storage.create_table(setup, T).unwrap();
            storage.commit(setup).unwrap();
        }

        let mut expected: Vec<i64> = Vec::new();
        for (round, (batch, keep)) in rounds.iter().enumerate() {
            let storage = open();
            let txn = storage.begin().unwrap();
            let mut written = Vec::new();
            for i in 0..*batch {
                let value = (round * 1000 + i) as i64;
                storage.insert(txn, T, int_row(value)).unwrap();
                written.push(value);
            }
            if *keep {
                storage.commit(txn).unwrap();
                expected.extend(written);
            }
            // Whether or not the transaction committed, the engine now
            // disappears without any chance to tidy up.
            std::mem::drop(storage);

            let storage = open();
            let check = storage.begin().unwrap();
            let mut want = expected.clone();
            want.sort_unstable();
            proptest::prop_assert_eq!(values(&storage, check), want, "after round {}", round);
            storage.commit(check).unwrap();
        }
    }
}

/// Second half of `killed_process_leaves_a_consistent_database`: this runs
/// in a child process that the parent kills outright. Ignored by default so
/// a plain `cargo test` does not spawn it directly.
#[test]
#[ignore = "spawned as a child process by killed_process_leaves_a_consistent_database"]
fn crash_child() {
    let dir = PathBuf::from(std::env::var(CHILD_ENV).expect("child needs a target directory"));
    let storage = ferrite_storage::FerriteStorage::open_with(
        &dir,
        ferrite_storage::StorageConfig {
            cache_pages: 32,
            fsync: true,
            ..Default::default()
        },
    )
    .expect("open storage");

    let setup = storage.begin().unwrap();
    storage.create_table(setup, T).unwrap();
    for i in 0..250i64 {
        storage.insert(setup, T, int_row(i)).unwrap();
    }
    storage.commit(setup).unwrap();

    // Deliberately left open, along with plenty of dirty pages.
    let doomed = storage.begin().unwrap();
    for i in 10_000..10_400i64 {
        storage.insert(doomed, T, int_row(i)).unwrap();
    }

    std::fs::write(dir.join(READY_MARKER), b"ready").expect("write marker");
    // Wait to be killed. The parent is watching for the marker.
    loop {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

#[test]
fn killed_process_leaves_a_consistent_database() {
    let dir = std::env::temp_dir().join(format!("ferrite-kill-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let scratch = Scratch::borrowed(dir.clone());

    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "crash_child", "--ignored", "--nocapture"])
        .env(CHILD_ENV, &dir)
        .spawn()
        .expect("spawn crash child");

    let marker = dir.join(READY_MARKER);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while !marker.exists() {
        if let Some(status) = child.try_wait().expect("poll child") {
            let _ = std::fs::remove_dir_all(&dir);
            panic!("crash child exited early with {status}");
        }
        assert!(
            std::time::Instant::now() < deadline,
            "crash child never became ready"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }

    child.kill().expect("kill crash child");
    child.wait().expect("reap crash child");

    let storage = scratch.open();
    let txn = storage.begin().unwrap();
    let seen = values(&storage, txn);
    assert_eq!(
        seen,
        (0..250).collect::<Vec<i64>>(),
        "the committed rows, and only those, should have survived"
    );

    // The recovered database must be fully usable, not merely readable.
    storage.insert(txn, T, int_row(-1)).unwrap();
    storage.commit(txn).unwrap();
    storage.checkpoint().unwrap();

    let txn = storage.begin().unwrap();
    assert_eq!(values(&storage, txn).len(), 251);
    storage.commit(txn).unwrap();
    drop(storage);
    let _ = std::fs::remove_dir_all(&dir);
}
