//! End-to-end tests that use the engine the way a user would: open, write,
//! read, close, reopen, and simulate crashes.

use rengine::{Db, Options};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;

/// A fresh, empty directory for one test.
fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("rengine-it-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// Tiny sizes so a few hundred writes produce several SSTables.
fn small_options() -> Options {
    Options {
        memtable_size_limit: 2 * 1024,
        block_size: 256,
        sync_writes: false,
        compaction_trigger: 0,
        ..Options::default()
    }
}

fn key(i: usize) -> Vec<u8> {
    format!("key{i:04}").into_bytes()
}

#[test]
fn put_get_overwrite_delete() {
    let mut db = Db::open(temp_dir("basic"), small_options()).unwrap();

    db.put(b"a", b"1").unwrap();
    assert_eq!(db.get(b"a").unwrap(), Some(b"1".to_vec()));

    db.put(b"a", b"2").unwrap();
    assert_eq!(db.get(b"a").unwrap(), Some(b"2".to_vec()));

    db.delete(b"a").unwrap();
    assert_eq!(db.get(b"a").unwrap(), None);
    assert_eq!(db.get(b"never-written").unwrap(), None);
}

#[test]
fn newer_values_and_tombstones_win_across_sstables() {
    let mut db = Db::open(temp_dir("layers"), small_options()).unwrap();

    for i in 0..300 {
        db.put(&key(i), b"old").unwrap();
    }
    db.flush().unwrap();
    for i in (0..300).step_by(2) {
        db.put(&key(i), b"new").unwrap();
    }
    for i in (0..300).step_by(3) {
        db.delete(&key(i)).unwrap();
    }
    assert!(db.tables().len() > 2, "test wants several sstables");

    for i in 0..300 {
        let expected = if i % 3 == 0 {
            None
        } else if i % 2 == 0 {
            Some(b"new".to_vec())
        } else {
            Some(b"old".to_vec())
        };
        assert_eq!(db.get(&key(i)).unwrap(), expected, "key {i}");
    }
}

#[test]
fn scan_merges_memtable_and_sstables() {
    let mut db = Db::open(temp_dir("scan"), small_options()).unwrap();
    for i in 0..200 {
        db.put(&key(i), format!("v{i}").as_bytes()).unwrap();
    }
    db.delete(&key(105)).unwrap();
    db.put(&key(110), b"updated").unwrap(); // stays in the memtable

    let results = db.scan(&key(100), Some(&key(120))).unwrap();
    let keys: Vec<Vec<u8>> = results.iter().map(|(k, _)| k.clone()).collect();
    let expected: Vec<Vec<u8>> = (100..120).filter(|&i| i != 105).map(key).collect();
    assert_eq!(keys, expected);
    assert_eq!(results[9], (key(110), b"updated".to_vec()));

    assert_eq!(db.scan(b"", None).unwrap().len(), 199);
}

#[test]
fn data_survives_reopen() {
    let dir = temp_dir("reopen");
    {
        let mut db = Db::open(&dir, small_options()).unwrap();
        for i in 0..500 {
            db.put(&key(i), b"value").unwrap();
        }
        db.delete(&key(7)).unwrap();
        // Some data is in SSTables, the rest only in the WAL + memtable.
        assert!(!db.tables().is_empty());
        assert!(db.memtable_len() > 0);
    } // `db` is dropped here, like closing the program.

    let mut db = Db::open(&dir, small_options()).unwrap();
    assert!(db.stats().wal_records_recovered > 0);
    for i in 0..500 {
        let expected = if i == 7 {
            None
        } else {
            Some(b"value".to_vec())
        };
        assert_eq!(db.get(&key(i)).unwrap(), expected, "key {i}");
    }
}

#[test]
fn compaction_keeps_latest_values_and_drops_garbage() {
    let dir = temp_dir("compact");
    let mut db = Db::open(&dir, small_options()).unwrap();
    for round in 0..3 {
        for i in 0..200 {
            db.put(&key(i), format!("round{round}").as_bytes()).unwrap();
        }
    }
    for i in 0..50 {
        db.delete(&key(i)).unwrap();
    }
    db.flush().unwrap();
    let files_before = db.tables().len();
    assert!(files_before > 3);

    db.compact().unwrap();
    assert_eq!(db.tables().len(), 1);
    // 600 puts + 50 deletes collapsed into the 150 live keys.
    assert_eq!(db.tables()[0].entry_count(), 150);
    for i in 0..200 {
        let expected = if i < 50 {
            None
        } else {
            Some(b"round2".to_vec())
        };
        assert_eq!(db.get(&key(i)).unwrap(), expected);
    }

    // Old files are gone from disk; only one .sst remains.
    let sst_files = std::fs::read_dir(&dir)
        .unwrap()
        .filter(|e| {
            e.as_ref()
                .unwrap()
                .path()
                .extension()
                .is_some_and(|x| x == "sst")
        })
        .count();
    assert_eq!(sst_files, 1);

    // And it all survives a reopen.
    drop(db);
    let mut db = Db::open(&dir, small_options()).unwrap();
    assert_eq!(db.get(&key(199)).unwrap(), Some(b"round2".to_vec()));
}

#[test]
fn recovers_from_a_torn_wal_write() {
    let dir = temp_dir("torn");
    {
        let mut db = Db::open(&dir, small_options()).unwrap();
        db.put(b"safe", b"yes").unwrap();
    }
    // Simulate a crash in the middle of writing the next record: append half
    // a record's worth of garbage to the log.
    let mut wal = OpenOptions::new()
        .append(true)
        .open(dir.join("wal.log"))
        .unwrap();
    wal.write_all(&[0xAB, 0xCD, 0x01, 0x00, 0x00]).unwrap();
    drop(wal);

    {
        let mut db = Db::open(&dir, small_options()).unwrap();
        assert_eq!(db.get(b"safe").unwrap(), Some(b"yes".to_vec()));
        // New writes after recovery must not be lost behind the garbage.
        db.put(b"after", b"crash").unwrap();
    }
    let mut db = Db::open(&dir, small_options()).unwrap();
    assert_eq!(db.get(b"safe").unwrap(), Some(b"yes".to_vec()));
    assert_eq!(db.get(b"after").unwrap(), Some(b"crash".to_vec()));
}

#[test]
fn orphan_sstables_are_cleaned_up_on_open() {
    let dir = temp_dir("orphan");
    {
        let mut db = Db::open(&dir, small_options()).unwrap();
        db.put(b"k", b"v").unwrap();
        db.flush().unwrap();
    }
    // A file left behind by a flush that crashed before updating the manifest.
    let orphan = dir.join("999999.sst");
    std::fs::write(&orphan, b"half-written garbage").unwrap();

    let mut db = Db::open(&dir, small_options()).unwrap();
    assert!(!orphan.exists());
    assert_eq!(db.get(b"k").unwrap(), Some(b"v".to_vec()));
}
