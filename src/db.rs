//! # The database: tying all the pieces together
//!
//! This is the public face of the engine. It owns:
//!
//! * the **write-ahead log** (`wal.log`) for durability,
//! * the **memtable** for recent writes,
//! * a list of **SSTables** on disk, newest first,
//! * the **manifest** that records which SSTables are live.
//!
//! ## The write path
//!
//! ```text
//! put(k, v)
//!   1. append to WAL (+ fsync)          <- now the write survives a crash
//!   2. insert into memtable             <- now reads can see it
//!   3. memtable too big?  -> flush():
//!        a. write memtable to a new SSTable (sorted, sequential)
//!        b. add it to the manifest (atomic rename)
//!        c. empty the WAL               <- data is safe in the SSTable now
//!        d. too many SSTables? -> compact()
//! ```
//!
//! ## The read path
//!
//! ```text
//! get(k)
//!   1. memtable          (newest data)
//!   2. SSTable N         (newest file)
//!   3. SSTable N-1
//!   ...                  the FIRST place that knows about k wins.
//!                        If that's a tombstone, k is deleted.
//! ```
//!
//! ## Opening (recovery)
//!
//! ```text
//! open(dir)
//!   1. read MANIFEST -> open the live SSTables
//!   2. delete leftover .sst files not in the manifest (crash debris)
//!   3. replay wal.log into a fresh memtable
//! ```

use crate::compaction::compact_tables;
use crate::entry::Value;
use crate::manifest::{MANIFEST_FILE, Manifest, sync_dir};
use crate::memtable::MemTable;
use crate::merge::{EntryIter, MergeIterator};
use crate::sstable::{SsTable, SsTableBuilder};
use crate::stats::Stats;
use crate::wal::Wal;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

const WAL_FILE: &str = "wal.log";

/// Tuning knobs. The defaults are reasonable; the REPL uses tiny sizes so
/// you can watch flushes and compactions happen.
#[derive(Debug, Clone)]
pub struct Options {
    /// Flush the memtable to an SSTable once it holds about this many bytes.
    pub memtable_size_limit: usize,
    /// Target size of each SSTable data block, in bytes.
    pub block_size: usize,
    /// Bloom filter size. 10 bits per key gives ~1% false positives.
    pub bloom_bits_per_key: usize,
    /// Consult bloom filters during reads? (Turn off to see the difference.)
    pub use_bloom_filter: bool,
    /// fsync the WAL after every write? Safer but much slower.
    pub sync_writes: bool,
    /// Compact automatically when there are this many SSTables. 0 = never.
    pub compaction_trigger: usize,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            memtable_size_limit: 64 * 1024,
            block_size: 4 * 1024,
            bloom_bits_per_key: 10,
            use_bloom_filter: true,
            sync_writes: true,
            compaction_trigger: 8,
        }
    }
}

/// A key-value database stored in a directory.
pub struct Db {
    dir: PathBuf,
    options: Options,
    wal: Wal,
    memtable: MemTable,
    /// Live SSTables, newest first (the order reads check them).
    tables: Vec<SsTable>,
    manifest: Manifest,
    stats: Stats,
}

impl Db {
    /// Opens the database in `dir`, creating it if it doesn't exist, and
    /// recovers any writes that were in the WAL.
    pub fn open(dir: impl AsRef<Path>, options: Options) -> io::Result<Db> {
        let dir = dir.as_ref().to_path_buf();
        fs::create_dir_all(&dir)?;

        // 1. The manifest says which tables are live. Open them.
        let manifest = Manifest::load_or_create(&dir)?;
        let mut tables = Vec::new();
        for &id in manifest.tables() {
            tables.push(SsTable::open(&table_path(&dir, id), id)?);
        }

        // 2. Remove crash debris: tables that never made it into the
        //    manifest, or that compaction replaced but didn't delete.
        remove_orphan_tables(&dir, manifest.tables())?;

        // 3. Replay the WAL to rebuild the memtable.
        let (wal, records) = Wal::open(&dir.join(WAL_FILE), options.sync_writes)?;
        let mut memtable = MemTable::new();
        let mut stats = Stats::default();
        for (key, value) in records {
            memtable.put(key, value);
            stats.wal_records_recovered += 1;
        }

        Ok(Db {
            dir,
            options,
            wal,
            memtable,
            tables,
            manifest,
            stats,
        })
    }

    /// Stores `value` under `key`, replacing any previous value.
    pub fn put(&mut self, key: &[u8], value: &[u8]) -> io::Result<()> {
        self.write(key, Value::Put(value.to_vec()))
    }

    /// Deletes `key` by writing a tombstone.
    pub fn delete(&mut self, key: &[u8]) -> io::Result<()> {
        self.write(key, Value::Tombstone)
    }

