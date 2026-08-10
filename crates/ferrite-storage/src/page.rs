//! Fixed-size page format with mandatory checksums.
//!
//! Every page is 8 KiB — the same size Postgres uses, which keeps the
//! trade-off between per-page overhead and read amplification in familiar
//! territory and makes a future `pg_upgrade`-style import easier to reason
//! about. The size is a compile-time constant rather than a per-database
//! setting: a variable page size buys nothing in v1 and would leak into
//! every offset computation below.
//!
//! Layout:
//!
//! ```text
//! byte  0..4    checksum   CRC-32C of bytes 4..8192
//! byte  4..12   lsn        journal sequence number of the last write
//! byte 12..13   kind       PageKind discriminant
//! byte 13..14   flags      reserved, always 0 in v1
//! byte 14..16   slot_count number of entries in the slot array
//! byte 16..18   free_end   start of the item area, which grows downward
//! byte 18..20   reserved
//! byte 20..24   extra      kind-specific u32 (see PageKind)
//! byte 24..     slot array slot_count entries of (u16 offset, u16 len)
//!      ..free_end          free space
//! free_end..8192           item area
//! ```
//!
//! Pages that hold a single fixed record (meta, table header, clog) ignore
//! the slot array and use [`Page::body`] instead.

use ferrite_common::FerriteError;

use crate::crc::crc32c;

pub const PAGE_SIZE: usize = 8192;
pub const HEADER_SIZE: usize = 24;
const SLOT_SIZE: usize = 4;

pub type PageId = u32;

/// Page 0 always holds the database metadata.
pub const META_PAGE: PageId = 0;
/// Sentinel for "no page", usable because page 0 is always the meta page.
pub const NO_PAGE: PageId = 0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageKind {
    /// Database metadata: magic, format version, allocator state, catalog
    /// tree pointer, clog directory.
    Meta,
    /// B-tree leaf. `extra` is the next leaf to the right (`NO_PAGE` at the
    /// end), which makes an ordered scan a linked-list walk.
    Leaf,
    /// B-tree internal node. `extra` is the leftmost child; each slot is a
    /// (separator key, child) pair covering keys `>=` the separator.
    Internal,
    /// Continuation of a payload too large to sit inside a leaf. `extra` is
    /// the next overflow page.
    Overflow,
    /// Per-table header: B-tree root plus the row-id allocator.
    TableHeader,
    /// Commit-status bitmap segment.
    Clog,
    /// On the allocator free list. `extra` is the next free page.
    Free,
}

impl PageKind {
    fn to_u8(self) -> u8 {
        match self {
            PageKind::Meta => 0,
            PageKind::Leaf => 1,
            PageKind::Internal => 2,
            PageKind::Overflow => 3,
            PageKind::TableHeader => 4,
            PageKind::Clog => 5,
            PageKind::Free => 6,
        }
    }

    fn from_u8(v: u8) -> Result<Self, FerriteError> {
        Ok(match v {
            0 => PageKind::Meta,
            1 => PageKind::Leaf,
            2 => PageKind::Internal,
            3 => PageKind::Overflow,
            4 => PageKind::TableHeader,
            5 => PageKind::Clog,
            6 => PageKind::Free,
            other => {
                return Err(FerriteError::Storage(format!("unknown page kind {other}")));
            }
        })
    }
}

/// One 8 KiB page held in memory. Cloning a page copies the whole buffer;
/// that is deliberate, since journal records need an owned snapshot of the
/// bytes at a specific point in time.
#[derive(Clone)]
pub struct Page {
    bytes: Box<[u8; PAGE_SIZE]>,
}

impl Page {
    pub fn new(kind: PageKind) -> Self {
        let mut page = Page {
            bytes: Box::new([0u8; PAGE_SIZE]),
        };
        page.set_kind(kind);
        page.set_free_end(PAGE_SIZE as u16);
        page
    }

    /// Wraps raw bytes read from disk, verifying the checksum. Checksums
    /// are not optional: a page that fails verification is reported rather
    /// than silently trusted.
    pub fn from_bytes(bytes: [u8; PAGE_SIZE], page_id: PageId) -> Result<Self, FerriteError> {
        let stored = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        let actual = crc32c(&bytes[4..]);
        if stored != actual {
            // Logged here, not only returned: a mismatch means the bytes on
            // disk changed under the database — failing storage, a torn
            // write, something else editing the file. An operator has to
            // learn about it even when the statement that hit it was retried
            // and the error never reached a person.
            ferrite_metrics::metrics().checksum_failures_total.inc();
            tracing::error!(
                page = page_id,
                stored = format!("{stored:#010x}"),
                computed = format!("{actual:#010x}"),
                "page checksum mismatch: the data file is corrupt"
            );
            return Err(FerriteError::Storage(format!(
                "page {page_id} checksum mismatch: stored {stored:#010x}, computed {actual:#010x}"
            )));
        }
        let page = Page {
            bytes: Box::new(bytes),
        };
        PageKind::from_u8(page.bytes[12])?;
        page.validate_layout(page_id)?;
        Ok(page)
    }

