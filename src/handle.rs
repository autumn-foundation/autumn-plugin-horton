//! [`Horton`]: the async handle to an open store.

use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

use horton::wal::WAL_RECORD_OVERHEAD;
use tokio::sync::{mpsc, oneshot};

use crate::config::HortonConfig;
use crate::device::{FileDevice, IoStats};
use crate::engine::{Engine, Entries, Reply, Request, Snapshot, TTL_BYTES};
use crate::error::HortonError;
use crate::keys::{self, check_app_key, check_value};
use crate::shape::BLOCK;
use crate::ttl;

/// A clock in Unix seconds. [`Horton::open`] uses the system clock.
pub type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;

/// Requests that can wait in the queue. A full queue makes callers wait.
const QUEUE: usize = 1024;

/// The store thread's stack. horton builds the database on the stack
/// before the plugin boxes it.
const STACK: usize = 64 << 20;

/// One operation in a [`Batch`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BatchOp {
    /// Put a key.
    Put(Vec<u8>, Vec<u8>),
    /// Put a key that expires after the given seconds.
    PutTtl(Vec<u8>, Vec<u8>, u64),
    /// Delete a key.
    Delete(Vec<u8>),
}

/// Writes that apply together or not at all.
///
/// The batch must fit one WAL block: 16 KiB, with 23 bytes of overhead
/// for each op (31 for a TTL op). [`Horton::write`] refuses a larger
/// batch with [`HortonError::BatchTooLarge`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Batch {
    ops: Vec<BatchOp>,
}

impl Batch {
    /// An empty batch.
    #[must_use]
    pub const fn new() -> Self {
        Self { ops: Vec::new() }
    }

    /// Adds a put.
    pub fn put(&mut self, key: &[u8], val: &[u8]) -> &mut Self {
        self.ops.push(BatchOp::Put(key.to_vec(), val.to_vec()));
        self
    }

    /// Adds a put that expires after `ttl` (rounded up to whole seconds).
    pub fn put_with_ttl(&mut self, key: &[u8], val: &[u8], ttl: Duration) -> &mut Self {
        self.ops.push(BatchOp::PutTtl(
            key.to_vec(),
            val.to_vec(),
            ttl::ttl_secs(ttl),
        ));
        self
    }

    /// Adds a delete.
    pub fn delete(&mut self, key: &[u8]) -> &mut Self {
        self.ops.push(BatchOp::Delete(key.to_vec()));
        self
    }

    /// The operations, in order.
    #[must_use]
    pub fn ops(&self) -> &[BatchOp] {
        &self.ops
    }

    /// The number of operations.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.ops.len()
    }

    /// True when the batch has no operations.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }

    /// Bytes this batch takes in a WAL block.
    pub(crate) fn wal_bytes(&self) -> usize {
        self.ops
            .iter()
            .map(|op| match op {
                BatchOp::Put(k, v) => WAL_RECORD_OVERHEAD + k.len() + v.len(),
                BatchOp::PutTtl(k, v, _) => WAL_RECORD_OVERHEAD + TTL_BYTES + k.len() + v.len(),
                BatchOp::Delete(k) => WAL_RECORD_OVERHEAD + k.len(),
            })
            .sum()
    }

    /// Checks every op and the total size. `space` selects the key rule.
    fn check(&self, space: Space) -> Result<(), HortonError> {
        for op in &self.ops {
            let (key, val) = match op {
                BatchOp::Put(k, v) | BatchOp::PutTtl(k, v, _) => (k.as_slice(), v.as_slice()),
                BatchOp::Delete(k) => (k.as_slice(), &[][..]),
            };
            space.check_key(key)?;
            check_value(val)?;
        }
        let bytes = self.wal_bytes();
        if bytes > BLOCK {
            return Err(HortonError::BatchTooLarge { bytes, max: BLOCK });
        }
        Ok(())
    }
}

/// Which keys a call may touch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Space {
    /// App keys only.
    App,
    /// Plugin keys (sessions). The key builder checks them.
    Plugin,
}

impl Space {
    const fn check_key(self, key: &[u8]) -> Result<(), HortonError> {
        match self {
            Self::App => check_app_key(key),
            Self::Plugin => {
                if key.is_empty() {
                    Err(HortonError::EmptyKey)
                } else if key.len() > keys::KEY_MAX {
                    Err(HortonError::KeyTooLarge {
                        len: key.len(),
                        max: keys::KEY_MAX,
                    })
                } else {
                    Ok(())
                }
            }
        }
    }
}

