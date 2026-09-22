use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::error::Result;
use crate::memtable::MemTable;
use crate::sstable::{self, SSTable};
use crate::wal::Wal;

/// Flush the memtable once its estimated size (sum of key + value bytes)
/// reaches this many bytes.
const DEFAULT_MEMTABLE_FLUSH_BYTES: usize = 256 * 1024;

/// Trigger compaction once more than this many SSTable files exist on disk.
const DEFAULT_COMPACTION_THRESHOLD: usize = 4;

#[derive(Clone, Copy, Debug)]
pub struct DbOptions {
    pub memtable_flush_bytes: usize,
    pub compaction_threshold: usize,
}

impl Default for DbOptions {
    fn default() -> Self {
        Self {
            memtable_flush_bytes: DEFAULT_MEMTABLE_FLUSH_BYTES,
            compaction_threshold: DEFAULT_COMPACTION_THRESHOLD,
        }
    }
}

/// A single-process, embedded key-value store: memtable + WAL + SSTables,
/// LSM-tree style. See the crate-level README for the on-disk layout.
pub struct Db {
    dir: PathBuf,
    sst_dir: PathBuf,
    wal: Wal,
    memtable: MemTable,
    /// SSTable file ids, ascending (index 0 is the oldest table on disk).
    sstable_ids: Vec<u64>,
    next_sstable_id: u64,
    options: DbOptions,
}

impl Db {
    /// Opens (creating if necessary) a database rooted at `path`, a directory
    /// that will hold `wal.log` and an `sstables/` subdirectory. Replays the WAL
    /// to restore any writes that happened since the last flush, so data
    /// survives a process crash or restart.
    pub fn open(path: &Path) -> Result<Self> {
        Self::open_with_options(path, DbOptions::default())
    }

    pub fn open_with_options(path: &Path, options: DbOptions) -> Result<Self> {
        fs::create_dir_all(path)?;
        let sst_dir = path.join("sstables");
        fs::create_dir_all(&sst_dir)?;
        let wal_path = path.join("wal.log");

        let mut memtable = MemTable::new();
        Wal::replay(&wal_path, |k, v| match v {
            Some(v) => memtable.put(k, v),
            None => memtable.delete(k),
        })?;

        let wal = Wal::open(&wal_path)?;

        let sstable_ids = sstable::list_sstable_ids(&sst_dir)?;
        let next_sstable_id = sstable_ids.last().map(|id| id + 1).unwrap_or(0);

        Ok(Db {
            dir: path.to_path_buf(),
            sst_dir,
            wal,
            memtable,
            sstable_ids,
            next_sstable_id,
            options,
        })
    }

    pub fn put(&mut self, key: impl Into<Vec<u8>>, value: impl Into<Vec<u8>>) -> Result<()> {
        let key = key.into();
        let value = value.into();
        // Durability first: the WAL write (and fsync) must land before the
        // memtable is mutated, so a crash in between never loses the write.
        self.wal.append_put(&key, &value)?;
        self.memtable.put(key, value);
        self.maybe_flush()
    }

    pub fn delete(&mut self, key: impl Into<Vec<u8>>) -> Result<()> {
        let key = key.into();
        self.wal.append_delete(&key)?;
        self.memtable.delete(key);
        self.maybe_flush()
    }

