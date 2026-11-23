//! The read cache never changes what a get returns.
//!
//! A property test runs the same random calls on a store with the read
//! cache, a store without it, and a model. Every get must agree.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    reason = "tests"
)]

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use autumn_plugin_horton::ttl::{deadline, ttl_secs};
use autumn_plugin_horton::{Batch, Clock, Horton, HortonConfig};
use proptest::prelude::*;

#[derive(Debug, Clone)]
enum Call {
    Put(u8, u8),
    PutTtl(u8, u8, u64),
    Delete(u8),
    DeleteRange(u8, u8),
    Batch(Vec<(u8, Option<u8>)>),
    Get(u8),
    Tick(u64),
    Flush,
    Compact,
}

/// A small key space, so gets, writes and clock steps meet on one key.
const KEYS: u8 = 8;

fn call() -> impl Strategy<Value = Call> {
    prop_oneof![
        3 => (0u8..KEYS, any::<u8>()).prop_map(|(k, v)| Call::Put(k, v)),
        2 => (0u8..KEYS, any::<u8>(), 0u64..4).prop_map(|(k, v, t)| Call::PutTtl(k, v, t)),
        1 => (0u8..KEYS).prop_map(Call::Delete),
        1 => (0u8..KEYS, 0u8..KEYS).prop_map(|(a, b)| Call::DeleteRange(a, b)),
        1 => proptest::collection::vec((0u8..KEYS, proptest::option::of(any::<u8>())), 1..6)
            .prop_map(Call::Batch),
        6 => (0u8..KEYS).prop_map(Call::Get),
        2 => (0u64..5).prop_map(Call::Tick),
        1 => Just(Call::Flush),
        1 => Just(Call::Compact),
    ]
}

fn key(k: u8) -> Vec<u8> {
    format!("key-{k:02}").into_bytes()
}

fn val(k: u8, v: u8) -> Vec<u8> {
    vec![v; 1 + usize::from(k) * 37]
}

/// The model: key to (value, expiry tick; 0 means none).
type Model = BTreeMap<Vec<u8>, (Vec<u8>, u64)>;

fn model_get(model: &Model, key: &[u8], now: u64) -> Option<Vec<u8>> {
    model
        .get(key)
        .filter(|(_, e)| *e == 0 || now < *e)
        .map(|(v, _)| v.clone())
}

fn open(dir: &tempfile::TempDir, name: &str, cache_mib: u64, clock: Clock) -> Horton {
    let config = HortonConfig::default()
        .with_path(dir.path().join(name))
        .with_size_mib(HortonConfig::min_size_mib())
        .with_sync(false)
        .with_cache_mib(cache_mib);
    Horton::open_with_clock(&config, clock).expect("open")
}

async fn both<T: PartialEq + std::fmt::Debug, F, Fut>(stores: &[Horton; 2], f: F) -> T
where
    F: Fn(Horton) -> Fut,
    Fut: Future<Output = T>,
{
    let a = f(stores[0].clone()).await;
    let b = f(stores[1].clone()).await;
    assert_eq!(a, b, "the cached and the uncached store differ");
    a
}

/// The batch for `ops`; the model takes the same ops.
fn batch_of(ops: &[(u8, Option<u8>)], model: &mut Model) -> Batch {
    let mut batch = Batch::new();
    for &(k, v) in ops {
        if let Some(v) = v {
            batch.put(&key(k), &val(k, v));
            model.insert(key(k), (val(k, v), 0));
        } else {
            batch.delete(&key(k));
            model.remove(&key(k));
        }
    }
    batch
}

