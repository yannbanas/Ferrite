//! Property tests for the MVCC invariants.
//!
//! `docs/architecture.md` singles these out as the class of bug an
//! example-based test will not find. Two properties are checked:
//!
//! 1. **Model equivalence.** A randomly generated interleaving of
//!    transactions is run against both the engine and a tiny reference
//!    implementation of snapshot isolation. Every read, every scan and
//!    every write outcome — including which writes are rejected with
//!    `SerializationFailure` — must agree. This subsumes "a transaction
//!    never sees a concurrent uncommitted write", because the model has no
//!    way to produce such a value.
//!
//! 2. **No lost updates.** Concurrent read-modify-write transactions on a
//!    single row must end with a counter equal to the number of
//!    transactions that were allowed to commit. A silently dropped write
//!    would show up as a shortfall.

mod support;

use std::collections::HashMap;

use ferrite_common::{FerriteError, RowId, StorageEngine, TableId, TxnId};
use proptest::prelude::*;
use support::{int_row, row_int, Scratch};

const T: TableId = 1;
const MAX_CONCURRENT: usize = 4;

#[derive(Debug, Clone)]
enum Op {
    Begin,
    Insert(usize, i64),
    Update(usize, usize, i64),
    Delete(usize, usize),
    Get(usize, usize),
    Scan(usize),
    Commit(usize),
    Abort(usize),
}

fn any_op() -> impl Strategy<Value = Op> {
    prop_oneof![
        3 => Just(Op::Begin),
        5 => (0..8usize, -1000i64..1000i64).prop_map(|(t, v)| Op::Insert(t, v)),
        6 => (0..8usize, 0..32usize, -1000i64..1000i64)
            .prop_map(|(t, r, v)| Op::Update(t, r, v)),
        3 => (0..8usize, 0..32usize).prop_map(|(t, r)| Op::Delete(t, r)),
        5 => (0..8usize, 0..32usize).prop_map(|(t, r)| Op::Get(t, r)),
        2 => (0..8usize).prop_map(Op::Scan),
        4 => (0..8usize).prop_map(Op::Commit),
        2 => (0..8usize).prop_map(Op::Abort),
    ]
}

/// What a transaction sees for one row: `None` means no version at all,
/// `Some(None)` means the visible version is a tombstone.
type View = Option<Option<i64>>;

#[derive(Default)]
struct RowHistory {
    /// `(commit sequence, value)`, oldest first. `None` is a delete.
    committed: Vec<(u64, Option<i64>)>,
    /// The single in-flight writer, if any.
    pending: Option<TxnId>,
}

struct ModelTxn {
    begin_seq: u64,
    writes: HashMap<RowId, Option<i64>>,
}

/// Reference snapshot-isolation implementation, deliberately written in
/// terms of commit ordering rather than transaction ids so it shares no
/// logic with the engine under test.
#[derive(Default)]
struct Model {
    seq: u64,
    txns: HashMap<TxnId, ModelTxn>,
    rows: HashMap<RowId, RowHistory>,
}

impl Model {
    fn begin(&mut self, txn: TxnId) {
        self.txns.insert(
            txn,
            ModelTxn {
                begin_seq: self.seq,
                writes: HashMap::new(),
            },
        );
    }

    fn view(&self, txn: TxnId, row: RowId) -> View {
        let state = self.txns.get(&txn).expect("transaction is live");
        if let Some(own) = state.writes.get(&row) {
            return Some(*own);
        }
        let history = self.rows.get(&row)?;
        history
            .committed
            .iter()
            .rev()
            .find(|(seq, _)| *seq <= state.begin_seq)
            .map(|(_, value)| *value)
    }

    /// Outcome the engine must produce for a write to `row` by `txn`.
    fn write_outcome(&self, txn: TxnId, row: RowId) -> Result<(), FerriteError> {
        let state = self.txns.get(&txn).expect("transaction is live");
        if let Some(history) = self.rows.get(&row) {
            if history.pending.is_some_and(|w| w != txn) {
                return Err(FerriteError::SerializationFailure);
            }
            if history
                .committed
                .last()
                .is_some_and(|(seq, _)| *seq > state.begin_seq)
            {
                return Err(FerriteError::SerializationFailure);
            }
        }
        match self.view(txn, row) {
            Some(Some(_)) => Ok(()),
            _ => Err(FerriteError::RowNotFound),
        }
    }

    fn record_write(&mut self, txn: TxnId, row: RowId, value: Option<i64>) {
        let history = self.rows.entry(row).or_default();
        history.pending = Some(txn);
        self.txns
            .get_mut(&txn)
            .expect("transaction is live")
            .writes
            .insert(row, value);
    }

