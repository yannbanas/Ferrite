pub type TxnId = u64;

/// MVCC visibility snapshot: a row version is visible to this snapshot if
/// its creating transaction committed before `xmin` and is not itself
/// in-flight, and (for deletes) it was not deleted by a transaction
/// visible under the same rule. Deliberately no transaction-id wraparound
/// handling in v1 — `TxnId` is a `u64`, wraparound is not a near-term
/// concern the way it is for Postgres's 32-bit xid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub txn_id: TxnId,
    pub xmin: TxnId,
    pub active_at_start: Vec<TxnId>,
}
