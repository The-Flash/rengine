//! # The memtable: a sorted buffer in memory
//!
//! Every write lands here first (right after the write-ahead log). The
//! memtable keeps keys **sorted**, which is the whole trick of an LSM-tree:
//!
//! * Writing to memory is fast, no matter what order keys arrive in.
//! * When the memtable gets big enough, we write it out to disk *in sorted
//!   order*, in one long sequential write. That becomes an SSTable.
//!
//! So random writes from users are turned into sequential writes on disk,
//! which disks (especially spinning ones, but SSDs too) are much better at.
//!
//! ## Why a `BTreeMap`?
//!
//! We need a structure that stays sorted as we insert. Rust's standard
//! `BTreeMap` does exactly that: inserts and lookups are `O(log n)` and
//! iterating returns keys in order. Real engines often use a **skip list**
//! instead, because it is easier to make safe for many threads at once.
//! The idea is the same.
//!
//! ## Tracking the size
//!
//! We keep a running estimate of how many bytes the memtable would take on
//! disk. The database flushes when this passes a limit
//! (`Options::memtable_size_limit`).

use crate::entry::{ENTRY_HEADER_SIZE, Value, encoded_len};
use std::collections::BTreeMap;
use std::ops::Bound;

/// An in-memory, sorted map from key to its newest [`Value`].
#[derive(Debug, Default)]
pub struct MemTable {
    map: BTreeMap<Vec<u8>, Value>,
    approx_size: usize,
}

impl MemTable {
    pub fn new() -> Self {
        Self::default()
    }

    /// Inserts or overwrites a key. A delete is just a put of
    /// `Value::Tombstone`.
    pub fn put(&mut self, key: Vec<u8>, value: Value) {
        let new_size = encoded_len(&key, &value);
        let key_len = key.len();
        // `insert` hands back the old value if the key already existed, so we
        // can subtract its size and keep the estimate accurate.
        if let Some(old) = self.map.insert(key, value) {
            self.approx_size -= key_len + old.size() + ENTRY_HEADER_SIZE;
        }
        self.approx_size += new_size;
    }

    /// Looks up a key. Returns:
    /// * `None` if the memtable knows nothing about it (check the disk next),
    /// * `Some(Value::Tombstone)` if it was deleted recently,
    /// * `Some(Value::Put(..))` if it has a value.
    pub fn get(&self, key: &[u8]) -> Option<&Value> {
        self.map.get(key)
    }

    /// Iterates over entries with `start <= key < end`, in sorted order.
    /// `end = None` means "until the last key".
    pub fn range<'a>(
        &'a self,
        start: &'a [u8],
        end: Option<&'a [u8]>,
    ) -> impl Iterator<Item = (&'a Vec<u8>, &'a Value)> {
        let end_bound = match end {
            // BTreeMap panics if end < start, so turn that into an empty range.
            Some(end) if end < start => Bound::Excluded(start),
            Some(end) => Bound::Excluded(end),
            None => Bound::Unbounded,
        };
        self.map
            .range::<[u8], _>((Bound::Included(start), end_bound))
    }

    /// Iterates over every entry in sorted order (used when flushing).
    pub fn iter(&self) -> impl Iterator<Item = (&Vec<u8>, &Value)> {
        self.map.iter()
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Estimated size in bytes once written to disk.
    pub fn approx_size(&self) -> usize {
        self.approx_size
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_keys_sorted() {
        let mut table = MemTable::new();
        for key in ["banana", "apple", "cherry"] {
            table.put(key.as_bytes().to_vec(), Value::Put(b"x".to_vec()));
        }
        let keys: Vec<&[u8]> = table.iter().map(|(k, _)| k.as_slice()).collect();
        assert_eq!(keys, vec![&b"apple"[..], b"banana", b"cherry"]);
    }

    #[test]
    fn overwrite_keeps_size_accurate() {
        let mut table = MemTable::new();
        table.put(b"k".to_vec(), Value::Put(b"short".to_vec()));
        table.put(b"k".to_vec(), Value::Put(b"a much longer value".to_vec()));
        assert_eq!(
            table.approx_size(),
            encoded_len(b"k", &Value::Put(b"a much longer value".to_vec()))
        );
    }

    #[test]
    fn range_is_half_open() {
        let mut table = MemTable::new();
        for key in ["a", "b", "c", "d"] {
            table.put(key.as_bytes().to_vec(), Value::Put(vec![]));
        }
        let keys: Vec<&[u8]> = table
            .range(b"b", Some(b"d"))
            .map(|(k, _)| k.as_slice())
            .collect();
        assert_eq!(keys, vec![&b"b"[..], b"c"]);
        assert_eq!(table.range(b"d", Some(b"a")).count(), 0);
    }
}
