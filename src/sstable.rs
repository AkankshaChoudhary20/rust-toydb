use std::fs::{self, File};
use std::io::{BufReader, BufWriter};
use std::path::{Path, PathBuf};

use crate::error::Result;
use crate::record::{read_record, write_record, Entry};

/// An immutable, sorted, on-disk table produced by flushing a memtable (or by
/// compacting older tables together). Entries are stored in ascending key order,
/// which lets point lookups stop scanning as soon as they pass the target key.
pub struct SSTable;

impl SSTable {
    /// Writes a new SSTable file from an iterator that yields entries in ascending
    /// key order (a `BTreeMap`'s iterator already satisfies this).
    pub fn write_from_sorted<I>(path: &Path, entries: I) -> Result<()>
    where
        I: Iterator<Item = Entry>,
    {
        let file = File::create(path)?;
        let mut writer = BufWriter::new(file);
        for (k, v) in entries {
            write_record(&mut writer, &k, v.as_deref())?;
        }
        use std::io::Write;
        writer.flush()?;
        writer.get_ref().sync_all()?;
        Ok(())
    }

    /// Point lookup within a single table.
    ///
    /// - `Ok(Some(Some(value)))` -- key present with a live value.
    /// - `Ok(Some(None))`        -- key present but tombstoned (deleted).
    /// - `Ok(None)`              -- key not present in this table at all.
    pub fn get(path: &Path, key: &[u8]) -> Result<Option<Option<Vec<u8>>>> {
        let file = File::open(path)?;
        let mut reader = BufReader::new(file);
        while let Some((k, v)) = read_record(&mut reader)? {
            if k.as_slice() == key {
                return Ok(Some(v));
            }
            if k.as_slice() > key {
                // Table is sorted ascending, so no later record can match.
                break;
            }
        }
        Ok(None)
    }

    /// Loads every entry in the table. Used by `scan` and by compaction.
    pub fn load_all(path: &Path) -> Result<Vec<Entry>> {
        let file = File::open(path)?;
        let mut reader = BufReader::new(file);
        let mut out = Vec::new();
        while let Some(entry) = read_record(&mut reader)? {
            out.push(entry);
        }
        Ok(out)
    }
}

pub fn sstable_path(dir: &Path, id: u64) -> PathBuf {
    dir.join(format!("{:010}.sst", id))
}

/// Scans `dir` for `NNNNNNNNNN.sst` files and returns their ids in ascending
/// (oldest-first) order.
pub fn list_sstable_ids(dir: &Path) -> Result<Vec<u64>> {
    let mut ids = Vec::new();
    if !dir.exists() {
        return Ok(ids);
    }
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let file_name = entry.file_name();
        let name = file_name.to_string_lossy();
        if let Some(stripped) = name.strip_suffix(".sst") {
            if let Ok(id) = stripped.parse::<u64>() {
                ids.push(id);
            }
        }
    }
    ids.sort_unstable();
    Ok(ids)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_and_point_lookup() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("0000000000.sst");
        let entries = vec![
            (b"a".to_vec(), Some(b"1".to_vec())),
            (b"b".to_vec(), None),
            (b"c".to_vec(), Some(b"3".to_vec())),
        ];
        SSTable::write_from_sorted(&path, entries.into_iter()).unwrap();

        assert_eq!(SSTable::get(&path, b"a").unwrap(), Some(Some(b"1".to_vec())));
        assert_eq!(SSTable::get(&path, b"b").unwrap(), Some(None));
        assert_eq!(SSTable::get(&path, b"c").unwrap(), Some(Some(b"3".to_vec())));
        assert_eq!(SSTable::get(&path, b"z").unwrap(), None);
        assert_eq!(SSTable::get(&path, b"0").unwrap(), None);
    }

    #[test]
    fn list_ids_sorted() {
        let dir = tempfile::tempdir().unwrap();
        for id in [5u64, 1, 3] {
            let path = sstable_path(dir.path(), id);
            SSTable::write_from_sorted(&path, std::iter::empty()).unwrap();
        }
        assert_eq!(list_sstable_ids(dir.path()).unwrap(), vec![1, 3, 5]);
    }
}
