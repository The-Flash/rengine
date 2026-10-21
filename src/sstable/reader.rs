//! # Reading an SSTable
//!
//! Opening a table reads only the small parts: the footer, the sparse index
//! and the bloom filter. Those stay in memory. Data blocks are read from disk
//! on demand, one block per lookup at most.
//!
//! A point lookup (`get`) goes through three filters, each cheaper than the
//! next step:
//!
//! ```text
//!   bloom filter says "definitely not here"?  -> done, 0 disk reads
//!   key smaller than the first key in file?   -> done, 0 disk reads
//!   otherwise: binary-search index, read ONE block, scan it
//! ```

use super::{BlockHandle, FOOTER_SIZE, MAGIC, SsTableIter, decode_index, read_at, read_block};
use crate::bloom::BloomFilter;
use crate::entry::{Value, corrupt, decode_entry, read_u64};
use crate::stats::Stats;
use std::cmp::Ordering;
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};

/// An open, read-only SSTable.
pub struct SsTable {
    id: u64,
    path: PathBuf,
    file: File,
    file_size: u64,
    /// The sparse index: first key and location of every data block, sorted.
    index: Vec<BlockHandle>,
    bloom: BloomFilter,
    entry_count: u64,
}

impl SsTable {
    /// Opens an SSTable file and loads its index and bloom filter.
    pub fn open(path: &Path, id: u64) -> io::Result<Self> {
        let file = File::open(path)?;
        let file_size = file.metadata()?.len();
        if file_size < FOOTER_SIZE as u64 {
            return Err(corrupt(format!(
                "{} is too small to be an SSTable",
                path.display()
            )));
        }

        // 1. The footer is always the last FOOTER_SIZE bytes.
        let footer = read_at(&file, file_size - FOOTER_SIZE as u64, FOOTER_SIZE)?;
        let index_offset = read_u64(&footer, 0)?;
        let index_len = read_u64(&footer, 8)?;
        let bloom_offset = read_u64(&footer, 16)?;
        let bloom_len = read_u64(&footer, 24)?;
        let entry_count = read_u64(&footer, 32)?;
        if read_u64(&footer, 40)? != MAGIC {
            return Err(corrupt(format!(
                "{} has a bad magic number",
                path.display()
            )));
        }

        // 2. The footer tells us where the index and bloom filter are.
        let index = decode_index(&read_block(&file, index_offset, index_len as u32)?)?;
        let bloom = BloomFilter::from_bytes(&read_block(&file, bloom_offset, bloom_len as u32)?)?;

        Ok(SsTable {
            id,
            path: path.to_path_buf(),
            file,
            file_size,
            index,
            bloom,
            entry_count,
        })
    }

    /// Looks up `key` in this table.
    ///
    /// Returns `Ok(None)` if this table has no entry for the key, or the
    /// entry's value (which may be a tombstone). `stats` records what the
    /// lookup cost, so you can watch the index and bloom filter at work.
    pub fn get(&self, key: &[u8], use_bloom: bool, stats: &mut Stats) -> io::Result<Option<Value>> {
        if use_bloom && !self.bloom.may_contain(key) {
            stats.bloom_skips += 1;
            return Ok(None);
        }

        let found = self.search_blocks(key, stats)?;
        if found.is_none() && use_bloom {
            // The filter said "maybe", but the key wasn't here.
            stats.bloom_false_positives += 1;
        }
        Ok(found)
    }

    /// Finds the one block that could hold `key`, reads it, and scans it.
    fn search_blocks(&self, key: &[u8], stats: &mut Stats) -> io::Result<Option<Value>> {
        let Some(block_index) = self.find_block(key) else {
            return Ok(None);
        };

        let block = self.read_data_block(block_index)?;
        stats.blocks_read += 1;

        // Entries inside a block are sorted, so we can stop as soon as we
        // pass the key. (Real engines also binary-search inside a block using
        // "restart points"; a linear scan of ~4 KB is fine for us.)
        let mut pos = 0;
        while pos < block.len() {
            let (entry_key, value, used) = decode_entry(&block[pos..])?;
            pos += used;
            match entry_key.as_slice().cmp(key) {
                Ordering::Less => continue,
                Ordering::Equal => return Ok(Some(value)),
                Ordering::Greater => break,
            }
        }
        Ok(None)
    }

