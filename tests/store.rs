//! The `Horton` handle over a real file.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    reason = "tests"
)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use autumn_plugin_horton::{Batch, Clock, Horton, HortonConfig, HortonError, KEY_MAX, VAL_MAX};

/// A store in a fresh temp dir. The dir goes away on drop.
struct Temp {
    dir: tempfile::TempDir,
}

impl Temp {
    fn new() -> Self {
        Self {
            dir: tempfile::tempdir().expect("tempdir"),
        }
    }

    fn config(&self) -> HortonConfig {
        HortonConfig::default()
            .with_path(self.dir.path().join("data").join("store.horton"))
            .with_size_mib(HortonConfig::min_size_mib())
            .with_sync(false)
    }

    fn open(&self) -> Horton {
        Horton::open(&self.config()).expect("open")
    }
}

/// A clock the test moves by hand.
fn manual_clock(start: u64) -> (Arc<AtomicU64>, Clock) {
    let now = Arc::new(AtomicU64::new(start));
    let read = Arc::clone(&now);
    (now, Arc::new(move || read.load(Ordering::SeqCst)))
}

#[tokio::test]
async fn put_get_delete_on_a_current_thread_runtime() {
    let t = Temp::new();
    let db = t.open();
    assert_eq!(db.get(b"k").await, Ok(None));
    db.put(b"k", b"v1").await.expect("put");
    assert_eq!(db.get(b"k").await, Ok(Some(b"v1".to_vec())));
    db.put(b"k", b"v2").await.expect("overwrite");
    assert_eq!(db.get(b"k").await, Ok(Some(b"v2".to_vec())));
    db.delete(b"k").await.expect("delete");
    assert_eq!(db.get(b"k").await, Ok(None));
    db.close().await.expect("close");
}

#[tokio::test]
async fn data_survives_close_and_reopen() {
    let t = Temp::new();
    let db = t.open();
    for i in 0..500u32 {
        db.put(format!("k{i:05}").as_bytes(), &i.to_le_bytes())
            .await
            .expect("put");
    }
    db.flush().await.expect("flush");
    db.put(b"after-flush", b"in the WAL only")
        .await
        .expect("put");
    db.close().await.expect("close");

    let db = t.open();
    assert_eq!(
        db.get(b"k00042").await,
        Ok(Some(42u32.to_le_bytes().to_vec()))
    );
    assert_eq!(
        db.get(b"after-flush").await,
        Ok(Some(b"in the WAL only".to_vec()))
    );
    // `close` flushed the memtable, so the WAL had nothing to replay.
    assert_eq!(db.stats().await.expect("stats").recovered_records, 0);
    db.close().await.expect("close");
}

#[tokio::test]
async fn data_survives_a_drop_without_close() {
    let t = Temp::new();
    {
        let db = t.open();
        db.put(b"k", b"v").await.expect("put");
    }
    // The store thread stops when the last handle goes. Wait for the lock.
    let db = open_when_free(&t.config()).await;
    assert_eq!(db.get(b"k").await, Ok(Some(b"v".to_vec())));
    // No flush ran: the value came back from the WAL.
    assert_eq!(db.stats().await.expect("stats").recovered_records, 1);
}

