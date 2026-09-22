//! End-to-end tests exercising `Db` the way a real caller would: through the
//! public API only, against a temp directory standing in for a real data dir.

use toydb::{Db, DbOptions};

#[test]
fn put_get_delete_overwrite_semantics() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = Db::open(dir.path()).unwrap();

    assert_eq!(db.get(b"missing").unwrap(), None);

    db.put(b"name".to_vec(), b"alice".to_vec()).unwrap();
    assert_eq!(db.get(b"name").unwrap(), Some(b"alice".to_vec()));

    // overwrite
    db.put(b"name".to_vec(), b"alice2".to_vec()).unwrap();
    assert_eq!(db.get(b"name").unwrap(), Some(b"alice2".to_vec()));

    // delete
    db.delete(b"name".to_vec()).unwrap();
    assert_eq!(db.get(b"name").unwrap(), None);

    // delete of a never-written key is a legal no-op
    db.delete(b"never-existed".to_vec()).unwrap();
    assert_eq!(db.get(b"never-existed").unwrap(), None);
}

#[test]
fn wal_replay_survives_drop_and_reopen() {
    let dir = tempfile::tempdir().unwrap();

    {
        let mut db = Db::open(dir.path()).unwrap();
        for i in 0..50u32 {
            db.put(format!("key{i}").into_bytes(), format!("val{i}").into_bytes())
                .unwrap();
        }
        db.delete(b"key0".to_vec()).unwrap();
        db.put(b"key1".to_vec(), b"overwritten".to_vec()).unwrap();
        // Db is dropped here (no explicit flush/close) -- simulates a crash
        // immediately after the last WAL write was fsynced.
    }

    let db = Db::open(dir.path()).unwrap();
    assert_eq!(db.get(b"key0").unwrap(), None);
    assert_eq!(db.get(b"key1").unwrap(), Some(b"overwritten".to_vec()));
    for i in 2..50u32 {
        assert_eq!(
            db.get(format!("key{i}").as_bytes()).unwrap(),
            Some(format!("val{i}").into_bytes())
        );
    }
}

#[test]
fn flush_to_sstable_then_read_after_flush() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = Db::open(dir.path()).unwrap();

    for i in 0..20u32 {
        db.put(format!("k{i}").into_bytes(), format!("v{i}").into_bytes())
            .unwrap();
    }
    db.flush().unwrap();
    assert_eq!(db.sstable_count(), 1);

    for i in 0..20u32 {
        assert_eq!(
            db.get(format!("k{i}").as_bytes()).unwrap(),
            Some(format!("v{i}").into_bytes())
        );
    }

    // Data flushed to an sstable must also survive a full close/reopen.
    drop(db);
    let db2 = Db::open(dir.path()).unwrap();
    assert_eq!(db2.get(b"k5").unwrap(), Some(b"v5".to_vec()));
}

#[test]
fn scan_returns_only_live_keys_with_prefix_across_memtable_and_sstables() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = Db::open(dir.path()).unwrap();

    db.put(b"fruit:apple".to_vec(), b"red".to_vec()).unwrap();
    db.put(b"fruit:banana".to_vec(), b"yellow".to_vec()).unwrap();
    db.put(b"veg:carrot".to_vec(), b"orange".to_vec()).unwrap();
    db.flush().unwrap();

    db.put(b"fruit:cherry".to_vec(), b"dark red".to_vec()).unwrap();
    db.delete(b"fruit:apple".to_vec()).unwrap();

    let mut results: Vec<_> = db.scan(b"fruit:").unwrap().collect();
    results.sort();
    assert_eq!(
        results,
        vec![
            (b"fruit:banana".to_vec(), b"yellow".to_vec()),
            (b"fruit:cherry".to_vec(), b"dark red".to_vec()),
        ]
    );

    let empty: Vec<_> = db.scan(b"nothing:").unwrap().collect();
    assert!(empty.is_empty());
}

#[test]
fn compaction_drops_tombstones_and_old_versions_permanently() {
    let dir = tempfile::tempdir().unwrap();
    let options = DbOptions {
        memtable_flush_bytes: 1,
        compaction_threshold: 100, // disable auto-compaction; we call it manually
    };
    let mut db = Db::open_with_options(dir.path(), options).unwrap();

    db.put(b"a".to_vec(), b"v1".to_vec()).unwrap(); // sstable 1
    db.put(b"a".to_vec(), b"v2".to_vec()).unwrap(); // sstable 2, supersedes v1
    db.put(b"b".to_vec(), b"keep".to_vec()).unwrap(); // sstable 3
    db.delete(b"b".to_vec()).unwrap(); // sstable 4, tombstone
    db.put(b"c".to_vec(), b"v3".to_vec()).unwrap(); // sstable 5

    assert!(db.sstable_count() >= 2, "expected multiple sstables before compaction");

    db.compact().unwrap();
    assert_eq!(db.sstable_count(), 1);

    assert_eq!(db.get(b"a").unwrap(), Some(b"v2".to_vec()));
    assert_eq!(db.get(b"b").unwrap(), None, "deleted key must not resurface");
    assert_eq!(db.get(b"c").unwrap(), Some(b"v3".to_vec()));

    // Reopen to make sure the compacted, on-disk state is what actually persists.
    drop(db);
    let db2 = Db::open(dir.path()).unwrap();
    assert_eq!(db2.get(b"a").unwrap(), Some(b"v2".to_vec()));
    assert_eq!(db2.get(b"b").unwrap(), None);
    assert_eq!(db2.get(b"c").unwrap(), Some(b"v3".to_vec()));
}

#[test]
fn many_keys_stress_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let options = DbOptions {
        memtable_flush_bytes: 512, // force several flushes/compactions
        compaction_threshold: 3,
    };
    let mut db = Db::open_with_options(dir.path(), options).unwrap();

    for i in 0..500u32 {
        db.put(format!("item-{i:04}").into_bytes(), format!("value-{i}").into_bytes())
            .unwrap();
    }
    for i in (0..500u32).step_by(3) {
        db.delete(format!("item-{i:04}").into_bytes()).unwrap();
    }

    for i in 0..500u32 {
        let key = format!("item-{i:04}").into_bytes();
        let expected = if i % 3 == 0 {
            None
        } else {
            Some(format!("value-{i}").into_bytes())
        };
        assert_eq!(db.get(&key).unwrap(), expected, "mismatch at i={i}");
    }

    // Survives reopen too.
    drop(db);
    let db2 = Db::open(dir.path()).unwrap();
    for i in 0..500u32 {
        let key = format!("item-{i:04}").into_bytes();
        let expected = if i % 3 == 0 {
            None
        } else {
            Some(format!("value-{i}").into_bytes())
        };
        assert_eq!(db2.get(&key).unwrap(), expected, "mismatch after reopen at i={i}");
    }
}