    /// Checks the slot directory against the page it describes.
    ///
    /// A checksum proves the bytes are the bytes that were written; it
    /// proves nothing about whether they describe a coherent page. The
    /// slot count, the free-space boundary and every slot's extent are all
    /// read from disk, and every one of them indexes into the page buffer
    /// somewhere downstream. Validating them once here is the difference
    /// between a corrupt file being reported and a corrupt file panicking
    /// the thread that read it.
    fn validate_layout(&self, page_id: PageId) -> Result<(), FerriteError> {
        let bad = |what: &str| {
            Err(FerriteError::Storage(format!(
                "page {page_id}: corrupt layout, {what}"
            )))
        };
        let count = self.slot_count();
        let slot_end = HEADER_SIZE + count * SLOT_SIZE;
        let free_end = self.free_end();
        if slot_end > PAGE_SIZE {
            return bad("slot array runs past the end of the page");
        }
        if free_end > PAGE_SIZE || free_end < slot_end {
            return bad("free-space boundary outside the item area");
        }
        for index in 0..count {
            let (offset, len) = self.slot_at(index);
            if offset < free_end || offset.saturating_add(len) > PAGE_SIZE {
                return bad("an item lies outside the item area");
            }
        }
        Ok(())
    }

    /// Serialises the page, recomputing the checksum over the current
    /// contents. Every path that reaches the disk or the journal goes
    /// through here.
    pub fn to_bytes(&self) -> [u8; PAGE_SIZE] {
        let mut out = *self.bytes;
        let checksum = crc32c(&out[4..]);
        out[..4].copy_from_slice(&checksum.to_le_bytes());
        out
    }

    /// Every page reaching this either came through
    /// [`Page::from_bytes`], which rejects an unknown kind, or through
    /// [`Page::new`], which writes one — so the byte is always a kind.
    pub fn kind(&self) -> PageKind {
        PageKind::from_u8(self.bytes[12]).expect("kind validated on load")
    }

    pub fn set_kind(&mut self, kind: PageKind) {
        self.bytes[12] = kind.to_u8();
    }

    pub fn lsn(&self) -> u64 {
        let mut buf = [0u8; 8];
        buf.copy_from_slice(&self.bytes[4..12]);
        u64::from_le_bytes(buf)
    }

    pub fn set_lsn(&mut self, lsn: u64) {
        self.bytes[4..12].copy_from_slice(&lsn.to_le_bytes());
    }

    pub fn extra(&self) -> u32 {
        let mut buf = [0u8; 4];
        buf.copy_from_slice(&self.bytes[20..24]);
        u32::from_le_bytes(buf)
    }

    pub fn set_extra(&mut self, v: u32) {
        self.bytes[20..24].copy_from_slice(&v.to_le_bytes());
    }

    pub fn slot_count(&self) -> usize {
        u16::from_le_bytes([self.bytes[14], self.bytes[15]]) as usize
    }

    fn set_slot_count(&mut self, v: usize) {
        self.bytes[14..16].copy_from_slice(&(v as u16).to_le_bytes());
    }

    fn free_end(&self) -> usize {
        u16::from_le_bytes([self.bytes[16], self.bytes[17]]) as usize
    }

    fn set_free_end(&mut self, v: u16) {
        self.bytes[16..18].copy_from_slice(&v.to_le_bytes());
    }

    /// Bytes after the header, for pages that hold one fixed record.
    pub fn body(&self) -> &[u8] {
        &self.bytes[HEADER_SIZE..]
    }

    pub fn body_mut(&mut self) -> &mut [u8] {
        &mut self.bytes[HEADER_SIZE..]
    }

    fn slot_at(&self, index: usize) -> (usize, usize) {
        let base = HEADER_SIZE + index * SLOT_SIZE;
        let offset = u16::from_le_bytes([self.bytes[base], self.bytes[base + 1]]) as usize;
        let len = u16::from_le_bytes([self.bytes[base + 2], self.bytes[base + 3]]) as usize;
        (offset, len)
    }

    fn write_slot(&mut self, index: usize, offset: usize, len: usize) {
        let base = HEADER_SIZE + index * SLOT_SIZE;
        self.bytes[base..base + 2].copy_from_slice(&(offset as u16).to_le_bytes());
        self.bytes[base + 2..base + 4].copy_from_slice(&(len as u16).to_le_bytes());
    }

