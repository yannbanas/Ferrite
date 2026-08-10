//! Commit-status bitmap — Ferrite's equivalent of Postgres's `pg_xact`.
//!
//! One bit per transaction id: set means committed. Everything else is an
//! abort, including transactions that were still running when the process
//! died, which is what lets recovery skip an undo pass entirely.
//!
//! Segments are ordinary pages listed in the meta page's directory. With
//! 8 KiB pages that is 65 344 transactions per segment and, given the room
//! the meta page has for the directory, roughly 1.3 x 10^8 transactions per
//! database before the directory fills — a hard v1 ceiling that a segment
//! file naming scheme would lift.

use ferrite_common::{FerriteError, TxnId};

use crate::page::{PageKind, HEADER_SIZE, PAGE_SIZE};
use crate::pager::Pager;

pub const TXNS_PER_CLOG_PAGE: usize = (PAGE_SIZE - HEADER_SIZE) * 8;

fn locate(txn: TxnId) -> (usize, usize, u8) {
    let segment = (txn / TXNS_PER_CLOG_PAGE as u64) as usize;
    let bit = (txn % TXNS_PER_CLOG_PAGE as u64) as usize;
    (segment, bit / 8, 1u8 << (bit % 8))
}

/// Whether `txn` reached a commit record. Unknown transactions — beyond
/// the allocated segments — are not committed, which is the safe answer
/// both for ids that were never used and for ids whose segment was lost.
pub fn is_committed(pager: &mut Pager, txn: TxnId) -> Result<bool, FerriteError> {
    let (segment, byte, mask) = locate(txn);
    let Some(&page_id) = pager.meta().clog_pages.get(segment) else {
        return Ok(false);
    };
    pager.with_page(page_id, |p| p.body()[byte] & mask != 0)
}

pub fn mark_committed(pager: &mut Pager, txn: TxnId) -> Result<(), FerriteError> {
    let (segment, byte, mask) = locate(txn);
    while pager.meta().clog_pages.len() <= segment {
        let page_id = pager.alloc_page(PageKind::Clog)?;
        pager.meta_mut().clog_pages.push(page_id);
    }
    let page_id = pager.meta().clog_pages[segment];
    pager.with_page_mut(page_id, |p| p.body_mut()[byte] |= mask)
}
