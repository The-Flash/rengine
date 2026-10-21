//! # Entries: the unit of data everything else stores
//!
//! An **entry** is one key plus what happened to it: either it was given a
//! value (`Put`) or it was deleted (`Tombstone`).
//!
//! ## Why deletes are stored as data ("tombstones")
//!
//! In an LSM-tree, files on disk are **immutable**: once written they are
//! never edited. So how do we delete a key that already lives in an old file?
//! We write a new entry that says "this key is deleted". That marker is called
//! a **tombstone**. Reads see the newest entry first; if it is a tombstone,
//! the key counts as missing, even though older files still hold a value.
//! Compaction (see `compaction.rs`) later throws away both.
//!
//! ## The byte format
//!
//! The write-ahead log and the SSTable data blocks both store entries in the
//! same simple, length-prefixed format. All integers are little-endian.
//!
//! ```text
//! +----------+-------------+---------------+-----------+-------------+
//! | kind: u8 | key_len:u32 | value_len:u32 | key bytes | value bytes |
//! +----------+-------------+---------------+-----------+-------------+
//!   1 byte      4 bytes        4 bytes       key_len     value_len
//!
//! kind: 0 = Put, 1 = Tombstone (a tombstone has value_len = 0)
//! ```
//!
//! "Length-prefixed" means we write how long something is *before* writing
//! it, so a reader knows exactly how many bytes to take. This is the most
//! common trick in binary file formats.
//!
//! This module also has a few helpers for reading integers out of byte
//! slices, which the other modules share.

use std::io;

/// What is stored for a key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    /// The key holds these bytes.
    Put(Vec<u8>),
    /// The key was deleted. Hides any older value for the same key.
    Tombstone,
}

impl Value {
    /// Converts to what a user expects back from `get`: `Some(bytes)` for a
    /// live value, `None` for a deleted key.
    pub fn into_option(self) -> Option<Vec<u8>> {
        match self {
            Value::Put(bytes) => Some(bytes),
            Value::Tombstone => None,
        }
    }

    /// Number of value bytes (0 for a tombstone).
    pub fn size(&self) -> usize {
        match self {
            Value::Put(bytes) => bytes.len(),
            Value::Tombstone => 0,
        }
    }
}

const KIND_PUT: u8 = 0;
const KIND_TOMBSTONE: u8 = 1;

/// Size of the fixed part of an entry: kind (1) + key_len (4) + value_len (4).
pub const ENTRY_HEADER_SIZE: usize = 1 + 4 + 4;

/// How many bytes `encode_entry` will produce for this key/value.
pub fn encoded_len(key: &[u8], value: &Value) -> usize {
    ENTRY_HEADER_SIZE + key.len() + value.size()
}

/// Appends the encoded form of one entry to `buf`.
pub fn encode_entry(buf: &mut Vec<u8>, key: &[u8], value: &Value) {
    let (kind, value_bytes): (u8, &[u8]) = match value {
        Value::Put(bytes) => (KIND_PUT, bytes),
        Value::Tombstone => (KIND_TOMBSTONE, &[]),
    };
    buf.push(kind);
    buf.extend_from_slice(&(key.len() as u32).to_le_bytes());
    buf.extend_from_slice(&(value_bytes.len() as u32).to_le_bytes());
    buf.extend_from_slice(key);
    buf.extend_from_slice(value_bytes);
}

/// Decodes one entry from the start of `buf`.
///
/// Returns the key, the value, and how many bytes were consumed, so the caller
/// can move forward to the next entry.
pub fn decode_entry(buf: &[u8]) -> io::Result<(Vec<u8>, Value, usize)> {
    let kind = *buf.first().ok_or_else(|| corrupt("entry is empty"))?;
    let key_len = read_u32(buf, 1)? as usize;
    let value_len = read_u32(buf, 5)? as usize;

    let key_start = ENTRY_HEADER_SIZE;
    let value_start = key_start + key_len;
    let end = value_start + value_len;
    if buf.len() < end {
        return Err(corrupt("entry is truncated"));
    }

    let key = buf[key_start..value_start].to_vec();
    let value = match kind {
        KIND_PUT => Value::Put(buf[value_start..end].to_vec()),
        KIND_TOMBSTONE => Value::Tombstone,
        other => return Err(corrupt(format!("unknown entry kind {other}"))),
    };
    Ok((key, value, end))
}

// ---------------------------------------------------------------------------
// Small helpers shared by the other modules
// ---------------------------------------------------------------------------

/// Reads a little-endian `u32` from `buf` at byte offset `pos`.
pub fn read_u32(buf: &[u8], pos: usize) -> io::Result<u32> {
    let bytes = buf
        .get(pos..pos + 4)
        .ok_or_else(|| corrupt("unexpected end of data while reading a u32"))?;
    Ok(u32::from_le_bytes(bytes.try_into().unwrap()))
}

/// Reads a little-endian `u64` from `buf` at byte offset `pos`.
pub fn read_u64(buf: &[u8], pos: usize) -> io::Result<u64> {
    let bytes = buf
        .get(pos..pos + 8)
        .ok_or_else(|| corrupt("unexpected end of data while reading a u64"))?;
    Ok(u64::from_le_bytes(bytes.try_into().unwrap()))
}

/// Builds the error we return whenever bytes on disk don't make sense.
pub fn corrupt(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn put_round_trips() {
        let mut buf = Vec::new();
        encode_entry(&mut buf, b"name", &Value::Put(b"ada".to_vec()));
        assert_eq!(
            buf.len(),
            encoded_len(b"name", &Value::Put(b"ada".to_vec()))
        );

        let (key, value, used) = decode_entry(&buf).unwrap();
        assert_eq!(key, b"name");
        assert_eq!(value, Value::Put(b"ada".to_vec()));
        assert_eq!(used, buf.len());
    }

    #[test]
    fn tombstone_round_trips() {
        let mut buf = Vec::new();
        encode_entry(&mut buf, b"gone", &Value::Tombstone);
        let (key, value, _) = decode_entry(&buf).unwrap();
        assert_eq!(key, b"gone");
        assert_eq!(value, Value::Tombstone);
    }

    #[test]
    fn truncated_entry_is_an_error() {
        let mut buf = Vec::new();
        encode_entry(&mut buf, b"key", &Value::Put(b"value".to_vec()));
        buf.pop();
        assert!(decode_entry(&buf).is_err());
    }
}
