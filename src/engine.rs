//! The store thread.
//!
//! One thread owns the horton `Db`. Callers send requests on a bounded
//! channel and wait for a oneshot reply, so no Tokio worker does disk I/O.
//!
//! - **Group commit.** The thread takes a write, then every queued write
//!   that fits the same WAL block, and commits them as one batch: one block
//!   write and one `fdatasync`. Each caller's batch stays atomic.
//! - **Room.** horton reports a full memtable, WAL or level 0 as an error
//!   that names the remedy. The thread applies it (flush or compact) and
//!   tries again. Callers do not see these errors.
//! - **Background compaction.** While a job is pending, the thread runs one
//!   bounded step between requests, and runs steps back to back when the
//!   channel is empty.
//! - **Clock.** Each request reads the clock once. The store clock is
//!   `max(last, wall)`, so it never goes back. Compaction purges expired
//!   entries with it.

use horton::wal::WAL_RECORD_OVERHEAD;
use horton::{Error, OpenReport, Progress, WriteBatch};
use tokio::sync::{mpsc, oneshot};

use crate::device::FileDevice;
use crate::error::{DbError, HortonError};
use crate::group;
use crate::handle::{Batch, BatchOp, Clock, Stats};
use crate::shape::{
    BLOCK, KEY_MAX, ShapeCompaction, ShapeDb, ShapeRevScan, ShapeScan, VAL_MAX, block_on,
};
use crate::ttl;

/// Most ops one WAL block can hold: each op needs its overhead and a key.
pub const GROUP_OPS: usize = BLOCK / (WAL_RECORD_OVERHEAD + 1);

/// Extra WAL bytes for a TTL record (the expiry tick).
pub const TTL_BYTES: usize = 8;

/// One WAL block of ops.
type GroupBatch = WriteBatch<KEY_MAX, VAL_MAX, GROUP_OPS>;

/// How many times one operation makes room and tries again. Each try
/// follows a flush or a whole compaction job. More than this means the
/// region is full.
const ROOM_TRIES: usize = 16;

/// Key-value pairs in scan order.
pub type Entries = Vec<(Vec<u8>, Vec<u8>)>;

/// The reply half of a request.
pub type Reply<T> = oneshot::Sender<Result<T, HortonError>>;

/// One request to the store thread.
pub enum Request {
    /// Commit a batch (validated by the caller).
    Write(Batch, Reply<()>),
    /// Delete `[start, end)`.
    DeleteRange(Vec<u8>, Vec<u8>, Reply<()>),
    /// Read one key.
    Get(Vec<u8>, Reply<Option<Vec<u8>>>),
    /// Read up to `limit` entries of `[start, end)`.
    Scan {
        /// The inclusive lower bound.
        start: Vec<u8>,
        /// The exclusive upper bound.
        end: Option<Vec<u8>>,
        /// Descending order.
        reverse: bool,
        /// The most entries to return.
        limit: usize,
        /// The reply.
        reply: Reply<Entries>,
    },
    /// Write the memtable to a table.
    Flush(Reply<()>),
    /// Flush, then compact until nothing is pending.
    Compact(Reply<()>),
    /// Report counters.
    Stats(Reply<Stats>),
    /// Flush, release the file and stop.
    Close(Reply<()>),
}

/// The store thread's state.
pub struct Engine {
    db: Box<ShapeDb<FileDevice>>,
    room: Room,
    batch: Box<GroupBatch>,
    /// A request taken off the channel that did not fit the last group.
    held: Option<Request>,
    /// Background compaction failed when the store had flushed this many
    /// times. Leave it to the write path until a flush changes the tables:
    /// a failing job reads its inputs before it fails.
    compaction_stuck: Option<u64>,
    open: OpenReport,
    groups: u64,
    grouped_writes: u64,
    clock: Clock,
    now: u64,
    /// The last stats, for callers that cannot wait (metrics).
    snapshot: Snapshot,
}

