//! B+-tree keyed by `u64`, with arbitrary-length byte payloads.
//!
//! Both the table catalog (`TableId` to table-header page) and every table
//! (`RowId` to MVCC version chain) are the same structure; only the
//! interpretation of the payload differs. It is a B+-tree rather than a
//! plain B-tree: payloads live only in leaves, and leaves are chained left
//! to right so a sequential scan never revisits an internal node.
//!
//! Item layouts:
//!
//! ```text
//! leaf, inline    [key u64][0u8][payload ...]
//! leaf, overflow  [key u64][1u8][total_len u32][first_page u32]
//! internal        [key u64][child u32]
//! ```
//!
//! An internal node's `extra` header field is its leftmost child, covering
//! keys below the first separator; slot `i` covers keys `>=` its own key.
//! A leaf's `extra` is the next leaf to the right.
//!
//! Known v1 limitation: deletion removes the entry but never merges or
//! rebalances nodes. Underfull leaves stay in the tree and are reclaimed
//! only by [`destroy`]. For an MVCC engine this matters less than it
//! sounds, because a row deletion is a version-chain update, not a key
//! removal — keys disappear only when pruning retires a whole chain.

use ferrite_common::FerriteError;

use crate::page::{Page, PageId, PageKind, NO_PAGE, PAGE_SIZE};
use crate::pager::Pager;

const KEY_LEN: usize = 8;
const FLAG_INLINE: u8 = 0;
const FLAG_OVERFLOW: u8 = 1;
const LEAF_HEADER: usize = KEY_LEN + 1;
const OVERFLOW_DESCRIPTOR: usize = LEAF_HEADER + 8;
const INTERNAL_ITEM: usize = KEY_LEN + 4;

/// Payloads above this go to overflow pages. Chosen so a leaf always holds
/// at least three inline entries, which keeps splits able to make progress.
const MAX_INLINE: usize = 2000;

const OVERFLOW_CHUNK: usize = PAGE_SIZE - crate::page::HEADER_SIZE - 4;

fn corrupt(msg: &str) -> FerriteError {
    FerriteError::Storage(format!("btree: {msg}"))
}

/// Reads a fixed-width little-endian field out of an item, as if the item
/// were zero-padded to the end of the field.
///
/// Both fields below are read by offset from a slot whose length comes off
/// the disk. `Page::validate_layout` proves the slot lies inside its page;
/// it cannot prove the slot is long enough to hold what a B-tree node puts
/// there, so a corrupt file can still present a two-byte "key". Reading it
/// total rather than panicking is safe because nothing indexes memory with
/// the result: a wrong key makes `lookup` miss and `payload_of` reject the
/// item, and a wrong child id sends `descend` at a page whose kind it
/// checks. Every one of those is an error, and none of them is a panic.
fn le_field<const N: usize>(item: &[u8], at: usize) -> [u8; N] {
    let mut buf = [0u8; N];
    let available = item.len().saturating_sub(at).min(N);
    buf[..available].copy_from_slice(&item[at..at + available]);
    buf
}

fn item_key(item: &[u8]) -> u64 {
    u64::from_le_bytes(le_field(item, 0))
}

fn internal_child(item: &[u8]) -> PageId {
    u32::from_le_bytes(le_field(item, KEY_LEN))
}

fn make_internal_item(key: u64, child: PageId) -> Vec<u8> {
    let mut v = Vec::with_capacity(INTERNAL_ITEM);
    v.extend_from_slice(&key.to_le_bytes());
    v.extend_from_slice(&child.to_le_bytes());
    v
}

/// Creates an empty tree and returns its root page id.
pub fn create(pager: &mut Pager) -> Result<PageId, FerriteError> {
    let id = pager.alloc_page(PageKind::Leaf)?;
    pager.with_page_mut(id, |p| p.set_extra(NO_PAGE))?;
    Ok(id)
}

