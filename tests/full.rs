//! A full store refuses new data but never wedges: reads, deletes, the
//! session sweep, close and reopen keep working.
//!
//! horton 0.18 gives deleted space back only when a compaction job runs
//! (madmax983/horton#32). These tests check that writes work again after
//! deletes, not how much space comes back.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    reason = "tests"
)]

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use autumn_plugin_horton::{Batch, Horton, HortonConfig, HortonError, HortonSessionStore};
use autumn_web::session::SessionStore;

fn config(dir: &tempfile::TempDir) -> HortonConfig {
    HortonConfig::default()
        .with_path(dir.path().join("full.horton"))
        .with_size_mib(HortonConfig::min_size_mib())
        .with_sync(false)
}

/// Writes batches of three 4000-byte values until the store refuses one.
/// Returns the values written.
async fn fill(db: &Horton, tag: u8) -> usize {
    let val = vec![tag; 4000];
    for i in 0..100_000u32 {
        let mut batch = Batch::new();
        for j in 0..3 {
            batch.put(format!("{}{i:08}{j}", tag as char).as_bytes(), &val);
        }
        match db.write(batch).await {
            Ok(()) => {}
            Err(HortonError::StoreFull { .. }) => return 3 * i as usize,
            Err(e) => panic!("batch {i}: {e}"),
        }
    }
    panic!("the store never filled");
}

#[tokio::test]
async fn a_full_store_refuses_data_but_never_wedges() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = Horton::open(&config(&dir)).expect("open");
    let first = fill(&db, b'a').await;
    assert!(first > 1000, "only {first} values fit");
    // At the quota: reads, deletes and maintenance work; data is refused.
    assert_eq!(
        db.get(b"a000000010").await.expect("get").map(|v| v.len()),
        Some(4000)
    );
    assert!(matches!(
        db.put(b"x", b"y").await,
        Err(HortonError::StoreFull { .. })
    ));
    db.delete(b"a000000010").await.expect("delete");
    db.delete_range(b"a", b"b").await.expect("delete_range");
    db.compact().await.expect("compact");
    db.flush().await.expect("flush");
    db.close().await.expect("close");
    // The store reopens, and writes work again.
    let db = Horton::open(&config(&dir)).expect("reopen");
    assert_eq!(db.get(b"a000000010").await, Ok(None));
    assert!(fill(&db, b'c').await > 0);
    assert!(db.stats().await.expect("stats").slots_free >= autumn_plugin_horton::RESERVE_SLOTS);
    db.close().await.expect("close");
}

#[tokio::test]
async fn expired_sessions_are_swept_from_a_full_store() {
    let dir = tempfile::tempdir().expect("tempdir");
    let now = Arc::new(AtomicU64::new(1_000));
    let read = Arc::clone(&now);
    let db = Horton::open_with_clock(&config(&dir), Arc::new(move || read.load(Ordering::SeqCst)))
        .expect("open");
    let store = HortonSessionStore::new(db.clone(), Duration::from_secs(60));
    let blob: HashMap<String, String> = [("blob".to_owned(), "x".repeat(3900))].into();
    let mut saved = 0u32;
    loop {
        match store.save(&format!("s{saved:08}"), blob.clone()).await {
            Ok(()) => saved += 1,
            Err(e) => {
                assert!(e.to_string().contains("full"), "{e}");
                break;
            }
        }
        assert!(saved < 100_000, "the store never filled");
    }
    assert!(saved > 1000, "only {saved} sessions fit");
    // A sweep before expiry deletes nothing.
    assert_eq!(store.sweep().await, Ok(0));
    // Every session expires; the sweep deletes each one once.
    now.store(1_000 + 62, Ordering::SeqCst);
    assert_eq!(store.load("s00000001").await, Ok(None));
    assert_eq!(store.sweep().await, Ok(u64::from(saved)));
    assert_eq!(store.sweep().await, Ok(0), "the index is empty");
    store.destroy("s00000002").await.expect("destroy");
    db.compact().await.expect("compact");
    db.close().await.expect("close");
}