    fn write(&mut self, key: &[u8], value: Value) -> io::Result<()> {
        // Our on-disk format stores lengths as u32.
        if key.len() > u32::MAX as usize || value.size() > u32::MAX as usize {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "key or value too large",
            ));
        }

        self.wal.append(key, &value)?; // 1. durable
        self.memtable.put(key.to_vec(), value); // 2. visible

        if self.memtable.approx_size() >= self.options.memtable_size_limit {
            self.flush()?; // 3. spill to disk
        }
        Ok(())
    }

    /// Returns the current value of `key`, or `None` if it doesn't exist.
    ///
    /// This takes `&mut self` only to update the statistics counters.
    pub fn get(&mut self, key: &[u8]) -> io::Result<Option<Vec<u8>>> {
        self.stats.gets += 1;

        // 1. The memtable has the newest data.
        if let Some(value) = self.memtable.get(key) {
            self.stats.memtable_hits += 1;
            return Ok(value.clone().into_option());
        }

        // 2. Then each SSTable, newest first. The first one that knows the
        //    key has its latest version.
        for table in &self.tables {
            self.stats.sstables_probed += 1;
            if let Some(value) = table.get(key, self.options.use_bloom_filter, &mut self.stats)? {
                return Ok(value.into_option());
            }
        }

        Ok(None)
    }

    /// Returns all live key/value pairs with `start <= key < end`, sorted by
    /// key. `end = None` scans to the last key.
    pub fn scan(&self, start: &[u8], end: Option<&[u8]>) -> io::Result<Vec<(Vec<u8>, Vec<u8>)>> {
        // One sorted source per place data can live, newest first.
        let mut sources: Vec<EntryIter> = Vec::new();
        sources.push(Box::new(
            self.memtable
                .range(start, end)
                .map(|(key, value)| Ok((key.clone(), value.clone()))),
        ));
        for table in &self.tables {
            sources.push(Box::new(table.iter_from(start)));
        }

        let mut results = Vec::new();
        for entry in MergeIterator::new(sources) {
            let (key, value) = entry?;
            if let Some(end) = end
                && key.as_slice() >= end
            {
                break; // Sorted output, so nothing after this is in range.
            }
            if let Value::Put(value) = value {
                results.push((key, value)); // Tombstones are hidden.
            }
        }
        Ok(results)
    }

    /// Writes the memtable to a new SSTable and empties the WAL.
    pub fn flush(&mut self) -> io::Result<()> {
        if self.memtable.is_empty() {
            return Ok(());
        }

        // a. Write the sorted memtable to a new file. The memtable iterates
        //    in key order, which is exactly what the builder needs.
        let id = self.manifest.allocate_id();
        let path = table_path(&self.dir, id);
        let mut builder = SsTableBuilder::create(
            &path,
            self.options.block_size,
            self.options.bloom_bits_per_key,
        )?;
        for (key, value) in self.memtable.iter() {
            builder.add(key, value)?;
        }
        builder.finish()?;
        let table = SsTable::open(&path, id)?;

        // b. Make it official: record it in the manifest (newest first).
        //    Until this rename happens, a crash would leave just an orphan
        //    file, and the data would still be in the WAL.
        let mut ids = vec![id];
        ids.extend_from_slice(self.manifest.tables());
        self.manifest.set_tables(ids);
        self.manifest.save()?;
        self.tables.insert(0, table);

        // c. The data is safely in an SSTable, so the WAL can be emptied.
        //    (If we crash right before this, replaying the WAL just rewrites
        //    the same values into the memtable. Harmless.)
        self.wal.reset()?;
        self.memtable = MemTable::new();
        self.stats.flushes += 1;

        // d. Too many tables? Merge them.
        let trigger = self.options.compaction_trigger;
        if trigger > 0 && self.tables.len() >= trigger {
            self.compact()?;
        }
        Ok(())
    }

    /// Merges all SSTables into one, dropping overwritten values and
    /// tombstones. See `compaction.rs`.
    pub fn compact(&mut self) -> io::Result<()> {
        if self.tables.is_empty() {
            return Ok(());
        }

        let id = self.manifest.allocate_id();
        let path = table_path(&self.dir, id);
        let live_entries = compact_tables(
            &self.tables,
            &path,
            self.options.block_size,
            self.options.bloom_bits_per_key,
        )?;

        // If everything was deleted, there's no need to keep an empty file.
        let new_tables = if live_entries == 0 {
            fs::remove_file(&path)?;
            Vec::new()
        } else {
            vec![SsTable::open(&path, id)?]
        };

        // Switch the manifest to the new table first...
        self.manifest
            .set_tables(new_tables.iter().map(SsTable::id).collect());
        self.manifest.save()?;

        // ...and only then delete the old files. If we crash in between, the
        // old files are orphans and `open` will clean them up.
        let old_tables = std::mem::replace(&mut self.tables, new_tables);
        for table in old_tables {
            let path = table.path().to_path_buf();
            drop(table); // Close the file before deleting it.
            fs::remove_file(path)?;
        }
        sync_dir(&self.dir)?;

        self.stats.compactions += 1;
        Ok(())
    }

    pub fn stats(&self) -> &Stats {
        &self.stats
    }

    /// The live SSTables, newest first.
    pub fn tables(&self) -> &[SsTable] {
        &self.tables
    }

    /// Number of entries currently in the memtable.
    pub fn memtable_len(&self) -> usize {
        self.memtable.len()
    }

    /// Estimated size of the memtable in bytes.
    pub fn memtable_size(&self) -> usize {
        self.memtable.approx_size()
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }
}

/// SSTable file name for an id, e.g. `000042.sst`. Zero-padding makes `ls`
/// show them in order.
fn table_path(dir: &Path, id: u64) -> PathBuf {
    dir.join(format!("{id:06}.sst"))
}

/// Deletes `.sst` files that the manifest doesn't list, plus any leftover
/// temporary manifest.
fn remove_orphan_tables(dir: &Path, live: &[u64]) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");

        let is_orphan = match name.strip_suffix(".sst") {
            Some(stem) => stem.parse::<u64>().is_ok_and(|id| !live.contains(&id)),
            None => name == format!("{MANIFEST_FILE}.tmp"),
        };
        if is_orphan {
            fs::remove_file(&path)?;
        }
    }
    Ok(())
}
