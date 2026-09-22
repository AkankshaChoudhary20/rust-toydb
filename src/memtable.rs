use std::collections::BTreeMap;

/// The in-memory table backing recent writes. Keys map to `Some(value)` for a live
/// write or `None` for a tombstone (a recorded delete). Kept sorted (BTreeMap) so it
/// can be flushed straight to a sorted SSTable file without an extra sort pass.
#[derive(Default)]
pub struct MemTable {
    map: BTreeMap<Vec<u8>, Option<Vec<u8>>>,
    size_bytes: usize,
}

impl MemTable {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn put(&mut self, key: Vec<u8>, value: Vec<u8>) {
        self.adjust_size(&key, Some(value.len()));
        self.map.insert(key, Some(value));
    }

    pub fn delete(&mut self, key: Vec<u8>) {
        self.adjust_size(&key, Some(0));
        self.map.insert(key, None);
    }

    /// Update the running byte-size estimate before overwriting `key`.
    fn adjust_size(&mut self, key: &[u8], new_value_len: Option<usize>) {
        if let Some(old) = self.map.get(key) {
            let old_len = old.as_ref().map(|v| v.len()).unwrap_or(0);
            self.size_bytes -= key.len() + old_len;
        }
        self.size_bytes += key.len() + new_value_len.unwrap_or(0);
    }

    /// Returns `Some(Some(value))` for a live entry, `Some(None)` for a tombstone,
    /// or `None` if the key has no entry in this memtable at all.
    pub fn get(&self, key: &[u8]) -> Option<Option<Vec<u8>>> {
        self.map.get(key).cloned()
    }

    pub fn size_bytes(&self) -> usize {
        self.size_bytes
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&Vec<u8>, &Option<Vec<u8>>)> {
        self.map.iter()
    }

    pub fn clear(&mut self) {
        self.map.clear();
        self.size_bytes = 0;
    }

    pub fn range_prefix<'a>(
        &'a self,
        prefix: &'a [u8],
    ) -> impl Iterator<Item = (&'a Vec<u8>, &'a Option<Vec<u8>>)> {
        self.map.iter().filter(move |(k, _)| k.starts_with(prefix))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn put_get_delete_overwrite() {
        let mut mt = MemTable::new();
        assert_eq!(mt.get(b"a"), None);

        mt.put(b"a".to_vec(), b"1".to_vec());
        assert_eq!(mt.get(b"a"), Some(Some(b"1".to_vec())));

        mt.put(b"a".to_vec(), b"2".to_vec());
        assert_eq!(mt.get(b"a"), Some(Some(b"2".to_vec())));

        mt.delete(b"a".to_vec());
        assert_eq!(mt.get(b"a"), Some(None));
        assert!(!mt.is_empty());
    }

    #[test]
    fn size_tracks_overwrites() {
        let mut mt = MemTable::new();
        mt.put(b"key".to_vec(), b"value".to_vec());
        let after_first = mt.size_bytes();
        assert_eq!(after_first, 3 + 5);

        mt.put(b"key".to_vec(), b"v".to_vec());
        assert_eq!(mt.size_bytes(), 3 + 1);
    }
}
