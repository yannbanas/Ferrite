//! The negative cache that keeps unique-constraint enforcement affordable.
//!
//! Enforcing a unique key without a secondary index means scanning the
//! table on every write, which turns a bulk load into quadratic work. What
//! makes that avoidable is that the *common* answer is "no conflict", and
//! a cheap structure can prove that answer on its own.
//!
//! [`KeyFilters`] holds, per (table, key columns), a set of hashes that is
//! a **superset** of the hashes of every key currently live in that table.
//! A hash that is absent from the set therefore cannot be present in the
//! table, and the write proceeds with no scan at all. A hash that is
//! present may be a stale entry (an aborted insert, a deleted row, a hash
//! collision), so it only means "look properly" — the authoritative answer
//! always comes from the table itself.
//!
//! Correctness rests entirely on the superset invariant, which is why the
//! rules for maintaining it are deliberately blunt:
//!
//! - a checked write adds its key's hash;
//! - a write that did not go through the check invalidates the table's
//!   filters, forcing a rebuild;
//! - a delete needs no maintenance at all, since removing a row can only
//!   leave the set more of a superset than it already was.
//!
//! The set is per process and never persisted: it is rebuilt by one scan
//! the first time a constraint is used after startup.

use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};

use ferrite_common::{TableId, Value};

/// Hashes retained per constraint before the filter gives up and answers
/// "look properly" every time. At eight bytes an entry this caps a
/// saturated filter's contribution at a few megabytes; past it, a table is
/// large enough that the scan it forces is the smaller problem.
pub const DEFAULT_FILTER_CAPACITY: usize = 1 << 20;

pub fn hash_key(key: &[Value]) -> u64 {
    let mut hasher = DefaultHasher::new();
    for value in key {
        std::mem::discriminant(value).hash(&mut hasher);
        match value {
            Value::Null => {}
            Value::Boolean(v) => v.hash(&mut hasher),
            Value::Int4(v) => v.hash(&mut hasher),
            Value::Int8(v) => v.hash(&mut hasher),
            // `to_bits` so that the hash agrees with `PartialEq` on the
            // exact bit pattern, which is what the row comparison uses.
            Value::Float8(v) => v.to_bits().hash(&mut hasher),
            Value::Text(v) | Value::Json(v) => v.hash(&mut hasher),
            Value::Timestamp(v) => v.hash(&mut hasher),
            Value::Uuid(v) => v.hash(&mut hasher),
        }
    }
    hasher.finish()
}

#[derive(Debug, Default)]
struct Filter {
    hashes: HashSet<u64>,
    /// Set once the filter has stopped accepting entries, at which point
    /// it can no longer prove absence and every check must scan.
    saturated: bool,
}

impl Filter {
    fn add(&mut self, hash: u64, capacity: usize) {
        if self.saturated {
            return;
        }
        if self.hashes.len() >= capacity {
            self.hashes = HashSet::new();
            self.saturated = true;
            return;
        }
        self.hashes.insert(hash);
    }

    fn may_contain(&self, hash: u64) -> bool {
        self.saturated || self.hashes.contains(&hash)
    }
}

#[derive(Debug)]
pub struct KeyFilters {
    filters: HashMap<(TableId, Vec<usize>), Filter>,
    capacity: usize,
}

impl KeyFilters {
    pub fn new(capacity: usize) -> Self {
        Self {
            filters: HashMap::new(),
            capacity,
        }
    }

    /// Whether a filter for this constraint has been built. A missing one
    /// has to be built before it can prove anything, since an empty set is
    /// not a superset of a non-empty table.
    pub fn is_built(&self, table: TableId, columns: &[usize]) -> bool {
        self.filters.contains_key(&(table, columns.to_vec()))
    }

    /// Installs a freshly scanned set of hashes.
    pub fn build(&mut self, table: TableId, columns: &[usize], hashes: HashSet<u64>) {
        let saturated = hashes.len() >= self.capacity;
        self.filters.insert(
            (table, columns.to_vec()),
            Filter {
                hashes: if saturated { HashSet::new() } else { hashes },
                saturated,
            },
        );
    }

    /// `false` proves the key is absent from the table; `true` only means
    /// the table has to be consulted.
    pub fn may_contain(&self, table: TableId, columns: &[usize], hash: u64) -> bool {
        match self.filters.get(&(table, columns.to_vec())) {
            Some(filter) => filter.may_contain(hash),
            None => true,
        }
    }

    pub fn note(&mut self, table: TableId, columns: &[usize], hash: u64) {
        let capacity = self.capacity;
        if let Some(filter) = self.filters.get_mut(&(table, columns.to_vec())) {
            filter.add(hash, capacity);
        }
    }

    /// Drops every filter of `table`, which is what any write that skipped
    /// the check has to do: the superset invariant cannot be trusted once
    /// a key has entered the table behind the filter's back.
    pub fn invalidate(&mut self, table: TableId) {
        self.filters.retain(|(id, _), _| *id != table);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unbuilt_filter_proves_nothing() {
        let filters = KeyFilters::new(16);
        assert!(filters.may_contain(1, &[0], hash_key(&[Value::Int8(7)])));
    }

    #[test]
    fn a_built_filter_proves_absence_and_flags_presence() {
        let mut filters = KeyFilters::new(16);
        let present = hash_key(&[Value::Text("a".into())]);
        filters.build(1, &[0], HashSet::from([present]));
        assert!(filters.may_contain(1, &[0], present));
        assert!(!filters.may_contain(1, &[0], hash_key(&[Value::Text("b".into())])));
    }

    #[test]
    fn a_noted_key_is_flagged_afterwards() {
        let mut filters = KeyFilters::new(16);
        filters.build(1, &[0], HashSet::new());
        let key = hash_key(&[Value::Int4(3)]);
        assert!(!filters.may_contain(1, &[0], key));
        filters.note(1, &[0], key);
        assert!(filters.may_contain(1, &[0], key));
    }

    #[test]
    fn invalidation_makes_the_filter_prove_nothing_again() {
        let mut filters = KeyFilters::new(16);
        filters.build(1, &[0], HashSet::new());
        assert!(!filters.may_contain(1, &[0], 42));
        filters.invalidate(1);
        assert!(filters.may_contain(1, &[0], 42));
    }

    #[test]
    fn saturation_gives_up_on_proving_absence() {
        let mut filters = KeyFilters::new(2);
        filters.build(1, &[0], HashSet::new());
        for i in 0..4 {
            filters.note(1, &[0], i);
        }
        assert!(filters.may_contain(1, &[0], 999));
    }

    #[test]
    fn variants_of_the_same_number_hash_apart() {
        assert_ne!(
            hash_key(&[Value::Int4(1)]),
            hash_key(&[Value::Int8(1)]),
            "Value equality is variant-sensitive, so the hash has to be too"
        );
    }
}
