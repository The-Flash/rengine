//! # SSTables: sorted, immutable files with a built-in index
//!
//! **SSTable** stands for *Sorted String Table*. It's a file of key/value
//! entries **sorted by key** that is written once and never modified. When
//! the memtable is full, it becomes a new SSTable.
//!
//! Sorting is what makes searching fast. If you had a file of 10 million
//! unsorted entries, the only way to find a key would be to read the whole
//! file. Because the file is sorted, we can do much better.
//!
//! ## File layout
//!
//! ```text
//! offset 0
//! +--------------------------------------------------+
//! | data block 0   (entries for keys  aaa .. bzz)   |  ~4 KB each,
//! +--------------------------------------------------+  every block ends
//! | data block 1   (entries for keys  caa .. dzz)   |  with a CRC32
//! +--------------------------------------------------+
//! | ...                                              |
//! +--------------------------------------------------+
//! | data block N                                     |
//! +--------------------------------------------------+
//! | index block    first key + location of each      |  the SPARSE INDEX
//! |                data block                        |
//! +--------------------------------------------------+
//! | bloom filter   bit array covering all keys       |
//! +--------------------------------------------------+
//! | footer (48 B)  where the index & bloom filter    |  fixed size, always
//! |                are, entry count, magic number    |  the last 48 bytes
//! +--------------------------------------------------+
//! ```
//!
//! ## Why blocks?
//!
//! Disks don't read single bytes. They read in chunks: 4 KB pages for an SSD
//! or the OS page cache. Reading 10 bytes costs about the same as reading
//! 4096. So we group entries into ~4 KB **blocks** and always read a whole
//! block at a time.
//!
//! ## The sparse index
//!
//! The index block stores just **one key per data block** (its first key),
//! not one per entry. That's why it's called *sparse*. It is small enough to
//! keep in memory: a table with a million 100-byte entries has ~25,000 blocks,
//! so the index holds ~25,000 keys instead of a million.
//!
//! To find key `K`:
//!
//! 1. **Binary search** the in-memory index for the last block whose first key
//!    is `<= K`. That's the only block that could contain `K`.
//! 2. **Read that one block** from disk (one I/O).
//! 3. Scan the ~40 entries inside it.
//!
//! ```text
//! index:  [ "apple" -> block 0 ][ "grape" -> block 1 ][ "melon" -> block 2 ]
//!
//! get("kiwi"):  "grape" <= "kiwi" < "melon"   =>  read block 1 only
//! get("aaa"):   "aaa" < "apple"               =>  not in this file, 0 reads
//! ```
//!
//! ## The footer
//!
//! How does a reader find the index if it's at the end of a variable-size
//! file? The footer has a **fixed size** and is always the last 48 bytes. So
//! the reader seeks to `file_size - 48`, reads the footer, and learns where
//! everything else is. The magic number lets us reject files that aren't
//! SSTables at all.
//!
//! ```text
//! footer: [index_offset u64][index_len u64][bloom_offset u64][bloom_len u64]
//!         [entry_count u64][magic u64 = "RENGINE1"]
//! ```

mod builder;
mod iter;
mod reader;

pub use builder::SsTableBuilder;
pub use iter::SsTableIter;
pub use reader::SsTable;

use crate::checksum::crc32;
use crate::entry::{corrupt, read_u32, read_u64};
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};

/// The bytes `RENGINE1` read as a number. Every SSTable ends with this.
pub const MAGIC: u64 = u64::from_le_bytes(*b"RENGINE1");

/// The footer is six `u64` values.
pub const FOOTER_SIZE: usize = 6 * 8;

/// One entry of the sparse index: where a data block lives and the first key
/// stored in it.
#[derive(Debug, Clone)]
pub struct BlockHandle {
    pub first_key: Vec<u8>,
    /// Byte offset of the block in the file.
    pub offset: u64,
    /// Length of the block in bytes, including its 4-byte CRC.
    pub len: u32,
}

/// Encodes the index as `[count: u32]` followed by, for each block,
/// `[key_len: u32][key][offset: u64][len: u32]`.
fn encode_index(handles: &[BlockHandle]) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.extend_from_slice(&(handles.len() as u32).to_le_bytes());
    for handle in handles {
        buf.extend_from_slice(&(handle.first_key.len() as u32).to_le_bytes());
        buf.extend_from_slice(&handle.first_key);
        buf.extend_from_slice(&handle.offset.to_le_bytes());
        buf.extend_from_slice(&handle.len.to_le_bytes());
    }
    buf
}

fn decode_index(buf: &[u8]) -> io::Result<Vec<BlockHandle>> {
    let count = read_u32(buf, 0)? as usize;
    let mut handles = Vec::with_capacity(count);
    let mut pos = 4;
    for _ in 0..count {
        let key_len = read_u32(buf, pos)? as usize;
        pos += 4;
        let first_key = buf
            .get(pos..pos + key_len)
            .ok_or_else(|| corrupt("index entry key is truncated"))?
            .to_vec();
        pos += key_len;
        let offset = read_u64(buf, pos)?;
        pos += 8;
        let len = read_u32(buf, pos)?;
        pos += 4;
        handles.push(BlockHandle {
            first_key,
            offset,
            len,
        });
    }
    Ok(handles)
}

/// Reads exactly `len` bytes starting at `offset`.
fn read_at(file: &File, offset: u64, len: usize) -> io::Result<Vec<u8>> {
    // `&File` implements `Read` and `Seek`, so we can read through a shared
    // reference. (Each read moves the file's cursor, which is fine because
    // this engine is single-threaded.)
    let mut file = file;
    file.seek(SeekFrom::Start(offset))?;
    let mut buf = vec![0; len];
    file.read_exact(&mut buf)?;
    Ok(buf)
}

/// Reads a block written by the builder (payload + CRC32), verifies the
/// checksum, and returns just the payload.
fn read_block(file: &File, offset: u64, len: u32) -> io::Result<Vec<u8>> {
    let mut buf = read_at(file, offset, len as usize)?;
    if buf.len() < 4 {
        return Err(corrupt("block is too short to hold a checksum"));
    }
    let payload_len = buf.len() - 4;
    let stored_crc = read_u32(&buf, payload_len)?;
    if crc32(&buf[..payload_len]) != stored_crc {
        return Err(corrupt(format!(
            "checksum mismatch in block at offset {offset}"
        )));
    }
    buf.truncate(payload_len);
    Ok(buf)
}
