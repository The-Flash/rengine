//! # Compaction: cleaning up the pile of files
//!
//! Every flush adds another SSTable. Left alone, that causes three problems:
//!
//! 1. **Slower reads**: a lookup for a missing key may have to consult every
//!    table (bloom filters help a lot, but not infinitely).
//! 2. **Wasted space**: a key overwritten 100 times has 100 copies on disk.
//! 3. **Deleted data sticks around**: tombstones and the values they hide
//!    both take up space.
//!
//! **Compaction** fixes all three by merging several tables into one new
//! table, using the same k-way merge as range scans (`merge.rs`). Only the
//! newest version of each key is kept. Then the old tables are deleted.
//!
//! ```text
//! before:  table 3: b=7 e=X        after:  table 4: a=1 b=7 d=1 f=9
//!          table 2: b=5 d=1 e=2
//!          table 1: a=1 b=1 f=9
//! ```
//!
//! ## When is it safe to drop a tombstone?
//!
//! A tombstone exists to hide older values in older tables. If we compact
//! **all** tables together, there's nothing older left to hide, so the
//! tombstone can go. (If we only compacted *some* tables, dropping a
//! tombstone could make a deleted value in an older table reappear! Real
//! engines with partial compaction keep tombstones until they reach the
//! bottom level.)
//!
//! ## Compaction strategies in real engines
//!
//! We use the simplest strategy: merge *everything* into one file. That
//! rewrites the whole database each time, so it doesn't scale. Real engines:
//!
//! * **Leveled** (LevelDB, RocksDB default): tables are organized in levels,
//!   each ~10x bigger than the last. Small, frequent merges push data down
//!   one level at a time. Good read performance, more rewriting.
//! * **Size-tiered** (Cassandra default): merge several tables of similar
//!   size into one bigger table. Less rewriting, more tables to check.
//!
//! The trade-off between rewriting data (*write amplification*), checking
//! many files (*read amplification*) and keeping stale copies (*space
//! amplification*) is the central design tension of LSM-trees.

use crate::entry::Value;
use crate::merge::{EntryIter, MergeIterator};
use crate::sstable::{SsTable, SsTableBuilder};
use std::io;
use std::path::Path;

/// Merges `tables` (ordered newest first) into one new SSTable at
/// `output_path`, keeping only the newest live value of each key.
///
/// `tables` must be **all** the tables in the database, because tombstones
/// are dropped (see the module docs). Returns the number of entries written.
pub fn compact_tables(
    tables: &[SsTable],
    output_path: &Path,
    block_size: usize,
    bloom_bits_per_key: usize,
) -> io::Result<u64> {
    let sources: Vec<EntryIter> = tables
        .iter()
        .map(|table| Box::new(table.iter()) as EntryIter)
        .collect();

    let mut builder = SsTableBuilder::create(output_path, block_size, bloom_bits_per_key)?;
    for entry in MergeIterator::new(sources) {
        let (key, value) = entry?;
        if value == Value::Tombstone {
            continue; // Nothing older remains for it to hide.
        }
        builder.add(&key, &value)?;
    }
    builder.finish()
}
