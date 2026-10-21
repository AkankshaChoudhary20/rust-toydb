# toydb

A persistent key-value storage engine built from scratch in Rust, LSM-tree
style. This is a learning project: every on-disk structure (write-ahead log,
memtable, SSTable, compaction) is implemented directly on top of
`std::fs`/`std::io`, with no external database crate (no `sled`, `rocksdb`,
etc.) doing the actual work.

## Architecture

Data flows through three layers, the classic LSM-tree design used by systems
like LevelDB/RocksDB/Cassandra, simplified down to the essentials:

```
put/delete
    |
    v
 [ WAL ]  --append + fsync-->  wal.log            (durability)
    |
    v
 [ Memtable ]  BTreeMap<Vec<u8>, Option<Vec<u8>>>  (in-memory, sorted)
    |
    | once size >= threshold
    v
 [ SSTable ]  sstables/0000000001.sst, 0000000002.sst, ...  (immutable, sorted, on disk)
    |
    | once file count > threshold
    v
 [ Compaction ]  merge all sstables -> one new sstable, drop tombstones/old versions
```

**Write-ahead log (WAL).** Every `put`/`delete` is first appended to
`<db-dir>/wal.log` and `fsync`'d, *then* applied to the memtable. If the
process is killed at any point after that fsync returns, the write is
guaranteed to be recoverable. Each record is a small binary frame: a 1-byte
op code (`PUT`/`DELETE`), a length-prefixed key, and (for `PUT`) a
length-prefixed value.

**Memtable.** A `BTreeMap<Vec<u8>, Option<Vec<u8>>>` holding the most recent
state of every recently-written key. `None` is a tombstone, marking a
deleted key so the delete itself is remembered (and can shadow older data in
SSTables) until compaction removes it for good. Being a `BTreeMap` means it's
already sorted, so flushing it to disk requires no extra sort step.

**SSTables (Sorted String Tables).** When the memtable's estimated byte size
crosses a threshold, it is written out as an immutable, key-sorted file
under `<db-dir>/sstables/`, named by an increasing sequence number
(`0000000001.sst`, ...). The memtable is cleared and the WAL is truncated,
since everything it held is now durably on disk in the new SSTable. Because
each file is sorted, a point lookup can stop scanning as soon as it passes
the target key.

**Reads.** `get` checks the memtable first (most recent state), then falls
back to SSTables from newest to oldest, returning the first match. `scan`
merges every SSTable (oldest to newest) with the memtable on top, so later
writes always shadow earlier ones, then filters by prefix and drops
tombstones.

**Compaction.** Once the number of SSTable files exceeds a threshold, all of
them are merged into a single new SSTable: for each key, only the most
recent version survives, and tombstones are dropped entirely (a delete has
nothing older left to shadow once merged). The old files are then deleted.
This is what actually reclaims space from deleted/overwritten keys and keeps
the number of files (and therefore point-lookup cost) bounded.

**Startup / recovery.** `Db::open` creates the data directory and
`sstables/` subdirectory if needed, replays `wal.log` into a fresh memtable
(reconstructing any writes made since the last flush), then opens the WAL
for further appends and discovers existing SSTable files on disk. This is
what makes data durable across a process restart or crash.

## On-disk layout

```
<db-dir>/
  wal.log                  # current write-ahead log (append-only, truncated on flush)
  sstables/
    0000000001.sst
    0000000002.sst
    ...
```

## Usage

### As a library

```rust
use toydb::Db;
use std::path::Path;

fn main() -> toydb::Result<()> {
    let mut db = Db::open(Path::new("./my-data"))?;

    db.put(b"user:1".to_vec(), b"alice".to_vec())?;
    db.put(b"user:2".to_vec(), b"bob".to_vec())?;
    db.delete(b"user:1".to_vec())?;

    assert_eq!(db.get(b"user:1")?, None);
    assert_eq!(db.get(b"user:2")?, Some(b"bob".to_vec()));

    for (k, v) in db.scan(b"user:")? {
        println!("{} = {}", String::from_utf8_lossy(&k), String::from_utf8_lossy(&v));
    }

    db.flush()?;   // force memtable -> sstable now, instead of waiting for the size threshold
    db.compact()?; // force merging all sstables now

    Ok(())
}
```

`Db::open_with_options` accepts a `DbOptions { memtable_flush_bytes, compaction_threshold }`
if you want to tune the flush/compaction thresholds (the defaults are 256 KiB
and 4 files respectively).

### CLI

```
$ cargo run --release --bin toydb-cli -- ./my-data
toydb REPL -- data dir: "./my-data"
commands: SET k v | GET k | DEL k | SCAN prefix | EXIT
toydb> SET name alice
OK
toydb> GET name
alice
toydb> SCAN na
name = alice
toydb> DEL name
OK
toydb> GET name
(nil)
toydb> EXIT
```

Data written this way persists in `./my-data` and will be there (replayed
from the WAL and/or read from SSTables) the next time the CLI is pointed at
the same directory.

## Running tests

```
cargo build --release
cargo test
```

Tests cover: put/get/delete/overwrite semantics, WAL replay after
dropping and reopening a `Db` at the same path (simulating a crash/restart),
flush-to-SSTable then read-after-flush correctness, prefix scans that merge
memtable and SSTable state, and compaction correctness (deleted/overwritten
keys do not resurface after compacting, including after a subsequent
reopen).

## Limitations

- No concurrency: `Db` is not `Sync`/thread-safe, and there's no
  locking between processes over the same directory.
- No transactions or atomic multi-key batches.
- No secondary indexes; `scan` is a prefix scan over the primary key only,
  implemented by loading and merging full SSTable contents rather than
  seeking within them.
- No checksums (CRC) on WAL/SSTable records; corruption within a record
  isn't detected, only clean truncation at end-of-file is handled gracefully.
- No bloom filters or block indexes, so a point lookup that misses can still
  require scanning (part of) every SSTable on disk.
- Compaction is full (all files merged at once), not leveled/tiered, so its
  cost grows with total data size rather than being incremental.
- Memtable size is an approximate byte count (sum of key/value lengths), not
  actual heap usage.

## Possible extensions

- Concurrency: wrap `Db` in a lock, or move to a single-writer/multi-reader
  design with a background flush/compaction thread.
- Bloom filters per SSTable to skip files that can't contain a given key.
- A sparse in-memory index (key -> file offset) per SSTable to allow seeking
  instead of scanning.
- Checksums per record (and per file) to detect corruption instead of only
  handling truncation.
- Leveled compaction (multiple levels with size ratios) instead of
  merge-everything compaction.
- Range-aware SSTable metadata (min/max key) to skip files during `get`/`scan`.
- A simple transaction/batch API (atomic multi-key writes via a single WAL
  record).

## Continuous integration

CI runs `cargo test --locked` (including integration and documentation tests) and builds release binaries with the stable Rust toolchain.

Checks run on pushes and pull requests and can be started manually from the Actions tab. The workflow has read-only repository permissions, cancels superseded runs, and stops after 15 minutes.
