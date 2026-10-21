//! # The manifest: which files make up the database right now?
//!
//! The data directory can contain SSTable files that are **not** part of the
//! database: half-written tables from a crash during a flush, or old tables
//! that compaction replaced but didn't get to delete. We need a single source
//! of truth for which tables are live. That's the **manifest**.
//!
//! It's a tiny text file (so you can `cat` it):
//!
//! ```text
//! next_id 8
//! table 7
//! table 5
//! table 3
//! ```
//!
//! Tables are listed **newest first**, which is the order reads check them.
//!
//! ## Updating it atomically
//!
//! If we overwrote the manifest in place and crashed halfway, we'd have a
//! broken manifest and lose track of everything. Instead we use the
//! **write-to-temp-then-rename** trick:
//!
//! 1. Write the new contents to `MANIFEST.tmp` and fsync it.
//! 2. `rename("MANIFEST.tmp", "MANIFEST")`.
//! 3. fsync the directory, so the rename itself is durable.
//!
//! On POSIX systems, `rename` is **atomic**: anyone looking at the directory
//! sees either the complete old file or the complete new file, never a mix.
//! This trick is used by text editors, package managers and databases alike.
//! (Real engines like LevelDB append small "edit" records to a log instead of
//! rewriting the whole file, but the goal is the same.)

use crate::entry::corrupt;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

pub const MANIFEST_FILE: &str = "MANIFEST";
const MANIFEST_TMP_FILE: &str = "MANIFEST.tmp";

pub struct Manifest {
    dir: PathBuf,
    /// Id to give the next SSTable we create. Ids only ever go up, so a
    /// higher id always means a newer table.
    next_id: u64,
    /// Ids of the live SSTables, newest first.
    tables: Vec<u64>,
}

impl Manifest {
    /// Loads the manifest from `dir`, or creates an empty one.
    pub fn load_or_create(dir: &Path) -> io::Result<Self> {
        match fs::read_to_string(dir.join(MANIFEST_FILE)) {
            Ok(text) => Self::parse(dir, &text),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let manifest = Manifest {
                    dir: dir.to_path_buf(),
                    next_id: 1,
                    tables: Vec::new(),
                };
                manifest.save()?;
                Ok(manifest)
            }
            Err(error) => Err(error),
        }
    }

    fn parse(dir: &Path, text: &str) -> io::Result<Self> {
        let mut next_id = None;
        let mut tables = Vec::new();
        for line in text.lines().filter(|line| !line.trim().is_empty()) {
            let (word, number) = line
                .split_once(' ')
                .ok_or_else(|| corrupt(format!("bad manifest line: {line:?}")))?;
            let number: u64 = number
                .trim()
                .parse()
                .map_err(|_| corrupt(format!("bad number in manifest line: {line:?}")))?;
            match word {
                "next_id" => next_id = Some(number),
                "table" => tables.push(number),
                _ => return Err(corrupt(format!("unknown manifest line: {line:?}"))),
            }
        }
        Ok(Manifest {
            dir: dir.to_path_buf(),
            next_id: next_id.ok_or_else(|| corrupt("manifest has no next_id line"))?,
            tables,
        })
    }

    /// Writes the manifest to disk atomically (see the module docs).
    pub fn save(&self) -> io::Result<()> {
        let mut text = format!("next_id {}\n", self.next_id);
        for id in &self.tables {
            text.push_str(&format!("table {id}\n"));
        }

        let tmp_path = self.dir.join(MANIFEST_TMP_FILE);
        let mut file = File::create(&tmp_path)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;

        fs::rename(&tmp_path, self.dir.join(MANIFEST_FILE))?;
        sync_dir(&self.dir)
    }

    /// Reserves a fresh id for a new SSTable. (It is persisted on the next
    /// `save`.)
    pub fn allocate_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// Live table ids, newest first.
    pub fn tables(&self) -> &[u64] {
        &self.tables
    }

    /// Replaces the list of live tables (newest first). Call `save` after.
    pub fn set_tables(&mut self, tables: Vec<u64>) {
        self.tables = tables;
    }
}

/// Makes changes to a directory (new files, renames, deletes) durable.
///
/// Creating or renaming a file changes the *directory*, which is stored
/// separately from the file's contents. On Linux especially, you need to
/// fsync the directory too, or the new name can vanish after a crash.
pub fn sync_dir(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    File::open(dir)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = dir; // Windows doesn't support opening directories this way.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::temp_dir;

    #[test]
    fn round_trips_through_disk() {
        let dir = temp_dir("manifest");
        let mut manifest = Manifest::load_or_create(&dir).unwrap();
        let a = manifest.allocate_id();
        let b = manifest.allocate_id();
        manifest.set_tables(vec![b, a]);
        manifest.save().unwrap();

        let loaded = Manifest::load_or_create(&dir).unwrap();
        assert_eq!(loaded.tables(), &[b, a]);
        assert_eq!(loaded.next_id, 3);
        assert!(!dir.join(MANIFEST_TMP_FILE).exists());
    }
}
