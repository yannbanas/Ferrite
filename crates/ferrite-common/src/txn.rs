pub type TxnId = u64;

/// MVCC visibility snapshot: a row version is visible to this snapshot if
/// its creating transaction committed strictly before `xmin` and is not
/// listed in `active_at_start`, and (for deletes) it was not deleted by a
/// transaction visible under the same rule. Deliberately no
/// transaction-id wraparound handling in v1 — `TxnId` is a `u64`,
/// wraparound is not a near-term concern the way it is for Postgres's
/// 32-bit xid.
///
/// `xmin` here is an **exclusive upper bound** — the `TxnId` that will be
/// handed out *next*, at the moment the snapshot was taken — not the
/// oldest active transaction the way Postgres names its own `xmin`. This
/// is the field that decides visibility on its own, together with
/// `active_at_start`; a reader coming from Postgres experience should not
/// assume the two `xmin`s mean the same thing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub txn_id: TxnId,
    pub xmin: TxnId,
    pub active_at_start: Vec<TxnId>,
}