fn slot_search(page: &Page, key: u64) -> Result<usize, usize> {
    let count = page.slot_count();
    let mut lo = 0usize;
    let mut hi = count;
    while lo < hi {
        let mid = (lo + hi) / 2;
        match item_key(page.item(mid)).cmp(&key) {
            std::cmp::Ordering::Less => lo = mid + 1,
            std::cmp::Ordering::Greater => hi = mid,
            std::cmp::Ordering::Equal => return Ok(mid),
        }
    }
    Err(lo)
}

fn child_for(page: &Page, key: u64) -> PageId {
    match slot_search(page, key) {
        Ok(i) => internal_child(page.item(i)),
        Err(0) => page.extra(),
        Err(i) => internal_child(page.item(i - 1)),
    }
}

/// Descends to the leaf that would contain `key`, recording the internal
/// nodes traversed so a split can propagate back up without re-walking.
fn descend(
    pager: &mut Pager,
    root: PageId,
    key: u64,
) -> Result<(PageId, Vec<PageId>), FerriteError> {
    let mut path = Vec::new();
    let mut id = root;
    for _ in 0..64 {
        let step = pager.with_page(id, |p| match p.kind() {
            PageKind::Leaf => Ok(None),
            PageKind::Internal => Ok(Some(child_for(p, key))),
            other => Err(corrupt(&format!("expected a node page, found {other:?}"))),
        })??;
        match step {
            None => return Ok((id, path)),
            Some(child) => {
                path.push(id);
                id = child;
            }
        }
    }
    Err(corrupt("tree depth exceeded 64 levels"))
}

fn write_overflow(pager: &mut Pager, data: &[u8]) -> Result<PageId, FerriteError> {
    let mut chunks: Vec<&[u8]> = data.chunks(OVERFLOW_CHUNK).collect();
    if chunks.is_empty() {
        chunks.push(&[]);
    }
    let mut next = NO_PAGE;
    // Built back to front so each page already knows its successor.
    for chunk in chunks.iter().rev() {
        let id = pager.alloc_page(PageKind::Overflow)?;
        pager.with_page_mut(id, |p| {
            p.set_extra(next);
            let body = p.body_mut();
            body[..4].copy_from_slice(&(chunk.len() as u32).to_le_bytes());
            body[4..4 + chunk.len()].copy_from_slice(chunk);
        })?;
        next = id;
    }
    Ok(next)
}

fn read_overflow(pager: &mut Pager, first: PageId, total: usize) -> Result<Vec<u8>, FerriteError> {
    let mut out = Vec::with_capacity(total);
    let mut id = first;
    while id != NO_PAGE {
        let (chunk, next) = pager.with_page(id, |p| {
            if p.kind() != PageKind::Overflow {
                return Err(corrupt("overflow chain points at a non-overflow page"));
            }
            let body = p.body();
            let len = u32::from_le_bytes(le_field(body, 0)) as usize;
            if len > OVERFLOW_CHUNK {
                return Err(corrupt("overflow chunk length out of range"));
            }
            Ok((body[4..4 + len].to_vec(), p.extra()))
        })??;
        out.extend_from_slice(&chunk);
        id = next;
    }
    if out.len() != total {
        return Err(corrupt(
            "overflow chain length does not match its descriptor",
        ));
    }
    Ok(out)
}

fn free_overflow(pager: &mut Pager, first: PageId) -> Result<(), FerriteError> {
    let mut id = first;
    while id != NO_PAGE {
        let next = pager.with_page(id, |p| p.extra())?;
        pager.free_page(id)?;
        id = next;
    }
    Ok(())
}

/// Frees the overflow chain an existing leaf item points at, if any.
fn release_item(pager: &mut Pager, item: &[u8]) -> Result<(), FerriteError> {
    if item.len() >= OVERFLOW_DESCRIPTOR && item[KEY_LEN] == FLAG_OVERFLOW {
        let first = u32::from_le_bytes(le_field(item, KEY_LEN + 5));
        free_overflow(pager, first)?;
    }
    Ok(())
}

