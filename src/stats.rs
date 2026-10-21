//! # Statistics: watching the engine work
//!
//! These counters exist purely for learning. They let you *see* the effect of
//! the sparse index and the bloom filters. Type `stats` in the REPL, or run
//! the benchmark example.

use std::fmt;

#[derive(Debug, Default, Clone)]
pub struct Stats {
    /// Number of `get` calls.
    pub gets: u64,
    /// `get` calls answered by the memtable without touching disk.
    pub memtable_hits: u64,
    /// SSTables consulted by `get` calls (bloom check included).
    pub sstables_probed: u64,
    /// Times a bloom filter said "definitely not here", saving a disk read.
    pub bloom_skips: u64,
    /// Times a bloom filter said "maybe", but the key wasn't there.
    pub bloom_false_positives: u64,
    /// Data blocks read from disk by `get` calls.
    pub blocks_read: u64,
    /// Memtables written to disk as SSTables.
    pub flushes: u64,
    /// Compactions performed.
    pub compactions: u64,
    /// Records replayed from the write-ahead log when the database opened.
    pub wal_records_recovered: u64,
}

impl fmt::Display for Stats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let per_get = |n: u64| {
            if self.gets == 0 {
                0.0
            } else {
                n as f64 / self.gets as f64
            }
        };
        writeln!(f, "gets                   {}", self.gets)?;
        writeln!(f, "  memtable hits        {}", self.memtable_hits)?;
        writeln!(
            f,
            "  sstables probed      {} ({:.2} per get)",
            self.sstables_probed,
            per_get(self.sstables_probed)
        )?;
        writeln!(f, "  bloom filter skips   {}", self.bloom_skips)?;
        writeln!(f, "  bloom false positive {}", self.bloom_false_positives)?;
        writeln!(
            f,
            "  blocks read          {} ({:.2} per get)",
            self.blocks_read,
            per_get(self.blocks_read)
        )?;
        writeln!(f, "flushes                {}", self.flushes)?;
        writeln!(f, "compactions            {}", self.compactions)?;
        write!(f, "wal records recovered  {}", self.wal_records_recovered)
    }
}
