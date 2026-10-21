//! An interactive shell for playing with the engine.
//!
//! ```text
//! cargo run                 # uses ./rengine-data
//! cargo run -- /tmp/mydb    # or any directory you like
//! ```
//!
//! It uses deliberately tiny sizes (4 KB memtable, 512-byte blocks) so that
//! flushes and compactions happen after just a few dozen writes, and you can
//! watch the files appear with `ls` in another terminal.

use rengine::{Db, Options};
use std::io::{self, BufRead, Write};

const HELP: &str = "\
commands:
  put <key> <value>     store a value (the value may contain spaces)
  get <key>             look up a key
  del <key>             delete a key (writes a tombstone)
  scan [start] [end]    list keys with start <= key < end
  fill <n>              insert n generated keys (key00000, key00001, ...)
  flush                 write the memtable to a new SSTable now
  compact               merge all SSTables into one
  tables                list the SSTables on disk
  stats                 show counters (bloom skips, blocks read, ...)
  help                  show this message
  quit                  exit (data is already on disk)";

fn main() -> io::Result<()> {
    let dir = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "rengine-data".to_string());
    let options = Options {
        memtable_size_limit: 4 * 1024,
        block_size: 512,
        compaction_trigger: 4,
        ..Options::default()
    };
    let mut db = Db::open(&dir, options)?;

    println!("rengine: opened {dir:?}");
    println!(
        "  {} sstable(s) on disk, {} entries recovered from the WAL into the memtable",
        db.tables().len(),
        db.stats().wal_records_recovered
    );
    println!("type `help` for commands\n");

    let stdin = io::stdin();
    loop {
        print!("rengine> ");
        io::stdout().flush()?;

        let mut line = String::new();
        if stdin.lock().read_line(&mut line)? == 0 {
            break; // End of input (Ctrl-D).
        }
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if line == "quit" || line == "exit" {
            break;
        }

        // Errors from a single command are printed, not fatal.
        if let Err(error) = run_command(&mut db, line) {
            println!("error: {error}");
        }
    }
    Ok(())
}

fn run_command(db: &mut Db, line: &str) -> io::Result<()> {
    // Split into at most 3 parts so values can contain spaces.
    let parts: Vec<&str> = line.splitn(3, ' ').collect();
    let tables_before = db.tables().len();
    let flushes_before = db.stats().flushes;
    let compactions_before = db.stats().compactions;

    match parts.as_slice() {
        ["put", key, value] => {
            db.put(key.as_bytes(), value.as_bytes())?;
            println!("ok");
        }
        ["get", key] => match db.get(key.as_bytes())? {
            Some(value) => println!("{}", String::from_utf8_lossy(&value)),
            None => println!("(not found)"),
        },
        ["del", key] => {
            db.delete(key.as_bytes())?;
            println!("ok (tombstone written)");
        }
        ["scan", rest @ ..] => {
            // `rest` may be ["a", "z"], ["a"] or [] (and "a z" may arrive as
            // one piece, because of splitn).
            let bounds: Vec<&str> = rest.iter().flat_map(|s| s.split_whitespace()).collect();
            let start = bounds.first().map(|s| s.as_bytes()).unwrap_or(b"");
            let end = bounds.get(1).map(|s| s.as_bytes());
            let results = db.scan(start, end)?;
            for (key, value) in &results {
                println!(
                    "{} = {}",
                    String::from_utf8_lossy(key),
                    String::from_utf8_lossy(value)
                );
            }
            println!("({} keys)", results.len());
        }
        ["fill", n] => {
            let n: usize = n
                .trim()
                .parse()
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "fill needs a number"))?;
            for i in 0..n {
                let key = format!("key{i:05}");
                let value = format!("value number {i}");
                db.put(key.as_bytes(), value.as_bytes())?;
            }
            println!("inserted {n} keys");
        }
        ["flush"] => {
            db.flush()?;
            println!("ok");
        }
        ["compact"] => {
            db.compact()?;
            println!("ok");
        }
        ["tables"] => print_tables(db),
        ["stats"] => println!("{}", db.stats()),
        ["help"] => println!("{HELP}"),
        _ => println!("unknown command; type `help`"),
    }

    // Tell the user when something interesting happened behind the scenes.
    let flushes = db.stats().flushes - flushes_before;
    let compactions = db.stats().compactions - compactions_before;
    if flushes > 0 || compactions > 0 {
        println!(
            "  [engine] {flushes} flush(es), {compactions} compaction(s): sstables {} -> {}",
            tables_before,
            db.tables().len()
        );
    }
    Ok(())
}

fn print_tables(db: &Db) {
    println!(
        "memtable: {} entries (~{} bytes)",
        db.memtable_len(),
        db.memtable_size()
    );
    if db.tables().is_empty() {
        println!("no sstables yet");
        return;
    }
    println!("sstables (newest first):");
    for table in db.tables() {
        let first_key = table
            .first_key()
            .map(|k| String::from_utf8_lossy(k).into_owned())
            .unwrap_or_default();
        println!(
            "  {:06}.sst  {:>6} entries  {:>3} blocks  {:>7} bytes  bloom {:>5} bytes  first key {first_key:?}",
            table.id(),
            table.entry_count(),
            table.block_count(),
            table.file_size(),
            table.bloom_size(),
        );
    }
}
