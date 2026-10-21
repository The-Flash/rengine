# rengine: build a storage engine to learn how databases store data

`rengine` is a small key-value **storage engine** written in Rust for
learning. It has no dependencies, and a large share of its ~2,900 lines
(including tests) are comments. It implements an **LSM-tree** (Log-Structured
Merge-tree), the design behind LevelDB, RocksDB, Cassandra, ScyllaDB,
CockroachDB's Pebble and many others.

You can `put`, `get`, `delete` and `scan` keys. Data survives crashes and
restarts, and lookups stay fast as the data grows, because of the on-disk
index and bloom filters this README explains.

```text
rengine> put name Ada Lovelace
ok
rengine> get name
Ada Lovelace
rengine> fill 300
inserted 300 keys
  [engine] 2 flush(es), 0 compaction(s): sstables 0 -> 2
```

---

## Table of contents

1. [Quick start](#1-quick-start)
2. [What is a storage engine?](#2-what-is-a-storage-engine)
3. [Why disks change everything](#3-why-disks-change-everything)
4. [Two families: B-trees and LSM-trees](#4-two-families-b-trees-and-lsm-trees)
5. [The big picture of rengine](#5-the-big-picture-of-rengine)
6. [Walkthrough: what happens on put, get, delete, scan](#6-walkthrough-what-happens-on-put-get-delete-scan)
7. [Deep dives, one concept at a time](#7-deep-dives-one-concept-at-a-time)
   - [7.1 Binary encoding & length prefixes](#71-binary-encoding--length-prefixes)
   - [7.2 The write-ahead log & fsync](#72-the-write-ahead-log--fsync)
   - [7.3 The memtable](#73-the-memtable)
   - [7.4 SSTables, blocks and the sparse index](#74-sstables-blocks-and-the-sparse-index)
   - [7.5 Bloom filters](#75-bloom-filters)
   - [7.6 Tombstones](#76-tombstones)
   - [7.7 The k-way merge](#77-the-k-way-merge)
   - [7.8 Compaction](#78-compaction)
   - [7.9 The manifest & atomic rename](#79-the-manifest--atomic-rename)
   - [7.10 Checksums](#710-checksums)
   - [7.11 Crash recovery, step by step](#711-crash-recovery-step-by-step)
8. [Hands-on: reading an SSTable byte by byte](#8-hands-on-reading-an-sstable-byte-by-byte)
9. [Seeing the index work: the benchmark](#9-seeing-the-index-work-the-benchmark)
10. [Code map & reading order](#10-code-map--reading-order)
11. [What this POC leaves out](#11-what-this-poc-leaves-out)
12. [Exercises](#12-exercises)
13. [Glossary](#13-glossary)
14. [Further reading](#14-further-reading)

---

## 1. Quick start

You need Rust (`rustup` from https://rustup.rs).

```sh
cargo run                                   # interactive shell, data in ./rengine-data
cargo test                                  # 28 unit + integration tests
cargo run --release --example benchmark     # see the index & bloom filters at work
cargo doc --open                            # browse the code docs in a browser
```

Inside the shell, type `help`. A good first session:

```text
put hello world        # store a key
get hello              # read it back
fill 300               # insert 300 keys: watch flushes happen
tables                 # list the SSTable files on disk
get key00042           # read from disk
stats                  # see bloom skips and blocks read
del key00042           # delete (writes a tombstone)
scan key00040 key00045 # range query
compact                # merge all files into one
quit
```

Then run `cargo run` again: everything is still there. Open a second
terminal and run `ls -l rengine-data` and `cat rengine-data/MANIFEST` while
you play.

> The shell uses tiny sizes (4 KB memtable, 512-byte blocks) so you can see
> things happen after a few writes. Real engines use megabytes.

---

## 2. What is a storage engine?

A database like PostgreSQL or MySQL has many layers: a network protocol, an
SQL parser, a query planner, transactions... At the bottom sits the
**storage engine**, which answers a much simpler question:

> Given a key, how do I store its value on disk, and how do I find it again
> quickly?

```text
   SQL: SELECT * FROM users WHERE id = 42
                 |
        +--------v---------+
        |  query planner   |
        +--------+---------+
                 |   get(key = "users/42")
        +--------v---------+
        |  STORAGE ENGINE  |   <- this project
        +--------+---------+
                 |   read block at offset 81920
        +--------v---------+
        |   disk / SSD     |
        +------------------+
```

Many databases let you swap engines: MySQL has InnoDB and MyRocks, MongoDB
has WiredTiger, and CockroachDB, TiDB and Kafka Streams are built on
RocksDB-style engines. Under the hood, nearly all of them are a
**key-value store** with **sorted keys**. Sorted keys make range queries
(`id BETWEEN 10 AND 20`) possible, and they are what this project is about.

---

## 3. Why disks change everything

If everything fit in RAM, we'd use a hash map or a sorted tree and be done.
But data must survive power loss and is often far bigger than RAM, so it
lives on disk. Disks have very different rules from memory:

| Operation                    | Rough time      |
|------------------------------|-----------------|
| Read from RAM                | ~100 ns         |
| Random read from SSD         | ~100,000 ns     |
| Random read from hard drive  | ~10,000,000 ns  |

Three facts follow, and every storage engine is designed around them:

1. **Disk reads are expensive. Minimize them.** A lookup that does 1 disk
   read instead of 20 is 20× faster. This is the job of an **index**.
2. **Disks read in blocks (pages).** Reading 1 byte costs about the same as
   reading 4 KB, because the hardware and the OS move whole 4 KB pages. So
   we group data into blocks and make every block read count.
3. **Sequential I/O is much faster than random I/O.** Writing one big chunk
   to the end of a file beats writing many small pieces scattered around.
   This is especially true on hard drives, and still true on SSDs.

---

## 4. Two families: B-trees and LSM-trees

There are two dominant designs.

### B-trees (update in place)

Used by PostgreSQL, MySQL/InnoDB, SQLite. Data lives in fixed-size pages
organized as a wide, shallow tree. To write a key you find its page and
**modify it in place**.

```text
                  [ 100 | 200 ]                <- root page
                 /      |      \
       [ 20 | 50 ]  [ 120 | 170 ]  [ 250 | 300 ]
        /  |  \       ...             ...
     pages with the actual keys and values
```

* Reads: excellent. 3 to 4 page reads find any key among billions.
* Writes: each write may rewrite a whole 4 KB page somewhere random on disk.

### LSM-trees (append, sort, merge)

Used by LevelDB, RocksDB, Cassandra, and **this project**. Writes never modify
existing files. They go into memory, are written out as **new sorted
files**, and those files are **merged** in the background.

* Writes: excellent. Everything is sequential.
* Reads: a key may be in any of several files, so we need tricks (sparse
  indexes, bloom filters, compaction) to keep reads fast. Those tricks are
  most of what this project teaches.

### The trade-off triangle

Every engine balances three kinds of "amplification":

* **Write amplification**: how many bytes hit the disk per byte the user
  writes. (LSM compaction rewrites data several times.)
* **Read amplification**: how many disk reads one lookup needs. (An LSM may
  check several files.)
* **Space amplification**: how much extra disk space is used. (An LSM keeps
  old versions until compaction.)

You can't minimize all three at once. Choosing an engine means choosing which
one to pay.

---

## 5. The big picture of rengine

```text
                        put / delete                    get / scan
                            |                               |
                            v                               |
   +----------------+   1. append   +-------------+         |
   |  wal.log       |<--------------|             |         |
   |  (on disk)     |               |   Db        |         |
   +----------------+               |             |         |
                                    +------+------+         |
                                 2. insert |                |
                                           v                v
                          +-----------------------------------+
   MEMORY                 |  MemTable (sorted BTreeMap)       |  <- checked 1st
                          +-----------------+-----------------+
  - - - - - - - - - - - - - - - - - - - - - | - - - - - - - - - - - - - - -
                                            | 3. flush when full
   DISK                                     v
                          +-----------------------------------+
                          |  000007.sst  (newest)             |  <- checked 2nd
                          +-----------------------------------+
                          |  000005.sst                       |  <- 3rd
                          +-----------------------------------+
                          |  000003.sst  (oldest)             |  <- last
                          +-----------------------------------+
                                            |
                                            | 4. compaction merges them
                                            v
                          +-----------------------------------+
                          |  000008.sst  (everything, clean)  |
                          +-----------------------------------+

   MANIFEST  =  "the live tables are 7, 5, 3"  (newest first)
```

The data directory after a while:

```text
rengine-data/
├── MANIFEST       # text file: which .sst files are live
├── wal.log        # recent writes not yet in an SSTable
├── 000003.sst     # sorted, immutable data files
├── 000005.sst
└── 000007.sst
```

---

## 6. Walkthrough: what happens on put, get, delete, scan

### `put("cat", "meow")`

```text
1. WAL:       append [crc][len][Put "cat" "meow"] to wal.log, then fsync.
              From now on the write survives a crash.
2. MemTable:  insert "cat" -> Put("meow") into the sorted in-memory map.
              From now on reads can see it.
3. Full?      If the memtable is over its size limit, flush it:
              a. write all its entries, in sorted order, to 000009.sst
              b. update MANIFEST to include table 9 (atomic rename)
              c. truncate wal.log (the data is safe in the .sst now)
              d. if there are too many tables, compact
```

Code: `Db::write` and `Db::flush` in `src/db.rs`.

### `get("cat")`

```text
1. MemTable has "cat"?          yes -> return it (no disk access at all)
2. For each SSTable, newest first:
     a. bloom filter says "definitely not here"?  -> skip file (0 reads)
     b. binary-search the in-memory sparse index -> the one block
        that could hold "cat"
     c. read that block from disk (1 read), scan it
     d. found an entry?  -> return it. The newest one wins.
3. Nowhere?  -> None
```

Code: `Db::get` in `src/db.rs`, `SsTable::get` in `src/sstable/reader.rs`.

### `delete("cat")`

Exactly like `put`, but the value is a **tombstone** (a "deleted" marker).
Older files still contain `cat = meow`, but `get` meets the tombstone first
and reports "not found". See [7.6](#76-tombstones).

### `scan("b", "d")`

```text
1. Make one sorted iterator per source: memtable range + each SSTable
   (each SSTable iterator uses the index to jump to the block holding "b").
2. Merge them with a heap: output keys in order; for duplicate keys keep
   the newest version only.
3. Skip tombstones; stop at the first key >= "d".
```

Code: `Db::scan` in `src/db.rs`, `MergeIterator` in `src/merge.rs`.

---

## 7. Deep dives, one concept at a time

Each section matches a source file. The file's top comment says the same
thing in more detail, next to the code.

### 7.1 Binary encoding & length prefixes
**File: `src/entry.rs`**

Everything on disk is just bytes, so we need a **format**: rules for turning
a key and value into bytes and back. We use **length-prefixed** fields:

```text
+----------+-------------+---------------+-----------+-------------+
| kind: u8 | key_len:u32 | value_len:u32 | key bytes | value bytes |
+----------+-------------+---------------+-----------+-------------+
kind: 0 = Put, 1 = Tombstone
```

Writing the length *before* the data tells the reader exactly how many bytes
to take, so keys and values can contain any bytes at all, including spaces,
newlines and zero bytes. Compare that with CSV, where a comma inside a value
breaks everything.

Integers are stored **little-endian** (least significant byte first), the
native order of x86 and ARM CPUs. The number 5 as a `u32` is `05 00 00 00`.

### 7.2 The write-ahead log & fsync
**File: `src/wal.rs`**

The memtable lives in RAM, so a crash would wipe it. The fix: **before**
changing memory, append the change to a log file. On startup, replay the log.

```text
wal.log:  [crc|len|Put a=1] [crc|len|Put b=2] [crc|len|Del a] [crc|len|Put c...
                                                                  ^ crash here
replay:   Put a=1, Put b=2, Del a   -> memtable = { b=2 }
          last record is cut short (torn write) -> ignored and trimmed
```

**`fsync`** is the subtle part. `write()` normally just copies bytes into the
OS's memory (the *page cache*), and the OS writes them to disk later. If the
power fails first, the data is gone even though `write()` succeeded. `fsync`
(`File::sync_data` in Rust) forces the data onto the physical device. It's
slow, often around a millisecond, so:

* `Options::sync_writes = true` (the default) fsyncs every write: safe and slow.
* The benchmark turns it off to measure everything else.
* Real databases use **group commit**: wait a moment, then fsync a batch of
  writes from many users together.

Once the memtable is flushed to an SSTable, the log is no longer needed, so
it is truncated to zero bytes.

### 7.3 The memtable
**File: `src/memtable.rs`**

An in-memory map that keeps keys **sorted**. We use Rust's `BTreeMap`.
(LevelDB and RocksDB use a *skip list*, which is easier to make lock-free for
concurrent writers.)

Why sorted? Because when it's full we write it to disk **in order**, in one
sequential pass, and sorted files are what make the index possible. This is
the central LSM idea:

> Accept writes in any order in memory, and write them to disk sorted and
> sequentially.

### 7.4 SSTables, blocks and the sparse index
**Files: `src/sstable/{mod,builder,reader,iter}.rs`**

This is the most important section for understanding **how data is indexed
on disk to make search fast**.

An **SSTable** (*Sorted String Table*) is an immutable file of entries sorted
by key:

```text
+--------------------------------------------------+
| data block 0   key000 .. key039                  |  ~4 KB each,
| data block 1   key040 .. key079                  |  each ends with a CRC32
| ...                                              |
| data block N   key960 .. key999                  |
+--------------------------------------------------+
| index block    key000 -> (offset 0,    len 4101) |  one entry per BLOCK
|                key040 -> (offset 4101, len 4097) |
|                ...                               |
+--------------------------------------------------+
| bloom filter                                     |
+--------------------------------------------------+
| footer: where index & bloom are, count, "RENGINE1"|  always the last 48 bytes
+--------------------------------------------------+
```

#### Idea 1: Sorting enables binary search

In a sorted list you can find anything with **binary search**: look at the
middle, decide which half the key is in, repeat. One million items take only
about 20 steps (log₂ 1,000,000 ≈ 20). Without sorting, you'd check all one
million.

#### Idea 2: Blocks match how disks work

Disks read whole pages, so we group entries into ~4 KB **blocks** and always
read a whole block. One block read gets us ~40 entries for the price of one.

#### Idea 3: A *sparse* index fits in memory

A **dense** index has one entry per key. For 100 million keys that's
gigabytes, too much for RAM. A **sparse** index has one entry per *block*:
the block's first key and its location. That's about 40 times smaller, small
enough to load into memory when the table is opened.

The lookup becomes:

```text
index (in RAM):  ["apple"@0] ["grape"@4101] ["melon"@8198] ["peach"@12301]

get("kiwi"):
  binary search the index: "grape" <= "kiwi" < "melon"
  -> read ONE block from disk at offset 4101
  -> scan its ~40 entries for "kiwi"
```

**At most one disk read per file**, however big the file is. In the
benchmark, a 12 MB file with 2,942 blocks needs exactly **1 block read** per
lookup, compared with ~1,463 for scanning from the start.

The code is short. This is the real lookup from `src/sstable/reader.rs`:

```rust
let count = self.index.partition_point(|handle| handle.first_key.as_slice() <= key);
count.checked_sub(1)   // the last block whose first key is <= key
```

`partition_point` is the standard library's binary search.

#### Idea 4: The footer is the entry point

The index is at the *end* of the file because we only know where blocks end
after writing them. A reader can't know where that is, so the last 48 bytes
are a fixed-size **footer** that points to the index and the bloom filter.
Open = read the footer, read the index, read the bloom filter. Data blocks
are read only when needed.

#### Idea 5: Immutability makes life simple

SSTables are never modified after being written. No locks are needed to read
them, they can be cached forever, a crash can't leave one half-modified, and
backups are just file copies. The cost: updates and deletes need new entries
(and tombstones), and compaction to clean up.

### 7.5 Bloom filters
**File: `src/bloom.rs`**

The sparse index makes each file cost **at most** one block read. But with 12
files, a key that doesn't exist could still cost 12 reads. A **bloom filter**
lets us skip files that definitely don't have the key, without reading them.

It's an array of bits. To **add** a key, hash it `k` ways and set those `k`
bits. To **check** a key, look at the same `k` bits:

```text
bits:            0 1 1 0 0 1 0 0 0 1 0 1
check "cow" ->   positions 3, 5, 9:  bit 3 is 0  => DEFINITELY NOT here
check "cat" ->   positions 2, 5, 9:  all 1       => MAYBE here (go read the block)
```

* If any bit is 0, the key was **never added**. That answer is always right.
* If all bits are 1, the key is **probably** there. Sometimes other keys set
  those bits: a **false positive**. We then do a block read that finds nothing.
  That's wasteful, but still correct.

The false positive rate depends on bits per key. With 10 bits per key
(1.25 bytes!) and `k = 7` hash functions, it's about **1%**. The benchmark
shows the effect:

```text
missing keys,  bloom ON :  0.05 blocks read/lookup
missing keys,  bloom OFF:  6.36 blocks read/lookup     <- ~130x more disk reads
```

Rather than computing 7 separate hashes, we use **double hashing**: one hash
`h` and a step `delta`, with positions `h, h+delta, h+2·delta, ...`.

### 7.6 Tombstones
**File: `src/entry.rs`, used everywhere**

Files are immutable, so how do you delete `cat` when it lives in an old file?
You write a newer entry saying "cat is deleted": a **tombstone**.

```text
memtable:     cat = TOMBSTONE     <- get("cat") stops here: "not found"
000007.sst:   ...
000003.sst:   cat = "meow"        <- still on disk, but hidden
```

Tombstones are removed during compaction, along with the old values they
hide. A tombstone can only be dropped once nothing older is left for it to
hide. Otherwise the deleted value would come back, a classic real-world bug
in distributed databases.

### 7.7 The k-way merge
**File: `src/merge.rs`**

Scans and compaction both need to combine several sorted sources into one
sorted stream, keeping only the **newest** version of each key. This is the
merge step of merge sort, generalized to `k` inputs:

```text
memtable (newest):  b=7            e=X
table 2:            b=5      d=1   e=2
table 1 (oldest):   a=1  b=1             f=9
                    ---------------------------
merged:             a=1  b=7   d=1  e=X  f=9
```

We put the current head of each source in a **min-heap** (priority queue),
ordered by `(key, source age)`. We repeatedly pop the smallest, discard any
older copies of the same key, and refill from the sources we took from. Each
step costs `O(log k)`.

The output is **lazy**: it's an `Iterator`, so a scan that stops early never
reads the remaining blocks.

### 7.8 Compaction
**File: `src/compaction.rs`**

Without compaction, files pile up: reads slow down, overwritten values waste
space, and deleted data never goes away. Compaction merges tables (with the
k-way merge) into a new one containing only the latest live values, then
deletes the old files.

```text
before: 4 files, 600 puts + 50 deletes      after: 1 file, 150 live entries
```

rengine uses the simplest strategy, **full compaction**: everything is merged
into one file when there are `compaction_trigger` tables. Real engines
compact in small pieces:

* **Leveled** (LevelDB/RocksDB): level 0 holds fresh flushes; each deeper
  level is ~10× bigger and holds non-overlapping files. Compaction merges one
  file into the overlapping files of the next level. Reads check about one
  file per level.
* **Size-tiered** (Cassandra): merge groups of similar-size files. Less
  rewriting, but more files to check on reads.

### 7.9 The manifest & atomic rename
**File: `src/manifest.rs`**

After a crash, the directory might contain a half-written `.sst` from an
interrupted flush, or old tables compaction didn't get to delete. Which files
are real? The **manifest** is the single source of truth:

```text
$ cat rengine-data/MANIFEST
next_id 3
table 2
table 1
```

Updating it safely uses a classic trick:

```text
1. write the new content to MANIFEST.tmp, fsync it
2. rename MANIFEST.tmp -> MANIFEST       (atomic!)
3. fsync the directory                   (makes the rename durable)
```

`rename` is **atomic** on POSIX systems: after a crash you see the whole old
file or the whole new file, never half of each. The order of operations is
what makes crashes safe:

* **Flush**: write the `.sst` → update the manifest → truncate the WAL.
  Crash before the manifest update? The `.sst` is an orphan (deleted on next
  open) and the data is still in the WAL. Crash after it? The WAL is replayed
  into the memtable again, which is harmless because the values are the same.
* **Compaction**: write the new `.sst` → update the manifest → delete the old
  files. Crash at any point leaves either the old set or the new set, plus
  some orphans to clean up.

### 7.10 Checksums
**File: `src/checksum.rs`**

Every WAL record and every SSTable block stores a **CRC32** checksum of its
bytes. On read we recompute it. A mismatch means the data is damaged (a torn
write, a bad disk sector, a flipped bit), and we report an error instead of
returning garbage. CRC32 is implemented by hand in about 30 lines using a
lookup table built at compile time (`const fn`).

### 7.11 Crash recovery, step by step
**File: `src/db.rs` (`Db::open`)**

```text
1. Read MANIFEST                -> which .sst files are live; open each
                                   (loads footer, index, bloom filter)
2. Delete orphan .sst files     -> debris from interrupted flush/compaction
3. Replay wal.log               -> rebuild the memtable
     stop at the first record that's cut short or has a bad CRC
     truncate the file there so new writes don't land after garbage
```

The integration tests simulate these crashes: `recovers_from_a_torn_wal_write`
and `orphan_sstables_are_cleaned_up_on_open` in `tests/integration.rs`.

---

## 8. Hands-on: reading an SSTable byte by byte

Binary formats feel like magic until you read one yourself. Create a tiny
table:

```sh
printf 'put apple red\nput banana yellow\ndel cherry\nflush\n' | cargo run -- /tmp/hexdemo
xxd /tmp/hexdemo/000001.sst
```

Output (151 bytes):

```text
00000000: 0005 0000 0003 0000 0061 7070 6c65 7265  .........applere
00000010: 6400 0600 0000 0600 0000 6261 6e61 6e61  d.........banana
00000020: 7965 6c6c 6f77 0106 0000 0000 0000 0063  yellow.........c
00000030: 6865 7272 79af 2163 5401 0000 0005 0000  herry.!cT.......
00000040: 0061 7070 6c65 0000 0000 0000 0000 3900  .apple........9.
00000050: 0000 3e2c c9d4 0700 0000 01cc d080 1804  ..>,............
00000060: 2127 1801 b3d0 3900 0000 0000 0000 1d00  !'....9.........
00000070: 0000 0000 0000 5600 0000 0000 0000 1000  ......V.........
00000080: 0000 0000 0000 0300 0000 0000 0000 5245  ..............RE
00000090: 4e47 494e 4531                           NGINE1
```

Decoded (offsets in hex):

```text
DATA BLOCK 0   (0x00 - 0x38, 57 bytes)
  0x00  00                    kind = Put
  0x01  05 00 00 00           key_len = 5
  0x05  03 00 00 00           value_len = 3
  0x09  61 70 70 6c 65        "apple"
  0x0e  72 65 64              "red"
  0x11  00 06000000 06000000  Put, key_len 6, value_len 6
  0x1a  "banana" "yellow"
  0x26  01 06000000 00000000  TOMBSTONE, key_len 6, value_len 0
  0x2f  "cherry"
  0x35  af 21 63 54           CRC32 of bytes 0x00..0x35

INDEX BLOCK    (0x39 - 0x55, 29 bytes)
  0x39  01 00 00 00           1 block in this table
  0x3d  05 00 00 00 "apple"   block 0's first key
  0x46  00 00 00 00 00 00 00 00   block 0 offset = 0
  0x4e  39 00 00 00           block 0 length = 0x39 = 57
  0x52  3e 2c c9 d4           CRC32

BLOOM FILTER   (0x56 - 0x65, 16 bytes)
  0x56  07 00 00 00           k = 7 hash functions
  0x5a  01 cc d0 80 18 04 21 27   64 bits (the minimum size)
  0x62  18 01 b3 d0           CRC32

FOOTER         (0x66 - 0x95, always 48 bytes)
  0x66  39 00.. (u64)         index offset = 0x39
  0x6e  1d 00.. (u64)         index length = 29
  0x76  56 00.. (u64)         bloom offset = 0x56
  0x7e  10 00.. (u64)         bloom length = 16
  0x86  03 00.. (u64)         3 entries
  0x8e  "RENGINE1"            magic number
```

Notice:

* The tombstone for `cherry` is a real entry on disk.
* The keys are sorted, even though we could have typed them in any order.
* Try `cat /tmp/hexdemo/MANIFEST`, then put a few keys and `xxd wal.log` to
  see WAL records (`[crc][len][entry]`).

---

## 9. Seeing the index work: the benchmark

```sh
cargo run --release --example benchmark
```

Sample output (from an Apple Silicon laptop):

```text
== 1. Writing 100000 keys ==
   139288 writes/sec, 12 SSTables created

== 2. Random lookups across 12 SSTables ==
   existing keys, bloom ON :  1.00 blocks read/lookup,  5.62 bloom skips/lookup,  21.35µs/lookup
   missing keys,  bloom ON :  0.05 blocks read/lookup, 11.90 bloom skips/lookup,   1.93µs/lookup
   missing keys,  bloom OFF:  6.36 blocks read/lookup,  0.00 bloom skips/lookup, 114.39µs/lookup

== 3. Compaction ==
   merged into 1 table in 188.90ms: 100000 entries, 2942 blocks, 12131 KB

== 4. Sparse index vs. scanning the file ==
   with index:        1.0 blocks/lookup     20.81µs/lookup
   linear scan:    1463.1 blocks/lookup     31.07ms/lookup
```

How to read it:

* **Existing keys cost exactly 1 block read.** The bloom filters skip the
  ~5.6 files that don't have the key, and the sparse index leads straight to
  the right block in the file that does.
* **Missing keys cost ~0 block reads with bloom filters** and ~6 without.
  (Without bloom, files whose key range doesn't cover the key are still
  skipped by the index: the key sorts before their first key.)
* **Index vs. no index: ~1,500× fewer block reads.** The time gap here is
  "only" ~1,500× because the OS has the whole file cached in RAM. On a cold
  disk each block read is a real I/O, and the gap is what decides whether a
  query takes microseconds or seconds.

---

## 10. Code map & reading order

Read in this order. Every file opens with a comment explaining its concept.

| # | File                     | Concept                                         |
|---|--------------------------|-------------------------------------------------|
| 1 | `src/entry.rs`           | Entries, tombstones, binary encoding            |
| 2 | `src/checksum.rs`        | CRC32, detecting corruption                     |
| 3 | `src/memtable.rs`        | Sorted in-memory buffer                         |
| 4 | `src/wal.rs`             | Write-ahead log, fsync, torn writes             |
| 5 | `src/sstable/mod.rs`     | SSTable layout, blocks, sparse index, footer    |
| 6 | `src/sstable/builder.rs` | Writing an SSTable in one sequential pass       |
| 7 | `src/bloom.rs`           | Bloom filters                                   |
| 8 | `src/sstable/reader.rs`  | Binary search + one block read per lookup       |
| 9 | `src/sstable/iter.rs`    | Streaming a table one block at a time           |
|10 | `src/merge.rs`           | k-way merge with a heap                         |
|11 | `src/compaction.rs`      | Merging tables, dropping garbage                |
|12 | `src/manifest.rs`        | Live-file tracking, atomic rename, dir fsync    |
|13 | `src/db.rs`              | The engine: write path, read path, recovery     |
|14 | `src/main.rs`            | Interactive shell                               |
|15 | `examples/benchmark.rs`  | Measuring the index and bloom filters           |
|16 | `tests/integration.rs`   | End-to-end and crash tests                      |

---

## 11. What this POC leaves out

rengine is complete enough to be correct, but it skips many things production
engines do:

* **Concurrency.** It's single-threaded. Real engines let many threads read
  and write at once, flush and compact in background threads, and keep
  writing to a new memtable while the old one is being flushed.
* **Leveled compaction.** We rewrite everything on each compaction.
* **Block cache.** We read blocks from the OS every time. Real engines keep
  hot blocks in an in-process LRU cache.
* **Compression.** Real engines compress each block (Snappy, LZ4, Zstd), and
  use **prefix compression** inside blocks (`user:0001`, `user:0002` share a
  long prefix).
* **Binary search within a block.** We scan a block linearly. LevelDB stores
  "restart points" so it can binary-search inside a block too.
* **Snapshots / MVCC.** Real engines tag each entry with a sequence number,
  so readers can see a consistent snapshot while writes continue.
* **Group commit, write batches, transactions.**
* **Zero-copy decoding.** We copy every key while scanning a block. Real
  engines compare bytes in place.

---

## 12. Exercises

Ordered roughly from easiest to hardest. The tests are your safety net:
run `cargo test` after each change.

1. **Key range check.** Store each table's smallest *and* largest key in
   the footer. In `SsTable::get`, skip the table when the key is outside that
   range, before even checking the bloom filter. How does the "bloom OFF"
   benchmark change?
2. **Tune the bloom filter.** Try `bloom_bits_per_key` of 2, 5, 10 and 20
   in the benchmark. Plot false positives against filter size. Does it match
   the `0.6185^bits` formula?
3. **Block cache.** Add a `HashMap<(table_id, block_index), Vec<u8>>` with a
   size limit and LRU eviction in front of `read_data_block`.
4. **Prefix compression.** In each block, store only the part of each key
   that differs from the previous key. Measure the file size difference.
5. **Write batches.** Add `Db::write_batch(Vec<(key, Value)>)` that writes
   several entries as **one** WAL record, so they all survive a crash or none
   do (atomicity).
6. **Restart points.** Every 16 entries, record the entry's offset at the end
   of the block, then binary-search those offsets in `search_blocks`.
7. **Leveled compaction.** Split tables into levels with size limits and only
   merge overlapping key ranges.
8. **Background compaction.** Move compaction to a thread. You'll need to
   think hard about what `get` sees while files are being swapped.

---

## 13. Glossary

| Term | Meaning |
|---|---|
| **Block** | A ~4 KB chunk of an SSTable; the unit of disk reads. |
| **Bloom filter** | A compact bit array that answers "definitely not present" or "maybe present". |
| **Checksum / CRC32** | A small number computed from data, used to detect corruption. |
| **Compaction** | Merging SSTables to remove old versions and tombstones. |
| **Dense index** | One index entry per key. |
| **Flush** | Writing the memtable to disk as a new SSTable. |
| **Footer** | Fixed-size end of an SSTable that points to the index and bloom filter. |
| **fsync** | System call that forces buffered writes onto the physical disk. |
| **Immutable** | Never modified after being written. |
| **k-way merge** | Merging k sorted inputs into one sorted output. |
| **Little-endian** | Byte order with the least significant byte first. |
| **LSM-tree** | Log-Structured Merge-tree: buffer writes in memory, write sorted files, merge them. |
| **Magic number** | Fixed bytes identifying a file type (`RENGINE1` here). |
| **Manifest** | File listing which SSTables are currently live. |
| **Memtable** | Sorted in-memory buffer of recent writes. |
| **Page cache** | The OS's in-memory cache of file contents. |
| **Read / write / space amplification** | Extra reads / writes / disk space beyond the logical minimum. |
| **Sequential I/O** | Reading or writing consecutive bytes; much faster than random I/O. |
| **Sparse index** | One index entry per block instead of per key. |
| **SSTable** | Sorted String Table: an immutable, sorted, indexed data file. |
| **Tombstone** | A marker entry meaning "this key was deleted". |
| **Torn write** | A write that was only partly persisted because of a crash. |
| **WAL** | Write-ahead log: changes are logged before being applied, for crash recovery. |

---

## 14. Further reading

* **Designing Data-Intensive Applications** by Martin Kleppmann, chapter 3
  ("Storage and Retrieval"). The best introduction to this topic. If you read
  one thing, read this.
* **LevelDB implementation notes**: `doc/impl.md` and `doc/table_format.md`
  in https://github.com/google/leveldb. rengine's SSTable format is a
  simplified version of LevelDB's.
* **The original LSM-tree paper**: O'Neil et al., *The Log-Structured
  Merge-Tree (LSM-Tree)*, 1996.
* **RocksDB wiki**: https://github.com/facebook/rocksdb/wiki, especially the
  pages on compaction and bloom filters.
* **Database Internals** by Alex Petrov: in-depth coverage of both B-trees
  and LSM-trees.
* **mini-lsm**: https://github.com/skyzh/mini-lsm, a step-by-step course on
  building a more complete LSM engine in Rust. A great next step after this
  project.

---

## License

Licensed under the [Apache License, Version 2.0](LICENSE).