    /// Reads first from the memtable (most recent state), then falls back to
    /// SSTables from newest to oldest, returning the first match found.
    pub fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        if let Some(v) = self.memtable.get(key) {
            return Ok(v);
        }
        for &id in self.sstable_ids.iter().rev() {
            let path = sstable::sstable_path(&self.sst_dir, id);
            if let Some(v) = SSTable::get(&path, key)? {
                return Ok(v);
            }
        }
        Ok(None)
    }

    /// Returns every live (non-deleted) key with the given prefix, merging
    /// SSTables (oldest to newest) and then the memtable on top so the most
    /// recent write for each key wins. Tombstones are filtered out.
    pub fn scan(&self, prefix: &[u8]) -> Result<impl Iterator<Item = (Vec<u8>, Vec<u8>)>> {
        let mut merged: BTreeMap<Vec<u8>, Option<Vec<u8>>> = BTreeMap::new();
        for &id in self.sstable_ids.iter() {
            let path = sstable::sstable_path(&self.sst_dir, id);
            for (k, v) in SSTable::load_all(&path)? {
                if k.starts_with(prefix) {
                    merged.insert(k, v);
                }
            }
        }
        for (k, v) in self.memtable.range_prefix(prefix) {
            merged.insert(k.clone(), v.clone());
        }
        Ok(merged.into_iter().filter_map(|(k, v)| v.map(|v| (k, v))))
    }

    /// Forces a flush of the memtable to a new SSTable file, regardless of the
    /// configured size threshold. Also resets the WAL, since everything it
    /// contains is now durably captured in the new table. A no-op if the
    /// memtable is empty.
    pub fn flush(&mut self) -> Result<()> {
        if self.memtable.is_empty() {
            return Ok(());
        }
        let id = self.next_sstable_id;
        self.next_sstable_id += 1;
        let path = sstable::sstable_path(&self.sst_dir, id);

        let entries: Vec<(Vec<u8>, Option<Vec<u8>>)> = self
            .memtable
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        SSTable::write_from_sorted(&path, entries.into_iter())?;

        self.sstable_ids.push(id);
        self.memtable.clear();
        self.wal.reset()?;

        if self.sstable_ids.len() > self.options.compaction_threshold {
            self.compact()?;
        }
        Ok(())
    }

    fn maybe_flush(&mut self) -> Result<()> {
        if self.memtable.size_bytes() >= self.options.memtable_flush_bytes {
            self.flush()?;
        }
        Ok(())
    }

    /// Merges every SSTable on disk into a single new one, keeping only the
    /// newest version of each key and dropping tombstones entirely (once
    /// merged, a delete has nothing older left to shadow, so it can be
    /// discarded along with whatever it deleted).
    pub fn compact(&mut self) -> Result<()> {
        if self.sstable_ids.len() <= 1 {
            return Ok(());
        }
        let mut merged: BTreeMap<Vec<u8>, Option<Vec<u8>>> = BTreeMap::new();
        for &id in self.sstable_ids.iter() {
            let path = sstable::sstable_path(&self.sst_dir, id);
            for (k, v) in SSTable::load_all(&path)? {
                merged.insert(k, v);
            }
        }
        let live: Vec<(Vec<u8>, Option<Vec<u8>>)> =
            merged.into_iter().filter(|(_, v)| v.is_some()).collect();

        let new_id = self.next_sstable_id;
        self.next_sstable_id += 1;
        let new_path = sstable::sstable_path(&self.sst_dir, new_id);
        SSTable::write_from_sorted(&new_path, live.into_iter())?;

        for &id in self.sstable_ids.iter() {
            let old_path = sstable::sstable_path(&self.sst_dir, id);
            let _ = fs::remove_file(old_path);
        }
        self.sstable_ids = vec![new_id];
        Ok(())
    }

    pub fn path(&self) -> &Path {
        &self.dir
    }

    /// Number of SSTable files currently on disk. Exposed for tests.
    pub fn sstable_count(&self) -> usize {
        self.sstable_ids.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn small_options() -> DbOptions {
        DbOptions {
            memtable_flush_bytes: 1, // flush after essentially every write
            compaction_threshold: 2,
        }
    }

    #[test]
    fn put_get_delete_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Db::open(dir.path()).unwrap();

        assert_eq!(db.get(b"k").unwrap(), None);

        db.put(b"k".to_vec(), b"v1".to_vec()).unwrap();
        assert_eq!(db.get(b"k").unwrap(), Some(b"v1".to_vec()));

        db.put(b"k".to_vec(), b"v2".to_vec()).unwrap();
        assert_eq!(db.get(b"k").unwrap(), Some(b"v2".to_vec()));

        db.delete(b"k".to_vec()).unwrap();
        assert_eq!(db.get(b"k").unwrap(), None);
    }

    #[test]
    fn flush_then_read_after_flush() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Db::open(dir.path()).unwrap();

        db.put(b"a".to_vec(), b"1".to_vec()).unwrap();
        db.put(b"b".to_vec(), b"2".to_vec()).unwrap();
        db.flush().unwrap();
        assert_eq!(db.sstable_count(), 1);

        // Values must still be readable purely from the SSTable now.
        assert_eq!(db.get(b"a").unwrap(), Some(b"1".to_vec()));
        assert_eq!(db.get(b"b").unwrap(), Some(b"2".to_vec()));

        // A later write shadows the flushed value.
        db.put(b"a".to_vec(), b"1-new".to_vec()).unwrap();
        assert_eq!(db.get(b"a").unwrap(), Some(b"1-new".to_vec()));
    }

    #[test]
    fn scan_with_prefix_merges_memtable_and_sstables() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Db::open(dir.path()).unwrap();

        db.put(b"user:1".to_vec(), b"alice".to_vec()).unwrap();
        db.put(b"user:2".to_vec(), b"bob".to_vec()).unwrap();
        db.put(b"order:1".to_vec(), b"widget".to_vec()).unwrap();
        db.flush().unwrap();

        // This one stays in the memtable, on top of the flushed table.
        db.put(b"user:3".to_vec(), b"carol".to_vec()).unwrap();
        db.delete(b"user:1".to_vec()).unwrap();

        let mut results: Vec<_> = db.scan(b"user:").unwrap().collect();
        results.sort();
        assert_eq!(
            results,
            vec![
                (b"user:2".to_vec(), b"bob".to_vec()),
                (b"user:3".to_vec(), b"carol".to_vec()),
            ]
        );
    }

    #[test]
    fn wal_replay_restores_state_after_reopen() {
        let dir = tempfile::tempdir().unwrap();

        {
            let mut db = Db::open(dir.path()).unwrap();
            db.put(b"a".to_vec(), b"1".to_vec()).unwrap();
            db.put(b"b".to_vec(), b"2".to_vec()).unwrap();
            db.delete(b"a".to_vec()).unwrap();
            db.put(b"c".to_vec(), b"3".to_vec()).unwrap();
            // Db is dropped here without an explicit flush, simulating a
            // crash right after the last WAL-durable write.
        }

        let db = Db::open(dir.path()).unwrap();
        assert_eq!(db.get(b"a").unwrap(), None);
        assert_eq!(db.get(b"b").unwrap(), Some(b"2".to_vec()));
        assert_eq!(db.get(b"c").unwrap(), Some(b"3".to_vec()));
    }

    #[test]
    fn wal_replay_after_a_flush_only_replays_the_remainder() {
        let dir = tempfile::tempdir().unwrap();

        {
            let mut db = Db::open(dir.path()).unwrap();
            db.put(b"a".to_vec(), b"1".to_vec()).unwrap();
            db.flush().unwrap(); // WAL reset here
            db.put(b"b".to_vec(), b"2".to_vec()).unwrap();
            // crash-simulated drop with "b" only in the WAL, "a" only in the sstable
        }

        let db = Db::open(dir.path()).unwrap();
        assert_eq!(db.get(b"a").unwrap(), Some(b"1".to_vec()));
        assert_eq!(db.get(b"b").unwrap(), Some(b"2".to_vec()));
    }

    #[test]
    fn compaction_drops_tombstones_and_superseded_values() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Db::open_with_options(dir.path(), small_options()).unwrap();

        // Each put/delete flushes immediately (threshold = 1 byte), producing
        // one sstable per operation and triggering compaction along the way.
        db.put(b"a".to_vec(), b"1".to_vec()).unwrap();
        db.put(b"a".to_vec(), b"2".to_vec()).unwrap(); // supersedes "1"
        db.put(b"b".to_vec(), b"1".to_vec()).unwrap();
        db.delete(b"b".to_vec()).unwrap(); // tombstone for b
        db.put(b"c".to_vec(), b"3".to_vec()).unwrap();

        db.compact().unwrap();
        assert_eq!(db.sstable_count(), 1);

        assert_eq!(db.get(b"a").unwrap(), Some(b"2".to_vec()));
        assert_eq!(db.get(b"b").unwrap(), None);
        assert_eq!(db.get(b"c").unwrap(), Some(b"3".to_vec()));

        // Reopening from disk must see the same, fully-compacted state.
        drop(db);
        let db2 = Db::open(dir.path()).unwrap();
        assert_eq!(db2.get(b"a").unwrap(), Some(b"2".to_vec()));
        assert_eq!(db2.get(b"b").unwrap(), None);
        assert_eq!(db2.get(b"c").unwrap(), Some(b"3".to_vec()));
    }

    #[test]
    fn automatic_compaction_triggers_past_threshold() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Db::open_with_options(dir.path(), small_options()).unwrap();

        for i in 0..5u32 {
            db.put(format!("k{i}").into_bytes(), b"v".to_vec()).unwrap();
        }
        // compaction_threshold = 2, so the table count should never be allowed
        // to run away with one sstable per write.
        assert!(db.sstable_count() <= 3, "count = {}", db.sstable_count());
    }
}
