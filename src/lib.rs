//! toydb: a small, dependency-free, persistent key-value storage engine,
//! built LSM-tree style, as a learning project.
//!
//! See the crate README for an architecture overview. In short: writes go to
//! an append-only write-ahead log (WAL) and an in-memory sorted memtable; once
//! the memtable grows past a threshold it is flushed to an immutable, sorted
//! SSTable file on disk; reads check the memtable, then SSTables newest to
//! oldest; and compaction periodically merges SSTables to reclaim space used
//! by deleted or overwritten keys.

mod db;
mod error;
mod memtable;
mod record;
mod sstable;
mod wal;

pub use db::{Db, DbOptions};
pub use error::{Error, Result};
