//! # Bloom filters: answering "is this key here?" without touching the disk
//!
//! When we look up a key, we may have to check many SSTable files. Most of
//! them won't contain the key, and reading a block from each one just to find
//! out is wasteful. A **bloom filter** is a tiny summary of every key in a
//! file that can answer one question very quickly:
//!
//! * "**Definitely not** in this file" (always correct), or
//! * "**Maybe** in this file" (usually correct, occasionally wrong).
//!
//! When it says "definitely not", we skip the file entirely: zero disk reads.
//!
//! ## How it works
//!
//! A bloom filter is just an array of bits, all starting at 0.
//!
//! * **Insert** a key: run it through `k` different hash functions. Each hash
//!   picks one bit position. Set those `k` bits to 1.
//! * **Query** a key: compute the same `k` positions. If *any* of them is 0,
//!   the key was never inserted (inserting would have set it). If all are 1,
//!   the key was *probably* inserted, but those bits might have been set by
//!   other keys. That case is a **false positive**.
//!
//! ```text
//! insert "cat":  hashes -> 2, 5, 9      bits: 0 0 1 0 0 1 0 0 0 1 0 0
//! insert "dog":  hashes -> 1, 5, 11     bits: 0 1 1 0 0 1 0 0 0 1 0 1
//! query  "cow":  hashes -> 3, 5, 9      bit 3 is 0  => definitely absent
//! query  "owl":  hashes -> 1, 2, 11     all 1      => "maybe" (false positive!)
//! ```
//!
//! There are never **false negatives**: if a key was inserted, the filter
//! always says "maybe". That is what makes it safe to skip files.
//!
//! ## Sizing
//!
//! With `m` bits per key and the best number of hashes `k ≈ m × ln 2`, the
//! false positive rate is about `0.6185^m`. At 10 bits per key (1.25 bytes
//! per key!) that is roughly **1%**. LevelDB and RocksDB use the same default.
//!
//! ## Getting `k` hashes cheaply
//!
//! Computing `k` independent hashes is slow, so we use **double hashing**:
//! compute one 64-bit hash `h`, derive a step `delta` from it, and use
//! `h, h + delta, h + 2·delta, ...` as the positions. Research shows this
//! works as well as truly independent hashes for bloom filters.

use crate::entry::{corrupt, read_u32};
use std::io;

/// A bloom filter: a bit array plus the number of hash functions used.
#[derive(Debug, Clone)]
pub struct BloomFilter {
    bits: Vec<u8>,
    num_hashes: u32,
}

/// Hashes a key to 64 bits.
///
/// We use FNV-1a (a simple, well-known hash) followed by a "finalizer" that
/// scrambles the bits. Keys like `user:0001` and `user:0002` differ in only
/// a few bits; the finalizer spreads that difference across the whole number
/// so that similar keys land on unrelated bit positions.
pub fn hash_key(key: &[u8]) -> u64 {
    // FNV-1a: for each byte, xor it in, then multiply by a large prime.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &byte in key {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    // Finalizer from MurmurHash3 ("fmix64").
    hash ^= hash >> 33;
    hash = hash.wrapping_mul(0xff51_afd7_ed55_8ccd);
    hash ^= hash >> 33;
    hash = hash.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    hash ^= hash >> 33;
    hash
}

impl BloomFilter {
    /// Builds a filter containing every key whose hash is in `key_hashes`.
    ///
    /// The SSTable builder collects hashes while writing (it doesn't know the
    /// final key count in advance) and calls this once at the end.
    pub fn build(key_hashes: &[u64], bits_per_key: usize) -> Self {
        // Never go below 64 bits, so tiny tables still get a usable filter.
        let num_bits = (key_hashes.len() * bits_per_key).max(64);
        let num_bytes = num_bits.div_ceil(8);

        // Optimal number of hash functions is bits_per_key × ln(2).
        let num_hashes = ((bits_per_key as f64) * std::f64::consts::LN_2)
            .round()
            .clamp(1.0, 30.0) as u32;

        let mut filter = BloomFilter {
            bits: vec![0; num_bytes],
            num_hashes,
        };
        for &hash in key_hashes {
            for bit in filter.bit_positions(hash) {
                filter.bits[bit / 8] |= 1 << (bit % 8);
            }
        }
        filter
    }

    /// Returns `false` if the key is definitely absent, `true` if it might be
    /// present.
    pub fn may_contain(&self, key: &[u8]) -> bool {
        self.bit_positions(hash_key(key))
            .all(|bit| self.bits[bit / 8] & (1 << (bit % 8)) != 0)
    }

    /// The `k` bit positions for a hash, using double hashing.
    fn bit_positions(&self, hash: u64) -> impl Iterator<Item = usize> + use<> {
        let num_bits = (self.bits.len() * 8) as u64;
        let delta = hash.rotate_right(17);
        let mut current = hash;
        (0..self.num_hashes).map(move |_| {
            let position = (current % num_bits) as usize;
            current = current.wrapping_add(delta);
            position
        })
    }

    /// Serializes the filter as `[num_hashes: u32][bit array...]`.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(4 + self.bits.len());
        out.extend_from_slice(&self.num_hashes.to_le_bytes());
        out.extend_from_slice(&self.bits);
        out
    }

    /// Reads back a filter written by `to_bytes`.
    pub fn from_bytes(bytes: &[u8]) -> io::Result<Self> {
        let num_hashes = read_u32(bytes, 0)?;
        let bits = bytes[4..].to_vec();
        if bits.is_empty() || num_hashes == 0 {
            return Err(corrupt("bloom filter is empty"));
        }
        Ok(BloomFilter { bits, num_hashes })
    }

    /// Size of the bit array in bytes.
    pub fn size_in_bytes(&self) -> usize {
        self.bits.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filter_for(keys: &[Vec<u8>]) -> BloomFilter {
        let hashes: Vec<u64> = keys.iter().map(|k| hash_key(k)).collect();
        BloomFilter::build(&hashes, 10)
    }

    #[test]
    fn never_has_false_negatives() {
        let keys: Vec<Vec<u8>> = (0..5_000).map(|i| format!("key{i}").into_bytes()).collect();
        let filter = filter_for(&keys);
        for key in &keys {
            assert!(filter.may_contain(key), "false negative for {key:?}");
        }
    }

    #[test]
    fn false_positive_rate_is_about_one_percent() {
        let keys: Vec<Vec<u8>> = (0..10_000)
            .map(|i| format!("key{i}").into_bytes())
            .collect();
        let filter = filter_for(&keys);

        let trials = 10_000;
        let false_positives = (0..trials)
            .filter(|i| filter.may_contain(format!("other{i}").as_bytes()))
            .count();
        let rate = false_positives as f64 / trials as f64;
        assert!(rate < 0.03, "false positive rate too high: {rate}");
    }

    #[test]
    fn survives_serialization() {
        let keys = vec![b"a".to_vec(), b"b".to_vec()];
        let filter = filter_for(&keys);
        let restored = BloomFilter::from_bytes(&filter.to_bytes()).unwrap();
        assert!(restored.may_contain(b"a"));
        assert!(restored.may_contain(b"b"));
    }
}
