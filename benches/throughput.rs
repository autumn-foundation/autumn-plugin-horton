//! Read and write throughput of a real store file.
//!
//! Each benchmark runs `tasks` concurrent callers on a multi-thread Tokio
//! runtime. One iteration is one call, so Criterion reports calls per
//! second. Run it with `cargo bench --bench throughput`.

#![allow(
    missing_docs,
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::cast_possible_truncation,
    clippy::significant_drop_tightening,
    reason = "benchmark"
)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use autumn_plugin_horton::{Batch, Horton, HortonConfig};
use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use tokio::runtime::Runtime;

/// Keys the read benchmarks load, and the key space the write ones use.
const KEYS: u64 = 20_000;

/// Bytes per value.
const VAL: usize = 100;

/// Concurrent callers.
const TASKS: [u64; 3] = [1, 16, 128];

fn runtime() -> Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .expect("runtime")
}

fn key(i: u64) -> [u8; 12] {
    let mut k = *b"key-00000000";
    k[4..].copy_from_slice(format!("{:08}", i % KEYS).as_bytes());
    k
}

/// A cheap, deterministic spread over the key space.
const fn spread(i: u64) -> u64 {
    i.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 11
}

/// The read cache of a benchmark store.
#[derive(Clone, Copy)]
enum Cache {
    /// No read cache: horton answers each get.
    Off,
    /// The default size: it holds every key of the benchmark.
    On,
    /// 1 MiB: about 4,300 of the 20,000 keys. Most gets miss and evict.
    Small,
}

/// Opens a store with the read cache `cache`.
fn open(dir: &tempfile::TempDir, sync: bool, cache: Cache) -> Horton {
    let config = HortonConfig::default()
        .with_path(dir.path().join("bench.horton"))
        .with_size_mib(256)
        .with_sync(sync);
    let config = match cache {
        Cache::Off => config.with_cache_mib(0),
        Cache::On => config,
        Cache::Small => config.with_cache_mib(1),
    };
    Horton::open(&config).expect("open")
}

/// Reads every key in two clock seconds. A key that a get reads in two
/// seconds is hot: the read cache keeps it until a write.
fn warm(rt: &Runtime, db: &Horton) {
    rt.block_on(async {
        for pass in 0..2 {
            if pass > 0 {
                tokio::time::sleep(Duration::from_millis(1_100)).await;
            }
            for i in 0..KEYS {
                db.get(&key(i)).await.expect("warm");
            }
        }
    });
}

/// Loads `KEYS` keys. `compact` moves them from the memtable to tables.
fn load(rt: &Runtime, db: &Horton, compact: bool) {
    rt.block_on(async {
        let val = [7u8; VAL];
        for chunk in (0..KEYS).collect::<Vec<_>>().chunks(64) {
            let mut b = Batch::new();
            for &i in chunk {
                b.put(&key(i), &val);
            }
            db.write(b).await.expect("load");
        }
        if compact {
            db.compact().await.expect("compact");
        }
    });
}

/// Runs `iters` calls of `op` over `tasks` callers and times them.
fn drive<F, Fut>(rt: &Runtime, db: &Horton, tasks: u64, iters: u64, op: F) -> Duration
where
    F: Fn(Horton, u64) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send,
{
    let op = Arc::new(op);
    rt.block_on(async {
        let start = Instant::now();
        let mut joins = Vec::new();
        for t in 0..tasks {
            let (db, op) = (db.clone(), Arc::clone(&op));
            let n = iters / tasks + u64::from(t < iters % tasks);
            joins.push(tokio::spawn(async move {
                for j in 0..n {
                    op(db.clone(), spread(t * 1_000_003 + j)).await;
                }
            }));
        }
        for j in joins {
            j.await.expect("task");
        }
        start.elapsed()
    })
}

fn writes(c: &mut Criterion) {
    let rt = runtime();
    for sync in [false, true] {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = open(&dir, sync, Cache::On);
        let mut g = c.benchmark_group(if sync { "put_sync" } else { "put" });
        g.throughput(Throughput::Elements(1));
        for tasks in TASKS {
            g.bench_with_input(BenchmarkId::from_parameter(tasks), &tasks, |b, &tasks| {
                b.iter_custom(|iters| {
                    drive(&rt, &db, tasks, iters, |db, i| async move {
                        db.put(&key(i), &[1u8; VAL]).await.expect("put");
                    })
                });
            });
        }
        g.finish();
        rt.block_on(db.close()).expect("close");
    }
}

fn reads(c: &mut Criterion) {
    let rt = runtime();
    // `get_memtable` and `get_table` turn the read cache off: they measure
    // horton's read path. `get_cached` reads the same table keys through a
    // warm read cache (every key hot). `get_churn` reads them through a
    // read cache that holds a fifth of them: the cost of misses.
    for (name, compact, cache) in [
        ("get_memtable", false, Cache::Off),
        ("get_table", true, Cache::Off),
        ("get_cached", true, Cache::On),
        ("get_churn", true, Cache::Small),
    ] {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = open(&dir, false, cache);
        // The memtable holds 4096 entries: keep the memtable case inside it.
        if compact {
            load(&rt, &db, true);
            if matches!(cache, Cache::On) {
                warm(&rt, &db);
            }
        } else {
            rt.block_on(async {
                for i in 0..2_000 {
                    db.put(&key(i), &[7u8; VAL]).await.expect("put");
                }
            });
        }
        let space = if compact { KEYS } else { 2_000 };
        let mut g = c.benchmark_group(name);
        g.throughput(Throughput::Elements(1));
        for tasks in TASKS {
            g.bench_with_input(BenchmarkId::from_parameter(tasks), &tasks, |b, &tasks| {
                b.iter_custom(|iters| {
                    drive(&rt, &db, tasks, iters, move |db, i| async move {
                        let v = db.get(&key(i % space)).await.expect("get");
                        assert_eq!(v.map(|v| v.len()), Some(VAL));
                    })
                });
            });
        }
        g.finish();
        rt.block_on(db.close()).expect("close");
    }
}

fn scans(c: &mut Criterion) {
    let rt = runtime();
    let dir = tempfile::tempdir().expect("tempdir");
    let db = open(&dir, false, Cache::On);
    load(&rt, &db, true);
    let mut g = c.benchmark_group("scan_100");
    g.throughput(Throughput::Elements(1));
    for tasks in [1, 16] {
        g.bench_with_input(BenchmarkId::from_parameter(tasks), &tasks, |b, &tasks| {
            b.iter_custom(|iters| {
                drive(&rt, &db, tasks, iters, |db, i| async move {
                    let rows = db.scan(&key(i), None, 100).await.expect("scan");
                    assert_ne!(rows.len(), 0);
                })
            });
        });
    }
    g.finish();
    rt.block_on(db.close()).expect("close");
}

criterion_group! {
    name = benches;
    config = Criterion::default()
        .sample_size(20)
        .warm_up_time(Duration::from_millis(500))
        .measurement_time(Duration::from_secs(3));
    targets = writes, reads, scans
}
criterion_main!(benches);