    fn commit(&mut self, txn: TxnId) {
        let state = self.txns.remove(&txn).expect("transaction is live");
        self.seq += 1;
        for (row, value) in state.writes {
            let history = self.rows.entry(row).or_default();
            history.committed.push((self.seq, value));
            if history.pending == Some(txn) {
                history.pending = None;
            }
        }
    }

    fn abort(&mut self, txn: TxnId) {
        let state = self.txns.remove(&txn).expect("transaction is live");
        for row in state.writes.keys() {
            if let Some(history) = self.rows.get_mut(row) {
                if history.pending == Some(txn) {
                    history.pending = None;
                }
            }
        }
    }

    fn visible_rows(&self, txn: TxnId) -> Vec<(RowId, i64)> {
        let mut out: Vec<(RowId, i64)> = self
            .rows
            .keys()
            .filter_map(|row| match self.view(txn, *row) {
                Some(Some(value)) => Some((*row, value)),
                _ => None,
            })
            .collect();
        out.sort_unstable();
        out
    }
}

fn same_error(expected: &FerriteError, actual: &FerriteError) -> bool {
    matches!(
        (expected, actual),
        (
            FerriteError::SerializationFailure,
            FerriteError::SerializationFailure
        ) | (FerriteError::RowNotFound, FerriteError::RowNotFound)
    )
}

fn run_program(ops: &[Op]) -> Result<(), TestCaseError> {
    let scratch = Scratch::new("mvcc_model");
    let storage = scratch.open();

    let setup = storage.begin().unwrap();
    storage.create_table(setup, T).unwrap();
    storage.commit(setup).unwrap();

    let mut model = Model::default();
    let mut live: Vec<TxnId> = Vec::new();
    let mut rows: Vec<RowId> = Vec::new();

    for op in ops {
        match op {
            Op::Begin => {
                if live.len() >= MAX_CONCURRENT {
                    continue;
                }
                let txn = storage.begin().unwrap();
                model.begin(txn);
                live.push(txn);
            }
            Op::Insert(t, value) => {
                let Some(&txn) = pick(&live, *t) else {
                    continue;
                };
                let row = storage.insert(txn, T, int_row(*value)).unwrap();
                prop_assert!(!rows.contains(&row), "row id {row} was handed out twice");
                model.record_write(txn, row, Some(*value));
                rows.push(row);
            }
            Op::Update(t, r, value) => {
                let Some(&txn) = pick(&live, *t) else {
                    continue;
                };
                let Some(&row) = pick(&rows, *r) else {
                    continue;
                };
                let expected = model.write_outcome(txn, row);
                let actual = storage.update(txn, T, row, int_row(*value));
                check_write(&expected, &actual, "update", txn, row)?;
                if expected.is_ok() {
                    model.record_write(txn, row, Some(*value));
                }
            }
            Op::Delete(t, r) => {
                let Some(&txn) = pick(&live, *t) else {
                    continue;
                };
                let Some(&row) = pick(&rows, *r) else {
                    continue;
                };
                let expected = model.write_outcome(txn, row);
                let actual = storage.delete(txn, T, row);
                check_write(&expected, &actual, "delete", txn, row)?;
                if expected.is_ok() {
                    model.record_write(txn, row, None);
                }
            }
            Op::Get(t, r) => {
                let Some(&txn) = pick(&live, *t) else {
                    continue;
                };
                let Some(&row) = pick(&rows, *r) else {
                    continue;
                };
                match (model.view(txn, row), storage.get(txn, T, row)) {
                    (Some(Some(expected)), Ok(actual)) => {
                        prop_assert_eq!(
                            row_int(&actual),
                            expected,
                            "txn {} read the wrong version of row {}",
                            txn,
                            row
                        );
                    }
                    (Some(Some(expected)), Err(e)) => prop_assert!(
                        false,
                        "txn {txn} should have read {expected} from row {row}, got {e}"
                    ),
                    (_, Ok(actual)) => prop_assert!(
                        false,
                        "txn {txn} should not see row {row}, got {}",
                        row_int(&actual)
                    ),
                    (_, Err(FerriteError::RowNotFound)) => {}
                    (_, Err(e)) => prop_assert!(false, "unexpected error reading row {row}: {e}"),
                }
            }
            Op::Scan(t) => {
                let Some(&txn) = pick(&live, *t) else {
                    continue;
                };
                let mut actual: Vec<(RowId, i64)> = storage
                    .scan(txn, T)
                    .unwrap()
                    .map(|r| {
                        let (id, row) = r.expect("scan row");
                        (id, row_int(&row))
                    })
                    .collect();
                actual.sort_unstable();
                prop_assert_eq!(
                    actual,
                    model.visible_rows(txn),
                    "scan mismatch for txn {}",
                    txn
                );
            }
            Op::Commit(t) => {
                let Some(&txn) = pick(&live, *t) else {
                    continue;
                };
                storage.commit(txn).unwrap();
                model.commit(txn);
                live.retain(|id| *id != txn);
            }
            Op::Abort(t) => {
                let Some(&txn) = pick(&live, *t) else {
                    continue;
                };
                storage.abort(txn).unwrap();
                model.abort(txn);
                live.retain(|id| *id != txn);
            }
        }
    }

    // Everything still open is rolled back, then a fresh transaction must
    // agree with the model about the committed state of the world.
    for txn in live.drain(..) {
        storage.abort(txn).unwrap();
        model.abort(txn);
    }
    let final_txn = storage.begin().unwrap();
    model.begin(final_txn);
    let mut actual: Vec<(RowId, i64)> = storage
        .scan(final_txn, T)
        .unwrap()
        .map(|r| {
            let (id, row) = r.expect("scan row");
            (id, row_int(&row))
        })
        .collect();
    actual.sort_unstable();
    prop_assert_eq!(
        actual,
        model.visible_rows(final_txn),
        "final state mismatch"
    );
    storage.commit(final_txn).unwrap();
    Ok(())
}