    /// Binary-searches the sparse index for the block that could contain
    /// `key`: the last block whose first key is `<= key`. Returns `None` if
    /// `key` is smaller than every key in the file.
    pub(crate) fn find_block(&self, key: &[u8]) -> Option<usize> {
        // `partition_point` is a binary search: it returns how many blocks
        // have `first_key <= key`. The block we want is the last of those.
        let count = self
            .index
            .partition_point(|handle| handle.first_key.as_slice() <= key);
        count.checked_sub(1)
    }

    /// Reads and verifies one data block from disk.
    pub(crate) fn read_data_block(&self, block_index: usize) -> io::Result<Vec<u8>> {
        let handle = &self.index[block_index];
        read_block(&self.file, handle.offset, handle.len)
    }

    /// Iterates over every entry in the table, in key order.
    pub fn iter(&self) -> SsTableIter<'_> {
        SsTableIter::new(self, b"")
    }

    /// Iterates over entries with `key >= start`, in key order. Uses the
    /// index to jump straight to the right block.
    pub fn iter_from(&self, start: &[u8]) -> SsTableIter<'_> {
        SsTableIter::new(self, start)
    }

    pub fn id(&self) -> u64 {
        self.id
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn entry_count(&self) -> u64 {
        self.entry_count
    }

    pub fn block_count(&self) -> usize {
        self.index.len()
    }

    pub fn file_size(&self) -> u64 {
        self.file_size
    }

    pub fn bloom_size(&self) -> usize {
        self.bloom.size_in_bytes()
    }

    /// The smallest key in the table, if it has any entries.
    pub fn first_key(&self) -> Option<&[u8]> {
        self.index.first().map(|handle| handle.first_key.as_slice())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sstable::SsTableBuilder;
    use crate::test_util::temp_dir;

    /// Writes keys key000..key999 with small blocks so the table has many.
    fn build_table(path: &Path) -> SsTable {
        let mut builder = SsTableBuilder::create(path, 256, 10).unwrap();
        for i in 0..1000 {
            let key = format!("key{i:03}");
            let value = if i % 10 == 0 {
                Value::Tombstone
            } else {
                Value::Put(format!("value{i}").into_bytes())
            };
            builder.add(key.as_bytes(), &value).unwrap();
        }
        builder.finish().unwrap();
        SsTable::open(path, 1).unwrap()
    }

    #[test]
    fn finds_every_key_with_one_block_read_each() {
        let dir = temp_dir("sst-get");
        let table = build_table(&dir.join("1.sst"));
        assert!(table.block_count() > 10, "test wants many blocks");

        let mut stats = Stats::default();
        for i in 0..1000 {
            let key = format!("key{i:03}");
            let found = table.get(key.as_bytes(), true, &mut stats).unwrap();
            let expected = if i % 10 == 0 {
                Value::Tombstone
            } else {
                Value::Put(format!("value{i}").into_bytes())
            };
            assert_eq!(found, Some(expected));
        }
        assert_eq!(stats.blocks_read, 1000, "exactly one block per lookup");
    }

    #[test]
    fn missing_keys_are_mostly_skipped_by_the_bloom_filter() {
        let dir = temp_dir("sst-missing");
        let table = build_table(&dir.join("1.sst"));
        let mut stats = Stats::default();
        for i in 0..1000 {
            let key = format!("key{i:03}-missing");
            assert_eq!(table.get(key.as_bytes(), true, &mut stats).unwrap(), None);
        }
        assert!(
            stats.bloom_skips > 950,
            "bloom skipped only {}",
            stats.bloom_skips
        );
    }

    #[test]
    fn rejects_out_of_order_keys() {
        let dir = temp_dir("sst-order");
        let mut builder = SsTableBuilder::create(&dir.join("1.sst"), 256, 10).unwrap();
        builder.add(b"b", &Value::Put(vec![])).unwrap();
        assert!(builder.add(b"a", &Value::Put(vec![])).is_err());
    }

    #[test]
    fn detects_corruption() {
        let dir = temp_dir("sst-corrupt");
        let path = dir.join("1.sst");
        build_table(&path);

        // Flip one byte inside the first data block.
        let mut bytes = std::fs::read(&path).unwrap();
        bytes[20] ^= 0xFF;
        std::fs::write(&path, bytes).unwrap();

        let table = SsTable::open(&path, 1).unwrap();
        let mut stats = Stats::default();
        let err = table.get(b"key001", true, &mut stats).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }
}