fn build_item(pager: &mut Pager, key: u64, payload: &[u8]) -> Result<Vec<u8>, FerriteError> {
    let mut item = Vec::with_capacity(LEAF_HEADER + payload.len().min(MAX_INLINE));
    item.extend_from_slice(&key.to_le_bytes());
    if payload.len() <= MAX_INLINE {
        item.push(FLAG_INLINE);
        item.extend_from_slice(payload);
    } else {
        let first = write_overflow(pager, payload)?;
        item.push(FLAG_OVERFLOW);
        item.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        item.extend_from_slice(&first.to_le_bytes());
    }
    Ok(item)
}

fn payload_of(pager: &mut Pager, item: &[u8]) -> Result<Vec<u8>, FerriteError> {
    if item.len() < LEAF_HEADER {
        return Err(corrupt("leaf item shorter than its header"));
    }
    match item[KEY_LEN] {
        FLAG_INLINE => Ok(item[LEAF_HEADER..].to_vec()),
        FLAG_OVERFLOW => {
            if item.len() < OVERFLOW_DESCRIPTOR {
                return Err(corrupt("truncated overflow descriptor"));
            }
            let total = u32::from_le_bytes(le_field(item, LEAF_HEADER));
            let first = u32::from_le_bytes(le_field(item, LEAF_HEADER + 4));
            read_overflow(pager, first, total as usize)
        }
        other => Err(corrupt(&format!("unknown leaf item flag {other}"))),
    }
}

/// Payload stored under `key`, or `None` if the key is absent.
pub fn lookup(pager: &mut Pager, root: PageId, key: u64) -> Result<Option<Vec<u8>>, FerriteError> {
    let (leaf, _) = descend(pager, root, key)?;
    let item = pager.with_page(leaf, |p| match slot_search(p, key) {
        Ok(i) => Some(p.item(i).to_vec()),
        Err(_) => None,
    })?;
    match item {
        None => Ok(None),
        Some(item) => Ok(Some(payload_of(pager, &item)?)),
    }
}

/// Inserts or replaces `key`, returning the tree's root, which changes
/// only when the old root had to be split.
pub fn upsert(
    pager: &mut Pager,
    root: PageId,
    key: u64,
    payload: &[u8],
) -> Result<PageId, FerriteError> {
    let (leaf, path) = descend(pager, root, key)?;
    let item = build_item(pager, key, payload)?;

    let existing = pager.with_page(leaf, |p| match slot_search(p, key) {
        Ok(i) => Some((i, p.item(i).to_vec())),
        Err(i) => {
            let _ = i;
            None
        }
    })?;

    let index = match existing {
        Some((i, old)) => {
            release_item(pager, &old)?;
            if pager.with_page_mut(leaf, |p| p.replace_item(i, &item))? {
                return Ok(root);
            }
            pager.with_page_mut(leaf, |p| p.remove_item(i))?;
            i
        }
        None => match pager.with_page(leaf, |p| slot_search(p, key))? {
            Ok(_) => unreachable!("absence was just established"),
            Err(i) => {
                if pager.with_page_mut(leaf, |p| p.insert_item(i, &item))? {
                    return Ok(root);
                }
                i
            }
        },
    };

    let (sep, right) = split_leaf(pager, leaf, index, &item)?;
    propagate(pager, root, path, sep, right)
}