async fn run(calls: Vec<Call>) {
    let dir = tempfile::tempdir().expect("tempdir");
    let now = Arc::new(AtomicU64::new(1_000));
    let read = Arc::clone(&now);
    let clock: Clock = Arc::new(move || read.load(Ordering::SeqCst));
    let stores = [
        open(&dir, "cached.horton", 1, Arc::clone(&clock)),
        open(&dir, "plain.horton", 0, clock),
    ];
    let mut model = Model::new();
    for c in calls {
        let t = now.load(Ordering::SeqCst);
        match c {
            Call::Put(k, v) => {
                both(
                    &stores,
                    |db| async move { db.put(&key(k), &val(k, v)).await },
                )
                .await
                .unwrap();
                model.insert(key(k), (val(k, v), 0));
            }
            Call::PutTtl(k, v, secs) => {
                let ttl = Duration::from_secs(secs);
                both(&stores, |db| async move {
                    db.put_with_ttl(&key(k), &val(k, v), ttl).await
                })
                .await
                .unwrap();
                model.insert(key(k), (val(k, v), deadline(t, ttl_secs(ttl))));
            }
            Call::Delete(k) => {
                both(&stores, |db| async move { db.delete(&key(k)).await })
                    .await
                    .unwrap();
                model.remove(&key(k));
            }
            Call::DeleteRange(a, b) => {
                both(&stores, |db| async move {
                    db.delete_range(&key(a), &key(b)).await
                })
                .await
                .unwrap();
                let (a, b) = (key(a), key(b));
                model.retain(|k, _| !(*k >= a && *k < b));
            }
            Call::Batch(ops) => {
                let batch = batch_of(&ops, &mut model);
                both(&stores, |db| {
                    let batch = batch.clone();
                    async move { db.write(batch).await }
                })
                .await
                .unwrap();
            }
            Call::Get(k) => {
                let got = both(&stores, |db| async move { db.get(&key(k)).await })
                    .await
                    .unwrap();
                assert_eq!(got, model_get(&model, &key(k), t), "get of {k} at {t}");
            }
            Call::Tick(d) => {
                now.fetch_add(d, Ordering::SeqCst);
            }
            Call::Flush => both(&stores, |db| async move { db.flush().await })
                .await
                .unwrap(),
            Call::Compact => both(&stores, |db| async move { db.compact().await })
                .await
                .unwrap(),
        }
    }
    // A last pass reads every key.
    let t = now.load(Ordering::SeqCst);
    for k in 0..KEYS {
        let got = both(&stores, |db| async move { db.get(&key(k)).await })
            .await
            .unwrap();
        assert_eq!(got, model_get(&model, &key(k), t), "final get of {k}");
    }
    let stats = stores[0].stats().await.expect("stats");
    assert!(stats.read_cache_bytes <= 1 << 20);
    let plain = stores[1].stats().await.expect("stats");
    assert_eq!(
        plain.read_cache_hits, 0,
        "a zero budget turns the cache off"
    );
    for db in &stores {
        db.close().await.expect("close");
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(96))]

    #[test]
    fn the_cache_never_changes_a_get(calls in proptest::collection::vec(call(), 1..120)) {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(run(calls));
    }
}

