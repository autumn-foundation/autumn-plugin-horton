//! The horton sizes this plugin compiles in.
//!
//! These are the sizes of horton's host key-value store example. CI runs
//! that example's crash tests on Linux, macOS and Windows.

use core::future::Future;
use core::pin::pin;
use core::task::{Context, Poll, Waker};

pub use crate::keys::{KEY_MAX, VAL_MAX};

/// Bytes per device block.
pub const BLOCK: usize = 16 * 1024;

/// LSM levels.
pub const LEVELS: usize = 4;

horton::db_types! {
    block: BLOCK,
    key_max: KEY_MAX,
    val_max: VAL_MAX,
    memtable_entries: 4096,
    memtable_arena: 1 << 20,
    levels: LEVELS,
    tables_per_level: 7,
    bloom_bytes: BLOCK - 4,
    cache_blocks: 64;

    /// The database type.
    pub type Db = ShapeDb;
    /// A forward scan.
    pub type Scan = ShapeScan;
    /// A reverse scan.
    pub type RevScan = ShapeRevScan;
    /// Compaction scratch.
    pub type Compaction = ShapeCompaction;
}

/// Runs a horton future to completion. [`FileDevice`](crate::device::FileDevice)
/// is ready on the first poll, so this loop does not spin.
pub fn block_on<F: Future>(fut: F) -> F::Output {
    let mut fut = pin!(fut);
    let mut cx = Context::from_waker(Waker::noop());
    loop {
        if let Poll::Ready(v) = fut.as_mut().poll(&mut cx) {
            return v;
        }
    }
}