/// Store counters and state.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Stats {
    /// The store file size, in blocks of 16 KiB.
    pub device_blocks: u64,
    /// WAL records replayed at open.
    pub recovered_records: u64,
    /// Table slots in the file.
    pub slots: u32,
    /// Slots that hold a live table.
    pub slots_used: u32,
    /// Slots free for new tables.
    pub slots_free: u32,
    /// Tables per LSM level.
    pub level_tables: Vec<usize>,
    /// Device I/O counters.
    pub io: IoStats,
    /// Block reads served by the cache.
    pub cache_hits: u64,
    /// Block reads that went to the file.
    pub cache_misses: u64,
    /// Group commits.
    pub groups: u64,
    /// Caller writes in those groups.
    pub grouped_writes: u64,
    /// Memtable flushes.
    pub flushes: u64,
    /// Compaction jobs done.
    pub compaction_jobs: u64,
    /// Compaction steps done.
    pub compaction_steps: u64,
    /// A compaction job waits to run.
    pub compaction_pending: bool,
    /// The store clock, in Unix seconds.
    pub clock: u64,
}

struct Inner {
    tx: mpsc::Sender<Request>,
    thread: Mutex<Option<JoinHandle<()>>>,
    snapshot: Snapshot,
}

/// A cloneable handle to an open store.
///
/// Every call is `async` and does no I/O on the calling thread: a store
/// thread does the work. The handle works on any async runtime. When the
/// last clone drops, the store thread stops. Data is safe then, because
/// every write is in the WAL before its call returns.
#[derive(Clone)]
pub struct Horton {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for Horton {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Horton")
            .field("closed", &self.is_closed())
            .finish()
    }
}

impl Horton {
    /// Opens (or creates) the store that `config` names, with the system
    /// clock. This call blocks while horton recovers the store. In async
    /// code, call it from `spawn_blocking`.
    ///
    /// # Errors
    ///
    /// [`HortonError::Config`], [`HortonError::Open`],
    /// [`HortonError::Locked`] or [`HortonError::Store`].
    pub fn open(config: &HortonConfig) -> Result<Self, HortonError> {
        Self::open_with_clock(config, Arc::new(ttl::wall_secs))
    }