/// Splits `leaf` after inserting `item` at slot `index`, returning the
/// separator key and the new right-hand page.
fn split_leaf(
    pager: &mut Pager,
    leaf: PageId,
    index: usize,
    item: &[u8],
) -> Result<(u64, PageId), FerriteError> {
    let (mut items, right_sibling) = pager.with_page(leaf, |p| {
        let items: Vec<Vec<u8>> = (0..p.slot_count()).map(|i| p.item(i).to_vec()).collect();
        (items, p.extra())
    })?;
    items.insert(index, item.to_vec());
    if items.len() < 2 {
        return Err(corrupt("a single entry does not fit in an empty leaf"));
    }

    let total: usize = items.iter().map(|i| Page::item_cost(i.len())).sum();
    let mut split_at = 0usize;
    let mut running = 0usize;
    for (i, entry) in items.iter().enumerate() {
        running += Page::item_cost(entry.len());
        if running * 2 >= total {
            split_at = i.max(1);
            break;
        }
    }
    if split_at == 0 || split_at >= items.len() {
        split_at = items.len() / 2;
    }

    let right_id = pager.alloc_page(PageKind::Leaf)?;
    let right_items = items.split_off(split_at);
    let separator = item_key(&right_items[0]);

    fill_leaf(pager, leaf, &items, right_id)?;
    fill_leaf(pager, right_id, &right_items, right_sibling)?;
    Ok((separator, right_id))
}

fn fill_leaf(
    pager: &mut Pager,
    id: PageId,
    items: &[Vec<u8>],
    sibling: PageId,
) -> Result<(), FerriteError> {
    pager.with_page_mut(id, |p| {
        let mut fresh = Page::new(PageKind::Leaf);
        fresh.set_extra(sibling);
        for (i, item) in items.iter().enumerate() {
            if !fresh.insert_item(i, item) {
                return Err(corrupt("split produced a half that does not fit in a page"));
            }
        }
        *p = fresh;
        Ok(())
    })?
}

/// Pushes a (separator, right child) pair up the recorded path, splitting
/// internal nodes as needed and growing a new root if the split reaches
/// the top.
fn propagate(
    pager: &mut Pager,
    root: PageId,
    mut path: Vec<PageId>,
    mut sep: u64,
    mut right: PageId,
) -> Result<PageId, FerriteError> {
    while let Some(parent) = path.pop() {
        let item = make_internal_item(sep, right);
        let index = match pager.with_page(parent, |p| slot_search(p, sep))? {
            Ok(_) => return Err(corrupt("duplicate separator key in an internal node")),
            Err(i) => i,
        };
        if pager.with_page_mut(parent, |p| p.insert_item(index, &item))? {
            return Ok(root);
        }
        let (up_sep, up_right) = split_internal(pager, parent, index, &item)?;
        sep = up_sep;
        right = up_right;
    }
    let new_root = pager.alloc_page(PageKind::Internal)?;
    pager.with_page_mut(new_root, |p| {
        p.set_extra(root);
        let inserted = p.insert_item(0, &make_internal_item(sep, right));
        debug_assert!(inserted, "a fresh internal node always has room");
    })?;
    Ok(new_root)
}

fn split_internal(
    pager: &mut Pager,
    node: PageId,
    index: usize,
    item: &[u8],
) -> Result<(u64, PageId), FerriteError> {
    let (mut items, leftmost) = pager.with_page(node, |p| {
        let items: Vec<Vec<u8>> = (0..p.slot_count()).map(|i| p.item(i).to_vec()).collect();
        (items, p.extra())
    })?;
    items.insert(index, item.to_vec());
    if items.len() < 3 {
        return Err(corrupt("internal node too small to split"));
    }

    let middle = items.len() / 2;
    let right_items = items.split_off(middle + 1);
    let promoted = items.pop().expect("middle entry exists");
    let sep = item_key(&promoted);
    let right_leftmost = internal_child(&promoted);

    let right_id = pager.alloc_page(PageKind::Internal)?;
    fill_internal(pager, node, &items, leftmost)?;
    fill_internal(pager, right_id, &right_items, right_leftmost)?;
    Ok((sep, right_id))
}

