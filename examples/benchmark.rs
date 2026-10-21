//! A small experiment that shows *why* the on-disk index and the bloom
//! filters matter. Run it in release mode:
//!
//! ```text
//! cargo run --release --example benchmark
//! ```
//!
//! It measures "blocks read from disk per lookup" as well as time, because
//! block reads are what an index is designed to minimize. (Times will look
//! very fast because the OS caches the files in RAM; on a cold disk, every
//! block read would cost ~0.1 ms on an SSD or ~10 ms on a hard drive.)

use rengine::{Db, Options, Stats};
use std::io;
use std::time::Instant;

const NUM_KEYS: u64 = 100_000;
const NUM_LOOKUPS: u64 = 10_000;
const NUM_LINEAR_LOOKUPS: u64 = 200;

/// A tiny random number generator (xorshift), so we need no dependencies.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

fn existing_key(i: u64) -> Vec<u8> {
    format!("user:{i:08}").into_bytes()
}

/// Sorts *between* existing keys, so the sparse index alone can't rule it out.
fn missing_key(i: u64) -> Vec<u8> {
    format!("user:{i:08}-missing").into_bytes()
}

fn main() -> io::Result<()> {
    let dir = std::env::temp_dir().join("rengine-benchmark");
    let _ = std::fs::remove_dir_all(&dir);

    let options = Options {
        memtable_size_limit: 1024 * 1024, // 1 MB memtables -> ~12 SSTables
        sync_writes: false,               // fsync per write would dominate
        compaction_trigger: 0,            // keep many tables for part 2
        ..Options::default()
    };

    // ---------------------------------------------------------------------
    println!("== 1. Writing {NUM_KEYS} keys ==");
    let mut db = Db::open(&dir, options.clone())?;
    let value = vec![b'x'; 100];
    let start = Instant::now();
    for i in 0..NUM_KEYS {
        db.put(&existing_key(i), &value)?;
    }
    db.flush()?;
    let elapsed = start.elapsed();
    println!(
        "   {:.0} writes/sec, {} SSTables created\n",
        NUM_KEYS as f64 / elapsed.as_secs_f64(),
        db.tables().len()
    );

    // ---------------------------------------------------------------------
    println!(
        "== 2. Random lookups across {} SSTables ==",
        db.tables().len()
    );
    let mut rng = Rng(42);
    measure(&mut db, "existing keys, bloom ON ", |_| {
        existing_key(rng.next() % NUM_KEYS)
    })?;
    measure(&mut db, "missing keys,  bloom ON ", |_| {
        missing_key(rng.next() % NUM_KEYS)
    })?;

    drop(db);
    let mut db = Db::open(
        &dir,
        Options {
            use_bloom_filter: false,
            ..options.clone()
        },
    )?;
    measure(&mut db, "missing keys,  bloom OFF", |_| {
        missing_key(rng.next() % NUM_KEYS)
    })?;
    println!("   -> without bloom filters, a missing key costs a disk read in many tables.\n");

    // ---------------------------------------------------------------------
    println!("== 3. Compaction ==");
    let start = Instant::now();
    db.compact()?;
    let table = &db.tables()[0];
    println!(
        "   merged into 1 table in {:.2?}: {} entries, {} blocks, {} KB, index+bloom kept in RAM\n",
        start.elapsed(),
        table.entry_count(),
        table.block_count(),
        table.file_size() / 1024
    );

    // ---------------------------------------------------------------------
    println!("== 4. Sparse index vs. scanning the file ==");
    let mut stats = Stats::default();
    let start = Instant::now();
    for _ in 0..NUM_LINEAR_LOOKUPS {
        let key = existing_key(rng.next() % NUM_KEYS);
        table.get(&key, true, &mut stats)?;
    }
    let indexed_time = start.elapsed() / NUM_LINEAR_LOOKUPS as u32;
    let indexed_blocks = stats.blocks_read as f64 / NUM_LINEAR_LOOKUPS as f64;

    let mut linear_blocks = 0;
    let start = Instant::now();
    for _ in 0..NUM_LINEAR_LOOKUPS {
        let key = existing_key(rng.next() % NUM_KEYS);
        // No index: read blocks from the start until we find the key.
        let mut iter = table.iter();
        for entry in iter.by_ref() {
            if entry?.0 == key {
                break;
            }
        }
        linear_blocks += iter.blocks_read();
    }
    let linear_time = start.elapsed() / NUM_LINEAR_LOOKUPS as u32;
    let linear_blocks = linear_blocks as f64 / NUM_LINEAR_LOOKUPS as f64;

    println!("   with index:    {indexed_blocks:>7.1} blocks/lookup  {indexed_time:>10.2?}/lookup");
    println!("   linear scan:   {linear_blocks:>7.1} blocks/lookup  {linear_time:>10.2?}/lookup");
    println!("   -> the index turns an O(n) scan into one binary search + one block read.");

    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}

/// Runs `NUM_LOOKUPS` gets and prints what they cost.
fn measure(db: &mut Db, label: &str, mut next_key: impl FnMut(u64) -> Vec<u8>) -> io::Result<()> {
    let before = db.stats().clone();
    let start = Instant::now();
    for i in 0..NUM_LOOKUPS {
        db.get(&next_key(i))?;
    }
    let per_lookup = start.elapsed() / NUM_LOOKUPS as u32;
    let after = db.stats();

    let n = NUM_LOOKUPS as f64;
    println!(
        "   {label}: {:>5.2} blocks read/lookup, {:>5.2} bloom skips/lookup, {:>8.2?}/lookup",
        (after.blocks_read - before.blocks_read) as f64 / n,
        (after.bloom_skips - before.bloom_skips) as f64 / n,
        per_lookup,
    );
    Ok(())
}
