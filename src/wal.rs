use std::fs::{File, OpenOptions};
use std::io::{BufReader, BufWriter, Seek, Write};
use std::path::{Path, PathBuf};

use crate::error::Result;
use crate::record::{read_record, write_record};

/// Append-only write-ahead log. Every mutation is written (and fsynced) here
/// before it is applied to the memtable, so a crash between the two can never
/// lose an acknowledged write: replaying the WAL on the next `Db::open` rebuilds
/// exactly the memtable state that existed before the crash.
pub struct Wal {
    path: PathBuf,
    writer: BufWriter<File>,
}

impl Wal {
    pub fn open(path: &Path) -> Result<Self> {
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .open(path)?;
        // A crash can leave a partial final record. Remove it before appending,
        // otherwise the next durable write would become part of that broken record.
        let valid_length = {
            let mut reader = BufReader::new(&file);
            loop {
                let start = reader.stream_position()?;
                if read_record(&mut reader)?.is_none() {
                    break start;
                }
            }
        };
        file.set_len(valid_length)?;
        Ok(Self {
            path: path.to_path_buf(),
            writer: BufWriter::new(file),
        })
    }

    pub fn append_put(&mut self, key: &[u8], value: &[u8]) -> Result<()> {
        write_record(&mut self.writer, key, Some(value))?;
        self.sync()
    }

    pub fn append_delete(&mut self, key: &[u8]) -> Result<()> {
        write_record(&mut self.writer, key, None)?;
        self.sync()
    }

    fn sync(&mut self) -> Result<()> {
        self.writer.flush()?;
        self.writer.get_ref().sync_all()?;
        Ok(())
    }

    /// Replay every record in the WAL file at `path` (if it exists) through `apply`,
    /// in the order they were written, so the caller can rebuild memtable state.
    pub fn replay<F: FnMut(Vec<u8>, Option<Vec<u8>>)>(path: &Path, mut apply: F) -> Result<()> {
        if !path.exists() {
            return Ok(());
        }
        let file = File::open(path)?;
        let mut reader = BufReader::new(file);
        while let Some((key, value)) = read_record(&mut reader)? {
            apply(key, value);
        }
        Ok(())
    }

    /// Truncate the log to empty. Called right after a memtable flush, once every
    /// record it contains is durably present in a new SSTable, so replaying it
    /// again on the next open would be redundant.
    pub fn reset(&mut self) -> Result<()> {
        self.writer.flush()?;
        let file = OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&self.path)?;
        self.writer = BufWriter::new(file);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replay_reproduces_writes_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wal.log");

        {
            let mut wal = Wal::open(&path).unwrap();
            wal.append_put(b"a", b"1").unwrap();
            wal.append_put(b"b", b"2").unwrap();
            wal.append_delete(b"a").unwrap();
        }

        let mut seen = Vec::new();
        Wal::replay(&path, |k, v| seen.push((k, v))).unwrap();

        assert_eq!(
            seen,
            vec![
                (b"a".to_vec(), Some(b"1".to_vec())),
                (b"b".to_vec(), Some(b"2".to_vec())),
                (b"a".to_vec(), None),
            ]
        );
    }

    #[test]
    fn reopening_discards_partial_tail_before_new_writes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wal.log");
        let mut encoded = Vec::new();
        write_record(&mut encoded, b"partial", Some(b"value")).unwrap();
        for cut in 1..encoded.len() {
            {
                let mut wal = Wal::open(&path).unwrap();
                wal.reset().unwrap();
                wal.append_put(b"before", b"1").unwrap();
            }
            OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap()
                .write_all(&encoded[..cut])
                .unwrap();
            {
                let mut wal = Wal::open(&path).unwrap();
                wal.append_put(b"after", b"2").unwrap();
            }
            let mut seen = Vec::new();
            Wal::replay(&path, |k, v| seen.push((k, v))).unwrap();
            assert_eq!(
                seen,
                vec![
                    (b"before".to_vec(), Some(b"1".to_vec())),
                    (b"after".to_vec(), Some(b"2".to_vec()))
                ],
                "partial length {cut}"
            );
        }
    }

    #[test]
    fn reset_truncates_log() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wal.log");

        let mut wal = Wal::open(&path).unwrap();
        wal.append_put(b"a", b"1").unwrap();
        wal.reset().unwrap();
        wal.append_put(b"b", b"2").unwrap();
        drop(wal);

        let mut seen = Vec::new();
        Wal::replay(&path, |k, v| seen.push((k, v))).unwrap();
        assert_eq!(seen, vec![(b"b".to_vec(), Some(b"2".to_vec()))]);
    }
}