    /// The bytes of one item. In range by construction: `index` always
    /// comes from [`Page::slot_count`], and the slot it names was checked
    /// against the page bounds by [`Page::validate_layout`] on the way in
    /// from disk.
    pub fn item(&self, index: usize) -> &[u8] {
        let (offset, len) = self.slot_at(index);
        &self.bytes[offset..offset + len]
    }

    /// Space an additional item of `len` bytes would need, including its
    /// slot. Callers compare this against [`Page::free_space`].
    pub fn item_cost(len: usize) -> usize {
        len + SLOT_SIZE
    }

    pub fn free_space(&self) -> usize {
        let slot_end = HEADER_SIZE + self.slot_count() * SLOT_SIZE;
        self.free_end().saturating_sub(slot_end)
    }

    /// Inserts an item at slot position `index`, shifting later slots
    /// right. Returns `false` if the page is full, leaving it untouched.
    #[must_use]
    pub fn insert_item(&mut self, index: usize, data: &[u8]) -> bool {
        if Self::item_cost(data.len()) > self.free_space() {
            return false;
        }
        let count = self.slot_count();
        debug_assert!(index <= count);
        let offset = self.free_end() - data.len();
        self.bytes[offset..offset + data.len()].copy_from_slice(data);
        self.set_free_end(offset as u16);

        for i in (index..count).rev() {
            let (o, l) = self.slot_at(i);
            self.write_slot(i + 1, o, l);
        }
        self.write_slot(index, offset, data.len());
        self.set_slot_count(count + 1);
        true
    }

    pub fn remove_item(&mut self, index: usize) {
        let count = self.slot_count();
        debug_assert!(index < count);
        for i in index + 1..count {
            let (o, l) = self.slot_at(i);
            self.write_slot(i - 1, o, l);
        }
        self.set_slot_count(count - 1);
        // The item bytes stay behind as a hole until the next compaction;
        // reclaiming them eagerly would mean moving every later item.
    }

    /// Replaces the item at `index`. Falls back to remove + insert, which
    /// may require compaction; returns `false` if the new data cannot fit
    /// even after compaction, leaving the page unchanged.
    #[must_use]
    pub fn replace_item(&mut self, index: usize, data: &[u8]) -> bool {
        let (_, old_len) = self.slot_at(index);
        if data.len() == old_len {
            let (offset, _) = self.slot_at(index);
            self.bytes[offset..offset + data.len()].copy_from_slice(data);
            return true;
        }
        let snapshot = self.clone();
        self.remove_item(index);
        if Self::item_cost(data.len()) > self.free_space() {
            self.compact();
        }
        if self.insert_item(index, data) {
            true
        } else {
            *self = snapshot;
            false
        }
    }

    /// Rewrites the item area with no holes. Slot order and contents are
    /// preserved; only offsets change.
    pub fn compact(&mut self) {
        let count = self.slot_count();
        let items: Vec<Vec<u8>> = (0..count).map(|i| self.item(i).to_vec()).collect();
        self.set_free_end(PAGE_SIZE as u16);
        self.set_slot_count(0);
        for (i, item) in items.iter().enumerate() {
            let inserted = self.insert_item(i, item);
            debug_assert!(inserted, "compaction cannot overflow a page");
        }
    }
}

impl std::fmt::Debug for Page {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Page")
            .field("kind", &self.kind())
            .field("lsn", &self.lsn())
            .field("slots", &self.slot_count())
            .field("free", &self.free_space())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrips_through_bytes() {
        let mut page = Page::new(PageKind::Leaf);
        page.set_lsn(42);
        page.set_extra(7);
        assert!(page.insert_item(0, b"hello"));
        assert!(page.insert_item(1, b"world"));

        let reloaded = Page::from_bytes(page.to_bytes(), 3).expect("checksum ok");
        assert_eq!(reloaded.kind(), PageKind::Leaf);
        assert_eq!(reloaded.lsn(), 42);
        assert_eq!(reloaded.extra(), 7);
        assert_eq!(reloaded.item(0), b"hello");
        assert_eq!(reloaded.item(1), b"world");
    }

    #[test]
    fn rejects_corrupted_page() {
        let before = ferrite_metrics::metrics().checksum_failures_total.get();
        let page = Page::new(PageKind::Leaf);
        let mut bytes = page.to_bytes();
        bytes[100] ^= 0xff;
        let err = Page::from_bytes(bytes, 9).unwrap_err();
        assert!(matches!(err, FerriteError::Storage(_)));
        // Corruption is not only reported to the caller: it is counted, so
        // an operator sees it even when the statement that hit it was
        // retried and the error never reached anyone.
        assert!(ferrite_metrics::metrics().checksum_failures_total.get() > before);
    }