/// The latest [`Stats`], shared with every handle.
pub type Snapshot = std::sync::Arc<std::sync::Mutex<Option<Stats>>>;

impl Engine {
    /// Builds the database on `device` and recovers it.
    ///
    /// # Errors
    ///
    /// [`HortonError::Config`] when the device is too small for the shape,
    /// and [`HortonError::Store`] when recovery fails.
    #[allow(
        clippy::large_stack_frames,
        reason = "the database is built on the stack before it is boxed; the store thread has a 64 MiB stack"
    )]
    pub fn open(device: FileDevice, clock: Clock, snapshot: Snapshot) -> Result<Self, HortonError> {
        let blocks = device.blocks();
        let min = ShapeDb::<FileDevice>::MIN_DEVICE_BLOCKS;
        if blocks < min {
            return Err(HortonError::Config(format!(
                "the store file holds {blocks} blocks; the minimum is {min}"
            )));
        }
        let mut db = Box::new(ShapeDb::new(device, horton::Config::whole_device(blocks)));
        let open = block_on(db.open())?;
        let now = ttl::advance(0, clock());
        let engine = Self {
            db,
            room: Room {
                scratch: Box::new(ShapeCompaction::new()),
                job_open: false,
                flushes: 0,
                jobs: 0,
                steps: 0,
            },
            batch: Box::new(GroupBatch::new()),
            held: None,
            compaction_stuck: None,
            open,
            groups: 0,
            grouped_writes: 0,
            clock,
            now,
            snapshot,
        };
        engine.publish();
        Ok(engine)
    }

    /// Shares the current stats with the handles.
    fn publish(&self) {
        let stats = self.stats();
        *self
            .snapshot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(stats);
    }

    /// Serves requests until `Close` or until every sender is gone.
    pub fn run(mut self, rx: &mut mpsc::Receiver<Request>) {
        loop {
            let req = if let Some(r) = self.held.take() {
                r
            } else if self.background_pending() {
                match rx.try_recv() {
                    Ok(r) => r,
                    Err(mpsc::error::TryRecvError::Empty) => {
                        self.background_step();
                        self.publish();
                        continue;
                    }
                    Err(mpsc::error::TryRecvError::Disconnected) => return,
                }
            } else {
                match rx.blocking_recv() {
                    Some(r) => r,
                    None => return,
                }
            };
            if let Request::Close(reply) = req {
                let result = self.room.flush(&mut self.db, self.now).map_err(Into::into);
                // Drop the database first: this releases the file lock
                // before the caller hears back.
                drop(self);
                let _ = reply.send(result);
                return;
            }
            self.tick();
            self.serve(req, rx);
            if self.background_pending() {
                self.background_step();
            }
            self.publish();
        }
    }

    /// Reads the clock. The store clock never goes back.
    fn tick(&mut self) {
        self.now = ttl::advance(self.now, (self.clock)());
    }

    fn background_pending(&self) -> bool {
        self.db.compaction_pending() && self.compaction_stuck != Some(self.room.flushes)
    }

    fn background_step(&mut self) {
        if self.room.step(&mut self.db, self.now).is_err() {
            self.compaction_stuck = Some(self.room.flushes);
        }
    }

    fn serve(&mut self, req: Request, rx: &mut mpsc::Receiver<Request>) {
        let now = self.now;
        match req {
            Request::Write(batch, reply) => self.write_group(batch, reply, rx),
            Request::DeleteRange(start, end, reply) => {
                let r = self.room.with_room(&mut self.db, now, |db| {
                    block_on(db.delete_range(&start, &end)).map(drop)
                });
                let _ = reply.send(r.map_err(Into::into));
            }
            Request::Get(key, reply) => {
                let mut buf = vec![0u8; VAL_MAX];
                let r = block_on(self.db.get_with_time(&key, &mut buf, now)).map(|n| {
                    n.map(|n| {
                        buf.truncate(n);
                        buf
                    })
                });
                let _ = reply.send(r.map_err(Into::into));
            }
            Request::Scan {
                start,
                end,
                reverse,
                limit,
                reply,
            } => {
                let r = if reverse {
                    self.scan_rev(&start, end.as_deref(), limit)
                } else {
                    self.scan_fwd(&start, end.as_deref(), limit)
                };
                let _ = reply.send(r.map_err(Into::into));
            }
            Request::Flush(reply) => {
                let _ = reply.send(self.room.flush(&mut self.db, now).map_err(Into::into));
            }
            Request::Compact(reply) => {
                let r = self.room.flush(&mut self.db, now).and_then(|()| {
                    while self.db.compaction_pending() {
                        self.room.job(&mut self.db, now)?;
                    }
                    Ok(())
                });
                if r.is_ok() {
                    self.compaction_stuck = None;
                }
                let _ = reply.send(r.map_err(Into::into));
            }
            Request::Stats(reply) => {
                let _ = reply.send(Ok(self.stats()));
            }
            // `run` handles `Close` before it calls `serve`.
            Request::Close(reply) => {
                let _ = reply.send(Ok(()));
            }
        }
    }

    /// Commits `first` together with every queued write that fits the
    /// same WAL block.
    fn write_group(&mut self, first: Batch, reply: Reply<()>, rx: &mut mpsc::Receiver<Request>) {
        let mut bytes = first.wal_bytes();
        let mut ops = first.len();
        let mut group = vec![(first, reply)];
        while let Ok(next) = rx.try_recv() {
            match next {
                Request::Write(b, r)
                    if group::fits(bytes, ops, b.wal_bytes(), b.len(), BLOCK, GROUP_OPS) =>
                {
                    bytes += b.wal_bytes();
                    ops += b.len();
                    group.push((b, r));
                }
                other => {
                    self.held = Some(other);
                    break;
                }
            }
        }
        let now = self.now;
        self.batch.clear();
        let mut result: Result<(), DbError> = Ok(());
        for op in group.iter().flat_map(|(b, _)| b.ops()) {
            let r = match op {
                BatchOp::Put(k, v) => self.batch.put(k, v),
                BatchOp::PutTtl(k, v, secs) => self.batch.put_ttl(k, v, ttl::expire_at(now, *secs)),
                BatchOp::Delete(k) => self.batch.delete(k),
            };
            // The caller checked every op, and the group fits one block,
            // so horton accepts each one.
            if let Err(e) = r {
                result = Err(e.widen());
                break;
            }
        }
        let batch = &*self.batch;
        let result = result.and_then(|()| {
            if batch.is_empty() {
                return Ok(());
            }
            self.room
                .with_room(&mut self.db, now, |db| block_on(db.write(batch)).map(drop))
        });
        self.groups += 1;
        self.grouped_writes += group.len() as u64;
        let result: Result<(), HortonError> = result.map_err(Into::into);
        for (_, reply) in group {
            let _ = reply.send(result.clone());
        }
    }

    fn scan_fwd(&self, start: &[u8], end: Option<&[u8]>, limit: usize) -> Result<Entries, DbError> {
        let mut out = Vec::new();
        if limit == 0 || end.is_some_and(|e| e <= start) {
            return Ok(out);
        }
        let mut scan = Box::new(ShapeScan::new(&self.db));
        block_on(scan.seek_with_time(start, end, u64::MAX, self.now))?;
        let mut key = [0u8; KEY_MAX];
        let mut val = vec![0u8; VAL_MAX];
        while out.len() < limit {
            match block_on(scan.next(&mut key, &mut val))? {
                Some((kl, vl)) => out.push((key[..kl].to_vec(), val[..vl].to_vec())),
                None => break,
            }
        }
        Ok(out)
    }

    /// Descending over `[start, end)`. horton's reverse scan starts at the
    /// greatest key `<= from`, so this skips `end` and stops below `start`.
    fn scan_rev(&self, start: &[u8], end: Option<&[u8]>, limit: usize) -> Result<Entries, DbError> {
        let mut out = Vec::new();
        if limit == 0 || end.is_some_and(|e| e <= start) {
            return Ok(out);
        }
        let mut scan = Box::new(ShapeRevScan::new(&self.db));
        block_on(scan.seek_prev_with_time(end.unwrap_or(&[]), None, u64::MAX, self.now))?;
        let mut key = [0u8; KEY_MAX];
        let mut val = vec![0u8; VAL_MAX];
        while out.len() < limit {
            match block_on(scan.prev(&mut key, &mut val))? {
                Some((kl, _)) if Some(&key[..kl]) == end => {}
                Some((kl, _)) if &key[..kl] < start => break,
                Some((kl, vl)) => out.push((key[..kl].to_vec(), val[..vl].to_vec())),
                None => break,
            }
        }
        Ok(out)
    }

    fn stats(&self) -> Stats {
        let level_tables = (0..crate::shape::LEVELS)
            .map(|l| self.db.level_tables(l).map_or(0, <[_]>::len))
            .collect();
        let slots = self.db.slot_stats();
        let cache = self.db.cache_stats();
        Stats {
            device_blocks: self.db.device().blocks(),
            recovered_records: self.open.recovered_records,
            slots: slots.slots,
            slots_used: slots.used,
            slots_free: slots.free,
            level_tables,
            io: self.db.device().stats(),
            cache_hits: cache.hits,
            cache_misses: cache.misses,
            groups: self.groups,
            grouped_writes: self.grouped_writes,
            flushes: self.room.flushes,
            compaction_jobs: self.room.jobs,
            compaction_steps: self.room.steps,
            compaction_pending: self.db.compaction_pending(),
            clock: self.now,
        }
    }
}

