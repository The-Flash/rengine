//! # Writing an SSTable
//!
//! The builder receives entries **already in sorted order** (from the
//! memtable, or from the merge during compaction) and streams them to disk:
//!
//! 1. Entries are appended to an in-memory "current block".
//! 2. When the block reaches `block_size` bytes, it is written to the file
//!    with a CRC, and its first key + location are remembered for the index.
//! 3. `finish()` writes the last block, then the index, the bloom filter and
//!    the footer, and finally fsyncs the file.
//!
//! The whole file is written front to back in one pass: purely sequential I/O.

use super::{BlockHandle, MAGIC, encode_index};
use crate::bloom::{BloomFilter, hash_key};
use crate::checksum::crc32;
use crate::entry::{Value, encode_entry};
use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::Path;

pub struct SsTableBuilder {
    /// `BufWriter` collects small writes in memory and sends them to the OS
    /// in big chunks, which is much faster than many tiny `write` calls.
    writer: BufWriter<File>,
    block_size: usize,
    bloom_bits_per_key: usize,

    /// Encoded entries of the block currently being filled.
    block: Vec<u8>,
    /// First key of the current block (goes into the index).
    block_first_key: Vec<u8>,
    /// One handle per finished data block: this becomes the index.
    index: Vec<BlockHandle>,
    /// Hash of every key, used to build the bloom filter at the end.
    key_hashes: Vec<u64>,

    /// How many bytes have been written so far = offset of the next block.
    offset: u64,
    last_key: Option<Vec<u8>>,
    entry_count: u64,
}

impl SsTableBuilder {
    /// Creates (or overwrites) the file at `path`.
    pub fn create(path: &Path, block_size: usize, bloom_bits_per_key: usize) -> io::Result<Self> {
        Ok(SsTableBuilder {
            writer: BufWriter::new(File::create(path)?),
            block_size,
            bloom_bits_per_key,
            block: Vec::new(),
            block_first_key: Vec::new(),
            index: Vec::new(),
            key_hashes: Vec::new(),
            offset: 0,
            last_key: None,
            entry_count: 0,
        })
    }

    /// Adds one entry. Keys must arrive in strictly increasing order, since
    /// the whole file format depends on it.
    pub fn add(&mut self, key: &[u8], value: &Value) -> io::Result<()> {
        if let Some(last) = &self.last_key
            && key <= last.as_slice()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "SSTable keys must be added in strictly increasing order",
            ));
        }

        if self.block.is_empty() {
            self.block_first_key = key.to_vec();
        }
        encode_entry(&mut self.block, key, value);
        self.key_hashes.push(hash_key(key));
        self.last_key = Some(key.to_vec());
        self.entry_count += 1;

        if self.block.len() >= self.block_size {
            self.finish_block()?;
        }
        Ok(())
    }

    /// Number of entries added so far.
    pub fn entry_count(&self) -> u64 {
        self.entry_count
    }

    /// Writes the current block to the file and records it in the index.
    fn finish_block(&mut self) -> io::Result<()> {
        if self.block.is_empty() {
            return Ok(());
        }
        // `mem::take` moves the data out and leaves an empty Vec behind,
        // ready for the next block.
        let block = std::mem::take(&mut self.block);
        let (offset, len) = self.write_with_checksum(&block)?;
        self.index.push(BlockHandle {
            first_key: std::mem::take(&mut self.block_first_key),
            offset,
            len,
        });
        Ok(())
    }

    /// Writes `payload` followed by its CRC32. Returns where it was written
    /// and how many bytes it took.
    fn write_with_checksum(&mut self, payload: &[u8]) -> io::Result<(u64, u32)> {
        let offset = self.offset;
        self.writer.write_all(payload)?;
        self.writer.write_all(&crc32(payload).to_le_bytes())?;
        let len = payload.len() + 4;
        self.offset += len as u64;
        Ok((offset, len as u32))
    }

    /// Writes the remaining block, the index, the bloom filter and the footer,
    /// then makes sure everything is durable on disk. Returns the number of
    /// entries in the table.
    pub fn finish(mut self) -> io::Result<u64> {
        self.finish_block()?;

        let index_bytes = encode_index(&self.index);
        let (index_offset, index_len) = self.write_with_checksum(&index_bytes)?;

        let bloom = BloomFilter::build(&self.key_hashes, self.bloom_bits_per_key);
        let (bloom_offset, bloom_len) = self.write_with_checksum(&bloom.to_bytes())?;

        // The footer: six u64 values. See the layout in `sstable/mod.rs`.
        for value in [
            index_offset,
            index_len as u64,
            bloom_offset,
            bloom_len as u64,
            self.entry_count,
            MAGIC,
        ] {
            self.writer.write_all(&value.to_le_bytes())?;
        }

        // Push BufWriter's buffer to the OS, then fsync to reach the disk.
        self.writer.flush()?;
        self.writer.get_ref().sync_all()?;
        Ok(self.entry_count)
    }
}