fn pick<Item>(items: &[Item], index: usize) -> Option<&Item> {
    if items.is_empty() {
        None
    } else {
        items.get(index % items.len())
    }
}

fn check_write(
    expected: &Result<(), FerriteError>,
    actual: &Result<(), FerriteError>,
    what: &str,
    txn: TxnId,
    row: RowId,
) -> Result<(), TestCaseError> {
    match (expected, actual) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(e), Err(a)) if same_error(e, a) => Ok(()),
        (Ok(()), Err(a)) => Err(TestCaseError::fail(format!(
            "{what} by txn {txn} on row {row} should have succeeded, failed with {a}"
        ))),
        (Err(e), Ok(())) => Err(TestCaseError::fail(format!(
            "{what} by txn {txn} on row {row} should have failed with {e}, succeeded"
        ))),
        (Err(e), Err(a)) => Err(TestCaseError::fail(format!(
            "{what} by txn {txn} on row {row} should have failed with {e}, failed with {a}"
        ))),
    }
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 256,
        max_shrink_iters: 4096,
        ..ProptestConfig::default()
    })]

    /// Snapshot isolation matches a reference implementation for arbitrary
    /// interleavings, which is what rules out reading a concurrent
    /// transaction's uncommitted writes.
    #[test]
    fn engine_matches_the_snapshot_isolation_model(
        ops in prop::collection::vec(any_op(), 1..80)
    ) {
        run_program(&ops)?;
    }
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 64,
        ..ProptestConfig::default()
    })]

    /// Read-modify-write from several overlapping transactions either
    /// serialises cleanly or reports `SerializationFailure`. It must never
    /// drop an increment on the floor.
    #[test]
    fn concurrent_increments_never_lose_a_write(
        schedule in prop::collection::vec(0..4usize, 4..48)
    ) {
        let scratch = Scratch::new("no_lost_update");
        let storage = scratch.open();

        let setup = storage.begin().unwrap();
        storage.create_table(setup, T).unwrap();
        let row = storage.insert(setup, T, int_row(0)).unwrap();
        storage.commit(setup).unwrap();

        // Four transaction slots, each stepping through read, write and
        // commit as the schedule names it. Overlapping slots are exactly
        // the concurrency that a lost update would need.
        #[derive(Clone, Copy, PartialEq)]
        enum Stage { Idle, Open, Written }
        let mut stage = [Stage::Idle; 4];
        let mut txn = [0u64; 4];
        let mut seen = [0i64; 4];
        let mut committed = 0i64;

        for slot in schedule {
            match stage[slot] {
                Stage::Idle => {
                    txn[slot] = storage.begin().unwrap();
                    seen[slot] = row_int(&storage.get(txn[slot], T, row).unwrap());
                    stage[slot] = Stage::Open;
                }
                Stage::Open => {
                    match storage.update(txn[slot], T, row, int_row(seen[slot] + 1)) {
                        Ok(()) => stage[slot] = Stage::Written,
                        Err(FerriteError::SerializationFailure) => {
                            storage.abort(txn[slot]).unwrap();
                            stage[slot] = Stage::Idle;
                        }
                        Err(e) => prop_assert!(false, "unexpected update error: {}", e),
                    }
                }
                Stage::Written => {
                    storage.commit(txn[slot]).unwrap();
                    committed += 1;
                    stage[slot] = Stage::Idle;
                }
            }
        }
        for slot in 0..4 {
            if stage[slot] != Stage::Idle {
                storage.abort(txn[slot]).unwrap();
            }
        }

        let check = storage.begin().unwrap();
        let final_value = row_int(&storage.get(check, T, row).unwrap());
        storage.commit(check).unwrap();
        prop_assert_eq!(
            final_value,
            committed,
            "counter should equal the number of committed increments"
        );
    }
}
