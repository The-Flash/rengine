//! # Iterating over an SSTable
//!
//! Range scans and compaction need to walk through a table's entries in key
//! order. The iterator loads **one block at a time**: it decodes entries from
//! the current block, and only when that block is used up does it read the
//! next one from disk. Memory use stays at about one block, no matter how big
//! the file is, and if the caller stops early we never read the rest.
//!
//! `iter_from(start)` uses the sparse index to jump directly to the block
//! containing `start`, instead of reading the file from the beginning.

use super::SsTable;
use crate::entry::{Value, decode_entry};
use std::io;

pub struct SsTableIter<'a> {
    table: &'a SsTable,
    /// Index of the next block to load from disk.
    next_block: usize,
    /// The decoded block we're currently reading entries from.
    block: Vec<u8>,
    /// Byte position of the next entry inside `block`.
    pos: usize,
    /// Entries with keys below this are skipped.
    min_key: Vec<u8>,
    /// How many blocks this iterator has read (handy for experiments).
    blocks_read: usize,
}

impl<'a> SsTableIter<'a> {
    pub(crate) fn new(table: &'a SsTable, start: &[u8]) -> Self {
        // Jump to the block that could contain `start`. If `start` is before
        // the first key, begin at block 0.
        let next_block = table.find_block(start).unwrap_or(0);
        SsTableIter {
            table,
            next_block,
            block: Vec::new(),
            pos: 0,
            min_key: start.to_vec(),
            blocks_read: 0,
        }
    }

    /// Number of data blocks read from disk so far.
    pub fn blocks_read(&self) -> usize {
        self.blocks_read
    }

    /// Stops the iterator after an error so it doesn't keep failing.
    fn stop(&mut self) {
        self.block.clear();
        self.pos = 0;
        self.next_block = self.table.block_count();
    }
}

// Implementing the standard `Iterator` trait lets us use `for` loops and
// combinators like `take_while`. Each item is a `Result` because reading from
// disk can fail.
impl Iterator for SsTableIter<'_> {
    type Item = io::Result<(Vec<u8>, Value)>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            // Still have entries in the current block? Return the next one.
            if self.pos < self.block.len() {
                match decode_entry(&self.block[self.pos..]) {
                    Ok((key, value, used)) => {
                        self.pos += used;
                        if key < self.min_key {
                            continue;
                        }
                        return Some(Ok((key, value)));
                    }
                    Err(error) => {
                        self.stop();
                        return Some(Err(error));
                    }
                }
            }

            // Current block used up. Load the next one, or finish.
            if self.next_block >= self.table.block_count() {
                return None;
            }
            match self.table.read_data_block(self.next_block) {
                Ok(block) => {
                    self.block = block;
                    self.pos = 0;
                    self.next_block += 1;
                    self.blocks_read += 1;
                }
                Err(error) => {
                    self.stop();
                    return Some(Err(error));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::entry::Value;
    use crate::sstable::{SsTable, SsTableBuilder};
    use crate::test_util::temp_dir;

    #[test]
    fn iter_from_starts_at_the_right_key_and_skips_earlier_blocks() {
        let dir = temp_dir("sst-iter");
        let path = dir.join("1.sst");
        let mut builder = SsTableBuilder::create(&path, 128, 10).unwrap();
        for i in 0..500 {
            builder
                .add(format!("k{i:03}").as_bytes(), &Value::Put(vec![b'v'; 10]))
                .unwrap();
        }
        builder.finish().unwrap();
        let table = SsTable::open(&path, 1).unwrap();

        let all: Vec<_> = table.iter().map(|r| r.unwrap().0).collect();
        assert_eq!(all.len(), 500);
        assert!(all.windows(2).all(|w| w[0] < w[1]), "keys come out sorted");

        let mut iter = table.iter_from(b"k250");
        let first = iter.next().unwrap().unwrap().0;
        assert_eq!(first, b"k250");
        assert_eq!(iter.blocks_read(), 1, "jumped straight to the right block");
    }
}
