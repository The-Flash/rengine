//! # Merging sorted sources: the heart of scans and compaction
//!
//! A key can have versions in several places at once: the memtable and any
//! number of SSTables. Each of those is **sorted**, and we want one combined
//! sorted stream where each key appears **once**, with its **newest** value.
//!
//! This is the classic **k-way merge** (the same idea as the merge step of
//! merge sort, but for `k` inputs instead of 2):
//!
//! 1. Look at the first entry of every source.
//! 2. Output the smallest key. If several sources have that same key, the
//!    newest source wins and the older versions are discarded.
//! 3. Advance the sources we took from, and repeat.
//!
//! ```text
//! memtable (newest):  b=7            e=X(tombstone)
//! table 2:            b=5      d=1   e=2
//! table 1 (oldest):   a=1  b=1             f=9
//!
//! merged:             a=1  b=7   d=1  e=X   f=9
//! ```
//!
//! To find the smallest key among `k` sources quickly, we keep the current
//! head of each source in a **binary heap** (a priority queue). Getting the
//! smallest is `O(log k)` instead of comparing all `k` every time.
//!
//! Note that tombstones are passed through. The caller decides what to do:
//! a scan hides them, compaction may drop them.

use crate::entry::Value;
use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::io;

/// Any sorted source of entries: the memtable, an SSTable, ... `Box<dyn ..>`
/// lets us put different iterator types in one `Vec`.
pub type EntryIter<'a> = Box<dyn Iterator<Item = io::Result<(Vec<u8>, Value)>> + 'a>;

/// The current head entry of one source, waiting in the heap.
struct HeapItem {
    key: Vec<u8>,
    value: Value,
    /// Position of the source in the input list. Lower = newer.
    source: usize,
}

// Rust's `BinaryHeap` always pops the *largest* item. We want the smallest
// key first, and for equal keys the newest source (lowest index) first. So we
// define the ordering "backwards": an item is "greater" when its key is
// smaller.
impl Ord for HeapItem {
    fn cmp(&self, other: &Self) -> Ordering {
        other
            .key
            .cmp(&self.key)
            .then_with(|| other.source.cmp(&self.source))
    }
}

impl PartialOrd for HeapItem {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for HeapItem {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for HeapItem {}

/// Merges sorted sources into one sorted stream, newest version per key.
pub struct MergeIterator<'a> {
    sources: Vec<EntryIter<'a>>,
    heap: BinaryHeap<HeapItem>,
    /// An I/O error from a source, reported on the next call to `next`.
    error: Option<io::Error>,
}

impl<'a> MergeIterator<'a> {
    /// `sources` must be ordered from **newest to oldest**.
    pub fn new(sources: Vec<EntryIter<'a>>) -> Self {
        let mut merge = MergeIterator {
            sources,
            heap: BinaryHeap::new(),
            error: None,
        };
        // Prime the heap with the first entry of every source.
        for source in 0..merge.sources.len() {
            merge.pull_from(source);
        }
        merge
    }

    /// Takes the next entry from `source` and puts it in the heap.
    fn pull_from(&mut self, source: usize) {
        match self.sources[source].next() {
            Some(Ok((key, value))) => self.heap.push(HeapItem { key, value, source }),
            Some(Err(error)) => self.error = Some(error),
            None => {} // This source is exhausted.
        }
    }
}

impl Iterator for MergeIterator<'_> {
    type Item = io::Result<(Vec<u8>, Value)>;

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(error) = self.error.take() {
            return Some(Err(error));
        }

        // The smallest key, from the newest source that has it.
        let winner = self.heap.pop()?;
        self.pull_from(winner.source);

        // Older versions of the same key sit right behind it in the heap.
        // Pop and discard them, refilling from their sources.
        while let Some(next) = self.heap.peek() {
            if next.key != winner.key {
                break;
            }
            let older = self.heap.pop().unwrap();
            self.pull_from(older.source);
        }

        if let Some(error) = self.error.take() {
            return Some(Err(error));
        }
        Some(Ok((winner.key, winner.value)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(entries: &[(&str, Option<&str>)]) -> EntryIter<'static> {
        let owned: Vec<_> = entries
            .iter()
            .map(|(k, v)| {
                let value = match v {
                    Some(v) => Value::Put(v.as_bytes().to_vec()),
                    None => Value::Tombstone,
                };
                Ok((k.as_bytes().to_vec(), value))
            })
            .collect();
        Box::new(owned.into_iter())
    }

    #[test]
    fn newest_version_wins_and_output_is_sorted() {
        let newest = source(&[("b", Some("7")), ("e", None)]);
        let middle = source(&[("b", Some("5")), ("d", Some("1")), ("e", Some("2"))]);
        let oldest = source(&[("a", Some("1")), ("b", Some("1")), ("f", Some("9"))]);

        let merged: Vec<(Vec<u8>, Value)> = MergeIterator::new(vec![newest, middle, oldest])
            .map(Result::unwrap)
            .collect();

        let expected = vec![
            (b"a".to_vec(), Value::Put(b"1".to_vec())),
            (b"b".to_vec(), Value::Put(b"7".to_vec())),
            (b"d".to_vec(), Value::Put(b"1".to_vec())),
            (b"e".to_vec(), Value::Tombstone),
            (b"f".to_vec(), Value::Put(b"9".to_vec())),
        ];
        assert_eq!(merged, expected);
    }
}
