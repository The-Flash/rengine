//! # The write-ahead log (WAL): never lose an acknowledged write
//!
//! The memtable lives in RAM. If the process crashes or the machine loses
//! power, RAM is wiped. So before we touch the memtable, we **append** the
//! write to a log file on disk. That's the "write-ahead" part: the log is
//! written *ahead of* the in-memory change.
//!
//! On startup, we read the log from the beginning and replay every record
//! into a fresh memtable, rebuilding exactly the state we had before the
//! crash. Once the memtable is flushed to an SSTable, its data is safe on disk
//! in another form, so we empty (truncate) the log.
//!
//! Appending to the end of a file is a **sequential write**, the cheapest
//! kind of disk write. That's why logs are everywhere in databases.
//!
//! ## `fsync`: making sure it's really on disk
//!
//! When you call `write`, the OS usually just copies the bytes into its own
//! memory (the "page cache") and writes them to the disk later. If the power
//! fails before that, the write is gone. `fsync` (Rust: `File::sync_data`)
//! forces the OS to push the bytes to the physical device and waits until it
//! confirms. It's slow (often milliseconds), which is why `Options` lets you
//! turn it off for benchmarks. Real databases batch many writes into a single
//! fsync ("group commit") to amortize the cost.
//!
//! ## Record format
//!
//! ```text
//! +-----------+-----------+---------------------------------+
//! | crc: u32  | len: u32  | entry (len bytes, see entry.rs) |
//! +-----------+-----------+---------------------------------+
//! ```
//!
//! The CRC covers the entry bytes. During replay, if a record is cut short or
//! its CRC doesn't match, we know the crash happened *while writing that
//! record*. That write was never acknowledged to the user, so it's safe to
//! drop it and everything after it.

use crate::checksum::crc32;
use crate::entry::{Value, decode_entry, encode_entry, read_u32};
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::Path;

/// Size of the record header: crc (4) + len (4).
const RECORD_HEADER_SIZE: usize = 8;

/// One recovered write: a key and what happened to it.
pub type Record = (Vec<u8>, Value);

/// An open write-ahead log file.
pub struct Wal {
    file: File,
    sync_writes: bool,
}

impl Wal {
    /// Opens the log at `path` (creating it if needed) and returns every
    /// valid record found in it, in the order they were written.
    ///
    /// If the end of the file contains a damaged or half-written record, the
    /// file is truncated to remove it. Otherwise new records would be
    /// appended *after* the garbage, and the next replay would stop at the
    /// garbage and never see them.
    pub fn open(path: &Path, sync_writes: bool) -> io::Result<(Wal, Vec<Record>)> {
        // `append(true)` makes every write go to the end of the file.
        let mut file = OpenOptions::new()
            .read(true)
            .append(true)
            .create(true)
            .open(path)?;

        let mut data = Vec::new();
        file.read_to_end(&mut data)?;

        let (records, valid_len) = parse_records(&data);
        if valid_len < data.len() {
            file.set_len(valid_len as u64)?;
            file.sync_all()?;
        }

        Ok((Wal { file, sync_writes }, records))
    }

    /// Appends one write to the log.
    pub fn append(&mut self, key: &[u8], value: &Value) -> io::Result<()> {
        let mut entry = Vec::new();
        encode_entry(&mut entry, key, value);

        let mut record = Vec::with_capacity(RECORD_HEADER_SIZE + entry.len());
        record.extend_from_slice(&crc32(&entry).to_le_bytes());
        record.extend_from_slice(&(entry.len() as u32).to_le_bytes());
        record.extend_from_slice(&entry);

        // One `write_all` call for the whole record keeps it together.
        self.file.write_all(&record)?;
        if self.sync_writes {
            self.file.sync_data()?;
        }
        Ok(())
    }

    /// Empties the log. Called after the memtable has been safely flushed to
    /// an SSTable, because the log's contents are no longer needed.
    pub fn reset(&mut self) -> io::Result<()> {
        self.file.set_len(0)?;
        self.file.sync_all()
    }
}

/// Decodes records from raw log bytes until the data ends or a damaged record
/// is found. Returns the records and how many bytes were valid.
fn parse_records(data: &[u8]) -> (Vec<Record>, usize) {
    let mut records = Vec::new();
    let mut pos = 0;

    while data.len() - pos >= RECORD_HEADER_SIZE {
        // Both reads are in bounds thanks to the loop condition.
        let stored_crc = read_u32(data, pos).unwrap();
        let len = read_u32(data, pos + 4).unwrap() as usize;

        let start = pos + RECORD_HEADER_SIZE;
        let Some(entry) = data.get(start..start + len) else {
            break; // Torn write: the record was cut short.
        };
        if crc32(entry) != stored_crc {
            break; // Damaged record.
        }
        let Ok((key, value, _)) = decode_entry(entry) else {
            break;
        };

        records.push((key, value));
        pos = start + len;
    }

    (records, pos)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::temp_dir;

    #[test]
    fn replays_records_in_order() {
        let dir = temp_dir("wal-replay");
        let path = dir.join("wal.log");
        {
            let (mut wal, records) = Wal::open(&path, false).unwrap();
            assert!(records.is_empty());
            wal.append(b"a", &Value::Put(b"1".to_vec())).unwrap();
            wal.append(b"b", &Value::Tombstone).unwrap();
        }
        let (_, records) = Wal::open(&path, false).unwrap();
        assert_eq!(
            records,
            vec![
                (b"a".to_vec(), Value::Put(b"1".to_vec())),
                (b"b".to_vec(), Value::Tombstone),
            ]
        );
    }

    #[test]
    fn stops_at_a_torn_record_and_trims_it() {
        let dir = temp_dir("wal-torn");
        let path = dir.join("wal.log");
        {
            let (mut wal, _) = Wal::open(&path, false).unwrap();
            wal.append(b"good", &Value::Put(b"1".to_vec())).unwrap();
            wal.append(b"torn", &Value::Put(b"2".to_vec())).unwrap();
        }
        // Simulate a crash mid-write by chopping off the last 3 bytes.
        let len = std::fs::metadata(&path).unwrap().len();
        OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(len - 3)
            .unwrap();

        let (_, records) = Wal::open(&path, false).unwrap();
        assert_eq!(records, vec![(b"good".to_vec(), Value::Put(b"1".to_vec()))]);
        // The garbage tail was removed from the file.
        assert!(std::fs::metadata(&path).unwrap().len() < len - 3);
    }
}