    /// Opens the store with `clock` in place of the system clock. Tests
    /// use it to control TTL expiry.
    ///
    /// # Errors
    ///
    /// The same as [`open`](Self::open).
    pub fn open_with_clock(config: &HortonConfig, clock: Clock) -> Result<Self, HortonError> {
        config.clone().with_enabled(true).validate()?;
        make_parent(&config.path)?;
        let device = FileDevice::open(&config.path, config.create_blocks(), config.sync)?;
        if device.blocks() != config.create_blocks() {
            tracing::warn!(
                path = %config.path.display(),
                file_blocks = device.blocks(),
                config_blocks = config.create_blocks(),
                "the horton store file keeps the size it was created with"
            );
        }
        let (tx, mut rx) = mpsc::channel(QUEUE);
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        let snapshot = Snapshot::default();
        let engine_snapshot = Arc::clone(&snapshot);
        let thread = std::thread::Builder::new()
            .name("horton-store".into())
            .stack_size(STACK)
            .spawn(move || match Engine::open(device, clock, engine_snapshot) {
                Ok(engine) => {
                    let _ = ready_tx.send(Ok(()));
                    engine.run(&mut rx);
                }
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                }
            })
            .map_err(|e| HortonError::Open {
                path: config.path.clone(),
                reason: e.to_string(),
            })?;
        ready_rx.recv().map_err(|_| HortonError::Closed)??;
        Ok(Self {
            inner: Arc::new(Inner {
                tx,
                thread: Mutex::new(Some(thread)),
                snapshot,
            }),
        })
    }

    /// The value of `key`, or `None`. Expired values read as `None`.
    ///
    /// # Errors
    ///
    /// A key error, [`HortonError::Store`] or [`HortonError::Closed`].
    pub async fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, HortonError> {
        check_app_key(key)?;
        self.get_in(key).await
    }

    /// Stores `key → val`. When this returns, the write is durable.
    ///
    /// # Errors
    ///
    /// A key or value error, [`HortonError::Store`] or
    /// [`HortonError::Closed`].
    pub async fn put(&self, key: &[u8], val: &[u8]) -> Result<(), HortonError> {
        let mut b = Batch::new();
        b.put(key, val);
        self.write(b).await
    }

    /// Stores `key → val` until `ttl` passes (rounded up to whole seconds).
    ///
    /// # Errors
    ///
    /// The same as [`put`](Self::put).
    pub async fn put_with_ttl(
        &self,
        key: &[u8],
        val: &[u8],
        ttl: Duration,
    ) -> Result<(), HortonError> {
        let mut b = Batch::new();
        b.put_with_ttl(key, val, ttl);
        self.write(b).await
    }

    /// Deletes `key`. Deleting a missing key is not an error.
    ///
    /// # Errors
    ///
    /// A key error, [`HortonError::Store`] or [`HortonError::Closed`].
    pub async fn delete(&self, key: &[u8]) -> Result<(), HortonError> {
        let mut b = Batch::new();
        b.delete(key);
        self.write(b).await
    }

    /// Applies `batch` atomically. When this returns, it is durable.
    ///
    /// # Errors
    ///
    /// A key, value or [`HortonError::BatchTooLarge`] error,
    /// [`HortonError::Store`] or [`HortonError::Closed`].
    pub async fn write(&self, batch: Batch) -> Result<(), HortonError> {
        self.write_in(batch, Space::App).await
    }

    /// Deletes every key in `[start, end)`. An empty or inverted range does
    /// nothing.
    ///
    /// # Errors
    ///
    /// A key error, [`HortonError::Store`] or [`HortonError::Closed`].
    pub async fn delete_range(&self, start: &[u8], end: &[u8]) -> Result<(), HortonError> {
        check_app_key(start)?;
        check_app_key(end)?;
        if start >= end {
            return Ok(());
        }
        let (start, end) = (start.to_vec(), end.to_vec());
        self.call(|r| Request::DeleteRange(start, end, r)).await
    }

    /// Up to `limit` entries of `[start, end)` in ascending key order. An
    /// empty `start` is the first key. `end = None` is past the last key.
    ///
    /// # Errors
    ///
    /// [`HortonError::KeyTooLarge`] for a long bound,
    /// [`HortonError::Store`] or [`HortonError::Closed`].
    pub async fn scan(
        &self,
        start: &[u8],
        end: Option<&[u8]>,
        limit: usize,
    ) -> Result<Entries, HortonError> {
        self.scan_in(start, end, limit, false).await
    }

    /// Up to `limit` entries of `[start, end)` in descending key order.
    ///
    /// # Errors
    ///
    /// The same as [`scan`](Self::scan).
    pub async fn scan_rev(
        &self,
        start: &[u8],
        end: Option<&[u8]>,
        limit: usize,
    ) -> Result<Entries, HortonError> {
        self.scan_in(start, end, limit, true).await
    }

    /// Writes the memtable to a table now. The store also does this by
    /// itself when the memtable fills.
    ///
    /// # Errors
    ///
    /// [`HortonError::Store`] or [`HortonError::Closed`].
    pub async fn flush(&self) -> Result<(), HortonError> {
        self.call(Request::Flush).await
    }

    /// Flushes, then compacts until no job is pending. Compaction also
    /// purges expired values.
    ///
    /// # Errors
    ///
    /// [`HortonError::Store`] or [`HortonError::Closed`].
    pub async fn compact(&self) -> Result<(), HortonError> {
        self.call(Request::Compact).await
    }

    /// Store counters and state.
    ///
    /// # Errors
    ///
    /// [`HortonError::Closed`].
    pub async fn stats(&self) -> Result<Stats, HortonError> {
        self.call(Request::Stats).await
    }

    /// Flushes and stops the store thread, and releases the file lock.
    /// Every clone then returns [`HortonError::Closed`]. A second close
    /// does nothing.
    ///
    /// # Errors
    ///
    /// [`HortonError::Store`] when the last flush fails. The store stops
    /// all the same; the WAL holds the data.
    pub async fn close(&self) -> Result<(), HortonError> {
        let result = match self.call(Request::Close).await {
            Err(HortonError::Closed) => Ok(()),
            r => r,
        };
        let thread = self
            .inner
            .thread
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(thread) = thread {
            // The thread has already dropped the database; it only returns.
            let _ = thread.join();
        }
        result
    }

    /// The stats after the last request, with no wait. `None` before the
    /// store thread has answered once.
    #[must_use]
    pub fn snapshot(&self) -> Option<Stats> {
        self.inner
            .snapshot
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// True when the store thread has stopped.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.inner.tx.is_closed()
    }

    pub(crate) async fn get_in(&self, key: &[u8]) -> Result<Option<Vec<u8>>, HortonError> {
        let key = key.to_vec();
        self.call(|r| Request::Get(key, r)).await
    }

    pub(crate) async fn write_in(&self, batch: Batch, space: Space) -> Result<(), HortonError> {
        batch.check(space)?;
        if batch.is_empty() {
            return Ok(());
        }
        self.call(|r| Request::Write(batch, r)).await
    }

    async fn scan_in(
        &self,
        start: &[u8],
        end: Option<&[u8]>,
        limit: usize,
        reverse: bool,
    ) -> Result<Entries, HortonError> {
        for bound in std::iter::once(start).chain(end) {
            if bound.len() > keys::KEY_MAX {
                return Err(HortonError::KeyTooLarge {
                    len: bound.len(),
                    max: keys::KEY_MAX,
                });
            }
        }
        let start = keys::app_floor(start).to_vec();
        let end = end.map(<[u8]>::to_vec);
        self.call(|reply| Request::Scan {
            start,
            end,
            reverse,
            limit,
            reply,
        })
        .await
    }

    async fn call<T>(&self, make: impl FnOnce(Reply<T>) -> Request) -> Result<T, HortonError> {
        let (tx, rx) = oneshot::channel();
        self.inner
            .tx
            .send(make(tx))
            .await
            .map_err(|_| HortonError::Closed)?;
        rx.await.map_err(|_| HortonError::Closed)?
    }
}

fn make_parent(path: &Path) -> Result<(), HortonError> {
    match path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => {
            std::fs::create_dir_all(dir).map_err(|e| HortonError::Open {
                path: path.to_path_buf(),
                reason: format!("cannot make {}: {e}", dir.display()),
            })
        }
        _ => Ok(()),
    }
}