/// Makes room when horton asks for it.
struct Room {
    scratch: Box<ShapeCompaction>,
    /// A compaction job is in progress on `scratch`.
    job_open: bool,
    flushes: u64,
    jobs: u64,
    steps: u64,
}

impl Room {
    /// Runs `op`. When horton says it needs room, makes room and tries again.
    fn with_room(
        &mut self,
        db: &mut ShapeDb<FileDevice>,
        now: u64,
        mut op: impl FnMut(&mut ShapeDb<FileDevice>) -> Result<(), DbError>,
    ) -> Result<(), DbError> {
        for _ in 0..ROOM_TRIES {
            match op(db) {
                Err(Error::TableFull | Error::ArenaFull | Error::WalFull) => self.flush(db, now)?,
                Err(Error::NeedsCompaction) => self.job(db, now)?,
                r => return r,
            }
        }
        op(db)
    }

    /// Flushes the memtable. Compacts first when level 0 is full.
    fn flush(&mut self, db: &mut ShapeDb<FileDevice>, now: u64) -> Result<(), DbError> {
        for _ in 0..ROOM_TRIES {
            match block_on(db.flush()) {
                Err(Error::NeedsCompaction) => self.job(db, now)?,
                r => {
                    self.flushes += u64::from(r.is_ok());
                    return r;
                }
            }
        }
        block_on(db.flush())
    }

    /// Runs one compaction job to its end.
    fn job(&mut self, db: &mut ShapeDb<FileDevice>, now: u64) -> Result<(), DbError> {
        while self.step(db, now)? == Progress::More {}
        Ok(())
    }

    /// One bounded compaction step. A new job purges entries that expired
    /// at or before `now`.
    fn step(&mut self, db: &mut ShapeDb<FileDevice>, now: u64) -> Result<Progress, DbError> {
        if !self.job_open {
            self.scratch.purge_before = now;
        }
        let p = block_on(db.compact_step(&mut self.scratch));
        self.job_open = matches!(p, Ok(Progress::More));
        let p = p?;
        self.steps += 1;
        self.jobs += u64::from(p == Progress::Done);
        Ok(p)
    }
}