async fn open_when_free(config: &HortonConfig) -> Horton {
    for _ in 0..200 {
        match Horton::open(config) {
            Ok(db) => return db,
            Err(HortonError::Locked { .. }) => {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Err(e) => panic!("open: {e}"),
        }
    }
    panic!("the store stayed locked");
}

#[tokio::test]
async fn a_second_open_of_one_file_is_refused() {
    let t = Temp::new();
    let db = t.open();
    assert!(matches!(
        Horton::open(&t.config()),
        Err(HortonError::Locked { .. })
    ));
    db.close().await.expect("close");
    let again = t.open();
    again.close().await.expect("close");
}

#[tokio::test]
async fn limits_are_typed_errors() {
    let t = Temp::new();
    let db = t.open();
    assert_eq!(db.put(b"", b"v").await, Err(HortonError::EmptyKey));
    assert_eq!(
        db.put(&[b'k'; KEY_MAX + 1], b"v").await,
        Err(HortonError::KeyTooLarge {
            len: KEY_MAX + 1,
            max: KEY_MAX
        })
    );
    assert_eq!(db.put(&[0, 1], b"v").await, Err(HortonError::ReservedKey));
    assert_eq!(db.get(&[0, 1]).await, Err(HortonError::ReservedKey));
    assert_eq!(db.delete(&[0, 1]).await, Err(HortonError::ReservedKey));
    assert_eq!(
        db.put(b"k", &[0; VAL_MAX + 1]).await,
        Err(HortonError::ValueTooLarge {
            len: VAL_MAX + 1,
            max: VAL_MAX
        })
    );
    let mut big = Batch::new();
    for i in 0..10u8 {
        big.put(&[b'k', i], &[0; VAL_MAX]);
    }
    assert!(matches!(
        db.write(big).await,
        Err(HortonError::BatchTooLarge { .. })
    ));
    assert!(matches!(
        db.delete_range(&[0], b"z").await,
        Err(HortonError::ReservedKey)
    ));
    // The largest key and value fit.
    db.put(&[b'k'; KEY_MAX], &[7; VAL_MAX])
        .await
        .expect("max put");
    assert_eq!(db.get(&[b'k'; KEY_MAX]).await, Ok(Some(vec![7; VAL_MAX])));
}

#[tokio::test]
async fn a_batch_applies_all_its_ops() {
    let t = Temp::new();
    let db = t.open();
    db.put(b"gone", b"x").await.expect("put");
    let mut b = Batch::new();
    b.put(b"a", b"1")
        .put(b"b", b"2")
        .delete(b"gone")
        .put_with_ttl(b"c", b"3", Duration::from_secs(3600));
    assert_eq!(b.len(), 4);
    assert!(!b.is_empty());
    db.write(b).await.expect("write");
    assert_eq!(db.get(b"a").await, Ok(Some(b"1".to_vec())));
    assert_eq!(db.get(b"b").await, Ok(Some(b"2".to_vec())));
    assert_eq!(db.get(b"c").await, Ok(Some(b"3".to_vec())));
    assert_eq!(db.get(b"gone").await, Ok(None));
    db.write(Batch::new())
        .await
        .expect("an empty batch is a no-op");
}

#[tokio::test]
async fn delete_range_removes_the_half_open_range() {
    let t = Temp::new();
    let db = t.open();
    for k in [b"a", b"b", b"c", b"d"] {
        db.put(k, b"v").await.expect("put");
    }
    db.delete_range(b"b", b"d").await.expect("delete_range");
    let keys: Vec<_> = db
        .scan(b"", None, 100)
        .await
        .expect("scan")
        .into_iter()
        .map(|(k, _)| k)
        .collect();
    assert_eq!(keys, vec![b"a".to_vec(), b"d".to_vec()]);
    // An inverted range is a no-op.
    db.delete_range(b"z", b"a").await.expect("inverted");
}

#[tokio::test]
async fn scans_respect_bounds_limits_and_direction() {
    let t = Temp::new();
    let db = t.open();
    for i in 0..50u8 {
        db.put(&[b'k', i], &[i]).await.expect("put");
    }
    let page = db
        .scan(&[b'k', 10], Some(&[b'k', 20]), 5)
        .await
        .expect("scan");
    let got: Vec<u8> = page.iter().map(|(k, _)| k[1]).collect();
    assert_eq!(got, vec![10, 11, 12, 13, 14]);

    let all = db
        .scan(&[b'k', 10], Some(&[b'k', 20]), 100)
        .await
        .expect("scan");
    assert_eq!(all.len(), 10);

    let rev = db
        .scan_rev(&[b'k', 10], Some(&[b'k', 20]), 3)
        .await
        .expect("scan_rev");
    let got: Vec<u8> = rev.iter().map(|(k, _)| k[1]).collect();
    assert_eq!(got, vec![19, 18, 17]);

    let tail = db.scan_rev(b"", None, 2).await.expect("scan_rev all");
    let got: Vec<u8> = tail.iter().map(|(k, _)| k[1]).collect();
    assert_eq!(got, vec![49, 48]);

    assert_eq!(db.scan(b"", None, 0).await, Ok(Vec::new()));
    assert_eq!(db.scan(&[0, 5], None, 1).await.expect("floor").len(), 1);
}

#[tokio::test]
async fn ttl_values_expire_on_the_store_clock() {
    let t = Temp::new();
    let (now, clock) = manual_clock(1_000);
    let db = Horton::open_with_clock(&t.config(), clock).expect("open");
    db.put_with_ttl(b"short", b"v", Duration::from_secs(10))
        .await
        .expect("put");
    db.put_with_ttl(b"sub-second", b"v", Duration::from_millis(1))
        .await
        .expect("put");
    db.put(b"forever", b"v").await.expect("put");
    assert_eq!(db.get(b"short").await, Ok(Some(b"v".to_vec())));
    assert_eq!(db.get(b"sub-second").await, Ok(Some(b"v".to_vec())));

    now.store(1_001, Ordering::SeqCst);
    assert_eq!(db.get(b"sub-second").await, Ok(None));
    now.store(1_010, Ordering::SeqCst);
    assert_eq!(db.get(b"short").await, Ok(None));
    let keys: Vec<_> = db
        .scan(b"", None, 10)
        .await
        .expect("scan")
        .into_iter()
        .map(|(k, _)| k)
        .collect();
    assert_eq!(keys, vec![b"forever".to_vec()]);

    // The clock never goes back: an expired value stays expired.
    now.store(5, Ordering::SeqCst);
    assert_eq!(db.get(b"short").await, Ok(None));
    assert_eq!(db.stats().await.expect("stats").clock, 1_010);

    // Compaction purges it, and the value stays gone after a reopen.
    db.compact().await.expect("compact");
    db.close().await.expect("close");
    let (_now, clock) = manual_clock(2_000);
    let db = Horton::open_with_clock(&t.config(), clock).expect("reopen");
    assert_eq!(db.get(b"short").await, Ok(None));
    assert_eq!(db.get(b"forever").await, Ok(Some(b"v".to_vec())));
}

#[tokio::test]
async fn a_closed_store_refuses_calls() {
    let t = Temp::new();
    let db = t.open();
    let other = db.clone();
    assert!(!db.is_closed());
    db.close().await.expect("close");
    assert!(other.is_closed());
    assert_eq!(other.get(b"k").await, Err(HortonError::Closed));
    assert_eq!(other.put(b"k", b"v").await, Err(HortonError::Closed));
    assert_eq!(other.close().await, Ok(()), "a second close is a no-op");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_writers_are_grouped_and_all_land() {
    let t = Temp::new();
    let db = t.open();
    let mut tasks = Vec::new();
    for w in 0..8u8 {
        let db = db.clone();
        tasks.push(tokio::spawn(async move {
            for i in 0..200u16 {
                let mut key = vec![b'w', w];
                key.extend_from_slice(&i.to_be_bytes());
                db.put(&key, &[w]).await.expect("put");
            }
        }));
    }
    for task in tasks {
        task.await.expect("join");
    }
    let all = db.scan(b"", None, 10_000).await.expect("scan");
    assert_eq!(all.len(), 1600);
    let stats = db.stats().await.expect("stats");
    assert_eq!(stats.grouped_writes, 1600);
    assert!(stats.groups <= 1600);
    db.close().await.expect("close");
}

#[tokio::test]
async fn the_store_makes_room_by_itself() {
    let t = Temp::new();
    let db = t.open();
    // About 20 MiB of overwrites into the smallest store: flushes, WAL
    // wraps and compaction must all happen with no error reaching the
    // caller.
    let val = vec![9u8; 2048];
    for round in 0..4u32 {
        for i in 0..2_500u32 {
            db.put(format!("key{i:06}").as_bytes(), &val)
                .await
                .unwrap_or_else(|e| panic!("round {round} put {i}: {e}"));
        }
    }
    let stats = db.stats().await.expect("stats");
    assert!(stats.flushes > 0, "{stats:?}");
    assert!(stats.compaction_steps > 0, "{stats:?}");
    assert_eq!(db.get(b"key000123").await, Ok(Some(val.clone())));
    db.compact().await.expect("compact");
    assert_eq!(db.get(b"key002499").await, Ok(Some(val)));
    db.close().await.expect("close");
}

#[tokio::test]
async fn an_existing_file_keeps_its_size() {
    let t = Temp::new();
    let db = t.open();
    let blocks = db.stats().await.expect("stats").device_blocks;
    db.close().await.expect("close");
    let bigger = t.config().with_size_mib(HortonConfig::min_size_mib() + 64);
    let db = Horton::open(&bigger).expect("reopen");
    assert_eq!(db.stats().await.expect("stats").device_blocks, blocks);
    db.close().await.expect("close");
}

#[test]
fn a_file_that_is_not_a_store_is_refused() {
    let t = Temp::new();
    let config = t.config();
    std::fs::create_dir_all(config.path.parent().expect("parent")).expect("mkdir");
    std::fs::write(&config.path, b"not a store").expect("write");
    assert!(matches!(
        Horton::open(&config),
        Err(HortonError::Open { .. })
    ));
}

#[test]
fn a_store_below_the_minimum_size_is_refused() {
    let t = Temp::new();
    let config = t.config().with_size_mib(1);
    assert!(matches!(Horton::open(&config), Err(HortonError::Config(_))));
}