fn fill_internal(
    pager: &mut Pager,
    id: PageId,
    items: &[Vec<u8>],
    leftmost: PageId,
) -> Result<(), FerriteError> {
    pager.with_page_mut(id, |p| {
        let mut fresh = Page::new(PageKind::Internal);
        fresh.set_extra(leftmost);
        for (i, item) in items.iter().enumerate() {
            if !fresh.insert_item(i, item) {
                return Err(corrupt("internal split produced an oversized half"));
            }
        }
        *p = fresh;
        Ok(())
    })?
}

/// Removes `key` if present. Nodes are never merged; see the module note.
pub fn remove(pager: &mut Pager, root: PageId, key: u64) -> Result<bool, FerriteError> {
    let (leaf, _) = descend(pager, root, key)?;
    let found = pager.with_page(leaf, |p| match slot_search(p, key) {
        Ok(i) => Some((i, p.item(i).to_vec())),
        Err(_) => None,
    })?;
    let Some((index, item)) = found else {
        return Ok(false);
    };
    release_item(pager, &item)?;
    pager.with_page_mut(leaf, |p| p.remove_item(index))?;
    Ok(true)
}

/// Smallest key `>= from` together with its payload, walking the leaf
/// chain. This is the primitive every sequential scan is built on.
pub fn seek(
    pager: &mut Pager,
    root: PageId,
    from: u64,
) -> Result<Option<(u64, Vec<u8>)>, FerriteError> {
    let (mut leaf, _) = descend(pager, root, from)?;
    loop {
        let hit = pager.with_page(leaf, |p| {
            let start = match slot_search(p, from) {
                Ok(i) => i,
                Err(i) => i,
            };
            if start < p.slot_count() {
                let item = p.item(start);
                Some((item_key(item), item.to_vec()))
            } else {
                None
            }
        })?;
        if let Some((key, item)) = hit {
            return Ok(Some((key, payload_of(pager, &item)?)));
        }
        let next = pager.with_page(leaf, |p| p.extra())?;
        if next == NO_PAGE {
            return Ok(None);
        }
        leaf = next;
    }
}