/// Repeat gets hit the cache, and the stats and metrics show it.
#[tokio::test]
async fn repeat_gets_hit_the_cache() {
    let dir = tempfile::tempdir().expect("tempdir");
    let now = Arc::new(AtomicU64::new(1_000));
    let read = Arc::clone(&now);
    let db = open(
        &dir,
        "s.horton",
        1,
        Arc::new(move || read.load(Ordering::SeqCst)),
    );
    // A get makes the entry ("absent"); the write then updates it.
    assert_eq!(db.get(b"written").await, Ok(None));
    db.put(b"written", b"w").await.expect("put");
    db.flush().await.expect("flush");
    let before = db.stats().await.expect("stats");
    // A key that no get asked for takes no entry.
    db.put(b"only-written", b"o").await.expect("put");
    assert_eq!(
        db.stats().await.expect("stats").read_cache_entries,
        before.read_cache_entries
    );
    for _ in 0..10 {
        assert_eq!(db.get(b"written").await, Ok(Some(b"w".to_vec())));
    }
    assert_eq!(db.get(b"missing").await, Ok(None));
    assert_eq!(db.get(b"missing").await, Ok(None));
    let after = db.snapshot().expect("snapshot");
    assert_eq!(after.read_cache_hits - before.read_cache_hits, 11);
    assert_eq!(after.read_cache_misses - before.read_cache_misses, 1);
    assert!(after.read_cache_entries >= 2);
    assert!(after.read_cache_bytes > 0);
    db.close().await.expect("close");

    // After a reopen, a value from a get holds for one clock second.
    let read = Arc::clone(&now);
    let db = open(
        &dir,
        "s.horton",
        1,
        Arc::new(move || read.load(Ordering::SeqCst)),
    );
    assert_eq!(db.get(b"written").await, Ok(Some(b"w".to_vec())));
    assert_eq!(db.get(b"written").await, Ok(Some(b"w".to_vec())));
    let s = db.stats().await.expect("stats");
    assert_eq!((s.read_cache_hits, s.read_cache_misses), (1, 1));
    now.fetch_add(1, Ordering::SeqCst);
    assert_eq!(db.get(b"written").await, Ok(Some(b"w".to_vec())));
    let s = db.stats().await.expect("stats");
    assert_eq!((s.read_cache_hits, s.read_cache_misses), (1, 2));
    db.close().await.expect("close");
}

/// A TTL value written through the cache expires on time.
#[tokio::test]
async fn a_cached_ttl_value_expires_on_the_store_clock() {
    let dir = tempfile::tempdir().expect("tempdir");
    let now = Arc::new(AtomicU64::new(1_000));
    let read = Arc::clone(&now);
    let db = open(
        &dir,
        "t.horton",
        1,
        Arc::new(move || read.load(Ordering::SeqCst)),
    );
    // The get makes the entry; the TTL write updates it with its expiry.
    assert_eq!(db.get(b"k").await, Ok(None));
    db.put_with_ttl(b"k", b"v", Duration::from_secs(2))
        .await
        .expect("put");
    assert_eq!(db.get(b"k").await, Ok(Some(b"v".to_vec())));
    // deadline = now + ttl + 1: live through 1_002, gone at 1_003.
    now.store(1_002, Ordering::SeqCst);
    assert_eq!(db.get(b"k").await, Ok(Some(b"v".to_vec())));
    now.store(1_003, Ordering::SeqCst);
    assert_eq!(db.get(b"k").await, Ok(None));
    assert!(db.stats().await.expect("stats").read_cache_hits >= 3);
    db.close().await.expect("close");
}

/// Sessions read through the cache. After the first load, a save updates
/// the entry, so the next load is a hit. The sweep leaves no entry for a
/// dead session behind.
#[tokio::test]
async fn sessions_hit_the_cache_and_the_sweep_leaves_no_dead_entries() {
    use autumn_plugin_horton::HortonSessionStore;
    use autumn_web::session::SessionStore;
    use std::collections::HashMap;

    let dir = tempfile::tempdir().expect("tempdir");
    let now = Arc::new(AtomicU64::new(1_000));
    let read = Arc::clone(&now);
    let db = open(
        &dir,
        "sess.horton",
        1,
        Arc::new(move || read.load(Ordering::SeqCst)),
    );
    let store = HortonSessionStore::new(db.clone(), Duration::from_mins(1));
    let data: HashMap<String, String> = [("user".to_owned(), "ada".to_owned())].into();
    for i in 0..20 {
        // A new session: the first load finds nothing.
        assert_eq!(store.load(&format!("s{i}")).await, Ok(None));
        store
            .save(&format!("s{i}"), data.clone())
            .await
            .expect("save");
    }
    let before = db.stats().await.expect("stats");
    for i in 0..20 {
        assert_eq!(store.load(&format!("s{i}")).await, Ok(Some(data.clone())));
    }
    let after = db.stats().await.expect("stats");
    assert_eq!(after.read_cache_hits - before.read_cache_hits, 20);
    assert_eq!(after.read_cache_misses, before.read_cache_misses);
    // Every session expires and the sweep deletes it.
    now.store(1_000 + 62, Ordering::SeqCst);
    assert_eq!(store.load("s0").await, Ok(None), "expired in the cache too");
    let entries = db.stats().await.expect("stats").read_cache_entries;
    assert_eq!(store.sweep().await, Ok(20));
    let swept = db.stats().await.expect("stats");
    assert!(
        swept.read_cache_entries + 20 <= entries,
        "the sweep forgot the dead sessions: {} of {entries} left",
        swept.read_cache_entries
    );
    for i in 0..20 {
        assert_eq!(store.load(&format!("s{i}")).await, Ok(None));
    }
    db.close().await.expect("close");
}