    /// A checksum proves the bytes were not altered on their way to disk.
    /// It proves nothing about whether they describe a coherent page — a
    /// page written by a buggy build, or a file someone edited and
    /// re-signed, passes it. Each of these would have indexed the page
    /// buffer out of bounds somewhere downstream and panicked the reader.
    #[test]
    fn rejects_a_well_checksummed_page_with_an_impossible_layout() {
        let sign = |mut bytes: [u8; PAGE_SIZE]| {
            let checksum = crc32c(&bytes[4..]);
            bytes[..4].copy_from_slice(&checksum.to_le_bytes());
            bytes
        };
        let base = || {
            let mut page = Page::new(PageKind::Leaf);
            assert!(page.insert_item(0, b"hello"));
            page.to_bytes()
        };

        let mut slots_past_the_page = base();
        slots_past_the_page[14..16].copy_from_slice(&u16::MAX.to_le_bytes());
        assert!(Page::from_bytes(sign(slots_past_the_page), 1).is_err());

        let mut free_end_past_the_page = base();
        free_end_past_the_page[16..18].copy_from_slice(&u16::MAX.to_le_bytes());
        assert!(Page::from_bytes(sign(free_end_past_the_page), 2).is_err());

        let mut item_past_the_page = base();
        item_past_the_page[HEADER_SIZE..HEADER_SIZE + 2]
            .copy_from_slice(&(PAGE_SIZE as u16 - 2).to_le_bytes());
        item_past_the_page[HEADER_SIZE + 2..HEADER_SIZE + 4].copy_from_slice(&64u16.to_le_bytes());
        assert!(Page::from_bytes(sign(item_past_the_page), 3).is_err());

        let mut item_inside_the_slot_array = base();
        item_inside_the_slot_array[HEADER_SIZE..HEADER_SIZE + 2]
            .copy_from_slice(&0u16.to_le_bytes());
        assert!(Page::from_bytes(sign(item_inside_the_slot_array), 4).is_err());

        // The untouched page still loads, so the check is not simply
        // refusing everything.
        assert!(Page::from_bytes(base(), 5).is_ok());
    }

    #[test]
    fn insert_keeps_slot_order() {
        let mut page = Page::new(PageKind::Internal);
        assert!(page.insert_item(0, b"c"));
        assert!(page.insert_item(0, b"a"));
        assert!(page.insert_item(1, b"b"));
        assert_eq!(page.slot_count(), 3);
        assert_eq!(page.item(0), b"a");
        assert_eq!(page.item(1), b"b");
        assert_eq!(page.item(2), b"c");
    }

    #[test]
    fn remove_shifts_slots() {
        let mut page = Page::new(PageKind::Leaf);
        for i in 0..5u8 {
            assert!(page.insert_item(i as usize, &[i]));
        }
        page.remove_item(2);
        assert_eq!(page.slot_count(), 4);
        assert_eq!(page.item(0), [0]);
        assert_eq!(page.item(2), [3]);
    }

    #[test]
    fn replace_grows_and_shrinks() {
        let mut page = Page::new(PageKind::Leaf);
        assert!(page.insert_item(0, b"short"));
        assert!(page.insert_item(1, b"tail"));
        assert!(page.replace_item(0, b"a much longer value"));
        assert_eq!(page.item(0), b"a much longer value");
        assert_eq!(page.item(1), b"tail");
        assert!(page.replace_item(0, b"x"));
        assert_eq!(page.item(0), b"x");
        assert_eq!(page.item(1), b"tail");
    }

    #[test]
    fn rejects_item_that_does_not_fit() {
        let mut page = Page::new(PageKind::Leaf);
        let big = vec![0u8; PAGE_SIZE];
        assert!(!page.insert_item(0, &big));
        assert_eq!(page.slot_count(), 0);
    }

    #[test]
    fn compaction_reclaims_holes() {
        let mut page = Page::new(PageKind::Leaf);
        let chunk = vec![7u8; 1000];
        for i in 0..7 {
            assert!(page.insert_item(i, &chunk));
        }
        for _ in 0..6 {
            page.remove_item(0);
        }
        let before = page.free_space();
        page.compact();
        assert!(page.free_space() > before);
        assert_eq!(page.item(0), chunk.as_slice());
    }

    #[test]
    fn failed_replace_leaves_page_intact() {
        let mut page = Page::new(PageKind::Leaf);
        assert!(page.insert_item(0, b"keep"));
        assert!(!page.replace_item(0, &vec![0u8; PAGE_SIZE]));
        assert_eq!(page.slot_count(), 1);
        assert_eq!(page.item(0), b"keep");
    }
}