/// Frees every page belonging to the tree, including overflow chains.
pub fn destroy(pager: &mut Pager, root: PageId) -> Result<(), FerriteError> {
    let mut stack = vec![root];
    while let Some(id) = stack.pop() {
        let kind = pager.with_page(id, |p| p.kind())?;
        match kind {
            PageKind::Leaf => {
                let items = pager
                    .with_page(id, |p| {
                        (0..p.slot_count()).map(|i| p.item(i).to_vec()).collect()
                    })
                    .map(|v: Vec<Vec<u8>>| v)?;
                for item in items {
                    release_item(pager, &item)?;
                }
            }
            PageKind::Internal => {
                let children = pager.with_page(id, |p| {
                    let mut c = vec![p.extra()];
                    for i in 0..p.slot_count() {
                        c.push(internal_child(p.item(i)));
                    }
                    c
                })?;
                stack.extend(children);
            }
            other => return Err(corrupt(&format!("unexpected {other:?} page in a tree"))),
        }
        pager.free_page(id)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDb;

    fn collect_all(pager: &mut Pager, root: PageId) -> Vec<(u64, Vec<u8>)> {
        let mut out = Vec::new();
        let mut cursor = 0u64;
        while let Some((key, payload)) = seek(pager, root, cursor).unwrap() {
            out.push((key, payload));
            if key == u64::MAX {
                break;
            }
            cursor = key + 1;
        }
        out
    }

    #[test]
    fn insert_and_lookup_roundtrip() {
        let db = TempDb::new("btree_basic");
        let mut pager = db.pager();
        let mut root = create(&mut pager).unwrap();
        for key in 0..200u64 {
            root = upsert(&mut pager, root, key, format!("value-{key}").as_bytes()).unwrap();
        }
        for key in 0..200u64 {
            assert_eq!(
                lookup(&mut pager, root, key).unwrap().unwrap(),
                format!("value-{key}").into_bytes()
            );
        }
        assert!(lookup(&mut pager, root, 999).unwrap().is_none());
    }

    #[test]
    fn splits_and_keeps_scan_order() {
        let db = TempDb::new("btree_split");
        let mut pager = db.pager();
        let mut root = create(&mut pager).unwrap();
        // Reverse insertion order stresses the left edge of every split.
        for key in (0..3000u64).rev() {
            root = upsert(&mut pager, root, key, &key.to_le_bytes()).unwrap();
        }
        let all = collect_all(&mut pager, root);
        assert_eq!(all.len(), 3000);
        for (i, (key, payload)) in all.iter().enumerate() {
            assert_eq!(*key, i as u64);
            assert_eq!(payload.as_slice(), &(i as u64).to_le_bytes());
        }
    }

    #[test]
    fn overflow_payloads_roundtrip() {
        let db = TempDb::new("btree_overflow");
        let mut pager = db.pager();
        let mut root = create(&mut pager).unwrap();
        let big: Vec<u8> = (0..40_000u32).map(|i| (i % 251) as u8).collect();
        root = upsert(&mut pager, root, 1, &big).unwrap();
        root = upsert(&mut pager, root, 2, b"small").unwrap();
        assert_eq!(lookup(&mut pager, root, 1).unwrap().unwrap(), big);
        assert_eq!(lookup(&mut pager, root, 2).unwrap().unwrap(), b"small");

        // Replacing an overflow entry must release the old chain, which we
        // observe by the free list absorbing the pages.
        root = upsert(&mut pager, root, 1, b"now tiny").unwrap();
        assert_eq!(lookup(&mut pager, root, 1).unwrap().unwrap(), b"now tiny");
        assert_ne!(pager.meta().free_list_head, crate::page::NO_PAGE);
    }

    #[test]
    fn replace_changes_value_not_key_set() {
        let db = TempDb::new("btree_replace");
        let mut pager = db.pager();
        let mut root = create(&mut pager).unwrap();
        for key in 0..500u64 {
            root = upsert(&mut pager, root, key, b"a").unwrap();
        }
        for key in 0..500u64 {
            root = upsert(&mut pager, root, key, &vec![b'b'; 300]).unwrap();
        }
        let all = collect_all(&mut pager, root);
        assert_eq!(all.len(), 500);
        assert!(all.iter().all(|(_, v)| v.len() == 300));
    }

    #[test]
    fn removal_makes_key_absent() {
        let db = TempDb::new("btree_remove");
        let mut pager = db.pager();
        let mut root = create(&mut pager).unwrap();
        for key in 0..1000u64 {
            root = upsert(&mut pager, root, key, &[7u8; 40]).unwrap();
        }
        for key in (0..1000u64).step_by(2) {
            assert!(remove(&mut pager, root, key).unwrap());
        }
        assert!(!remove(&mut pager, root, 0).unwrap());
        let all = collect_all(&mut pager, root);
        assert_eq!(all.len(), 500);
        assert!(all.iter().all(|(k, _)| k % 2 == 1));
    }

    #[test]
    fn destroy_returns_pages_to_the_allocator() {
        let db = TempDb::new("btree_destroy");
        let mut pager = db.pager();
        let mut root = create(&mut pager).unwrap();
        for key in 0..2000u64 {
            root = upsert(&mut pager, root, key, &[3u8; 60]).unwrap();
        }
        let high_water = pager.meta().page_count;
        destroy(&mut pager, root).unwrap();
        assert_eq!(pager.meta().page_count, high_water);

        let mut reused = 0;
        let mut root2 = create(&mut pager).unwrap();
        for key in 0..2000u64 {
            root2 = upsert(&mut pager, root2, key, &[3u8; 60]).unwrap();
        }
        if pager.meta().page_count == high_water {
            reused += 1;
        }
        assert_eq!(reused, 1, "rebuilding the tree should reuse freed pages");
    }
}