/// After a reopen, a key that a get reads in two clock seconds lasts until
/// a write. A key with a TTL does not: it goes back to horton once a
/// second, and it still expires on time.
#[tokio::test]
async fn a_hot_key_lasts_and_a_ttl_key_does_not() {
    let dir = tempfile::tempdir().expect("tempdir");
    let now = Arc::new(AtomicU64::new(1_000));
    let clock = || {
        let read = Arc::clone(&now);
        let clock: Clock = Arc::new(move || read.load(Ordering::SeqCst));
        clock
    };
    let db = open(&dir, "hot.horton", 1, clock());
    db.put(b"plain", b"p").await.expect("put");
    db.put_with_ttl(b"ttl", b"t", Duration::from_secs(10))
        .await
        .expect("put");
    db.close().await.expect("close");

    let db = open(&dir, "hot.horton", 1, clock());
    let misses = |s: &autumn_plugin_horton::Stats| s.read_cache_misses;
    for key in [&b"plain"[..], b"ttl"] {
        assert!(db.get(key).await.expect("get").is_some());
    }
    let first = db.stats().await.expect("stats");
    // The next second: both keys are stale and go back to horton once.
    now.store(1_001, Ordering::SeqCst);
    for key in [&b"plain"[..], b"ttl"] {
        assert!(db.get(key).await.expect("get").is_some());
    }
    let second = db.stats().await.expect("stats");
    assert_eq!(misses(&second) - misses(&first), 2);
    // Later: "plain" lasts, "ttl" goes back to horton again.
    now.store(1_005, Ordering::SeqCst);
    assert_eq!(db.get(b"plain").await, Ok(Some(b"p".to_vec())));
    let third = db.stats().await.expect("stats");
    assert_eq!(misses(&third), misses(&second), "the hot key lasts");
    assert_eq!(db.get(b"ttl").await, Ok(Some(b"t".to_vec())));
    let fourth = db.stats().await.expect("stats");
    assert_eq!(misses(&fourth), misses(&third) + 1, "the TTL key does not");
    // deadline = 1_000 + 10 + 1.
    now.store(1_011, Ordering::SeqCst);
    assert_eq!(db.get(b"ttl").await, Ok(None));
    // A write still updates the lasting entry.
    db.put(b"plain", b"q").await.expect("put");
    assert_eq!(db.get(b"plain").await, Ok(Some(b"q".to_vec())));
    db.delete(b"plain").await.expect("delete");
    assert_eq!(db.get(b"plain").await, Ok(None));
    db.close().await.expect("close");
}

/// A caller that has its reply sees the cache counts after its write.
#[tokio::test]
async fn the_snapshot_shows_the_cache_after_a_write() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = open(&dir, "snap.horton", 1, Arc::new(|| 1_000));
    assert_eq!(db.get(b"k").await, Ok(None));
    let before = db.snapshot().expect("snapshot").read_cache_bytes;
    db.put(b"k", &[7u8; 300]).await.expect("put");
    let after = db.snapshot().expect("snapshot").read_cache_bytes;
    assert_eq!(after, before + 300);
    db.close().await.expect("close");
}
