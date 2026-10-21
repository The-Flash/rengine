//! # rengine: a tiny LSM-tree storage engine for learning
//!
//! A storage engine is the part of a database that decides how data is laid
//! out on disk and how to find it again quickly. This crate implements a
//! small but complete **LSM-tree** (Log-Structured Merge-tree), the design
//! used by LevelDB, RocksDB, Cassandra and many others.
//!
//! Start with `README.md` for the big picture. Suggested reading order for
//! the code (each module starts with an explanation):
//!
//! | Module         | Concept                                              |
//! |----------------|------------------------------------------------------|
//! | [`entry`]      | key/value records, tombstones, binary encoding       |
//! | [`checksum`]   | CRC32 for detecting corruption                       |
//! | [`memtable`]   | sorted in-memory write buffer                        |
//! | [`wal`]        | write-ahead log, fsync, crash recovery               |
//! | [`sstable`]    | sorted on-disk files, blocks, the sparse index       |
//! | [`bloom`]      | bloom filters: skipping files without reading them   |
//! | [`merge`]      | k-way merge with a heap: newest version wins         |
//! | [`compaction`] | merging files, dropping old versions and tombstones  |
//! | [`manifest`]   | tracking live files, atomic rename                   |
//! | [`db`]         | the engine: write path, read path, recovery          |
//!
//! ```no_run
//! use rengine::{Db, Options};
//!
//! let mut db = Db::open("my-data", Options::default())?;
//! db.put(b"name", b"Ada")?;
//! assert_eq!(db.get(b"name")?, Some(b"Ada".to_vec()));
//! db.delete(b"name")?;
//! assert_eq!(db.get(b"name")?, None);
//! # Ok::<(), std::io::Error>(())
//! ```

// All modules are public so the examples (and you!) can poke at the internals.
pub mod bloom;
pub mod checksum;
pub mod compaction;
pub mod db;
pub mod entry;
pub mod manifest;
pub mod memtable;
pub mod merge;
pub mod sstable;
pub mod stats;
pub mod wal;

pub use db::{Db, Options};
pub use entry::Value;
pub use stats::Stats;

/// Helpers shared by the unit tests.
#[cfg(test)]
pub(crate) mod test_util {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Creates a fresh, empty directory for one test. Tests run in parallel,
    /// so every call gets a unique name.
    pub fn temp_dir(name: &str) -> PathBuf {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("rengine-unit-{name}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}
