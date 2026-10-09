//! A horton [`BlockDevice`] over one file.
//!
//! Block `i` is bytes `i * BLOCK .. (i + 1) * BLOCK` of the file. Reads and
//! writes use positioned I/O. A new file gets its full size at once; it is
//! sparse, so unused blocks take no disk. `poll_flush` is `fdatasync`.
//! The device holds an exclusive OS lock on the file while it is open.
//! Every call completes before it returns.

use core::cell::Cell;
use core::task::{Context, Poll};
use std::fs::{File, OpenOptions, TryLockError};
use std::io;
use std::path::Path;

use horton::BlockDevice;

use crate::error::HortonError;
use crate::shape::BLOCK;

/// A device failure. horton's error is `Copy`, so this keeps the kind only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IoError {
    /// The operation: `"read"`, `"write"` or `"sync"`.
    pub op: &'static str,
    /// The block (0 for a sync).
    pub block: u64,
    /// The operating system's reason.
    pub kind: io::ErrorKind,
}

impl core::fmt::Display for IoError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "{} of block {} failed: {}",
            self.op, self.block, self.kind
        )
    }
}

/// I/O counters.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct IoStats {
    /// Block reads.
    pub reads: u64,
    /// Block writes.
    pub writes: u64,
    /// `fdatasync` calls.
    pub syncs: u64,
}

/// One locked file, in blocks of 16 KiB.
#[derive(Debug)]
pub struct FileDevice {
    file: File,
    blocks: u64,
    sync: bool,
    reads: Cell<u64>,
    writes: u64,
    syncs: u64,
}

impl FileDevice {
    /// Opens the file at `path` and locks it. A new file gets
    /// `create_blocks` blocks. An existing file keeps its size, because
    /// horton's layout depends on it.
    ///
    /// # Errors
    ///
    /// [`HortonError::Locked`] when another store holds the file, and
    /// [`HortonError::Open`] when the file cannot be opened, sized or is
    /// not a whole number of blocks.
    pub fn open(path: &Path, create_blocks: u64, sync: bool) -> Result<Self, HortonError> {
        let open_err = |reason: String| HortonError::Open {
            path: path.to_path_buf(),
            reason,
        };
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .map_err(|e| open_err(e.to_string()))?;
        match file.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                return Err(HortonError::Locked {
                    path: path.to_path_buf(),
                });
            }
            Err(TryLockError::Error(e)) => return Err(open_err(e.to_string())),
        }
        let block = BLOCK as u64;
        let mut len = file.metadata().map_err(|e| open_err(e.to_string()))?.len();
        if len == 0 {
            len = create_blocks
                .checked_mul(block)
                .ok_or_else(|| open_err("the size overflows u64".into()))?;
            file.set_len(len).map_err(|e| open_err(e.to_string()))?;
            file.sync_all().map_err(|e| open_err(e.to_string()))?;
        }
        if len % block != 0 {
            return Err(open_err(format!(
                "the file is {len} bytes, not a whole number of {BLOCK}-byte blocks"
            )));
        }
        Ok(Self {
            file,
            blocks: len / block,
            sync,
            reads: Cell::new(0),
            writes: 0,
            syncs: 0,
        })
    }

    /// The size in blocks.
    #[must_use]
    pub const fn blocks(&self) -> u64 {
        self.blocks
    }

    /// The I/O counters.
    #[must_use]
    pub const fn stats(&self) -> IoStats {
        IoStats {
            reads: self.reads.get(),
            writes: self.writes,
            syncs: self.syncs,
        }
    }

    const fn offset(&self, op: &'static str, id: u64) -> Result<u64, IoError> {
        if id < self.blocks {
            Ok(id * BLOCK as u64)
        } else {
            Err(IoError {
                op,
                block: id,
                kind: io::ErrorKind::InvalidInput,
            })
        }
    }
}

#[cfg(unix)]
fn read_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<()> {
    std::os::unix::fs::FileExt::read_exact_at(file, buf, offset)
}

#[cfg(unix)]
fn write_at(file: &File, buf: &[u8], offset: u64) -> io::Result<()> {
    std::os::unix::fs::FileExt::write_all_at(file, buf, offset)
}

#[cfg(windows)]
fn read_at(file: &File, mut buf: &mut [u8], mut offset: u64) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        match file.seek_read(buf, offset) {
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => {
                buf = &mut buf[n..];
                offset += n as u64;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

#[cfg(windows)]
fn write_at(file: &File, mut buf: &[u8], mut offset: u64) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        match file.seek_write(buf, offset) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(n) => {
                buf = &buf[n..];
                offset += n as u64;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

impl BlockDevice for FileDevice {
    type Error = IoError;
    const BLOCK: usize = BLOCK;

    fn poll_read_block(
        &self,
        _cx: &mut Context<'_>,
        id: u64,
        buf: &mut [u8],
    ) -> Poll<Result<(), IoError>> {
        self.reads.set(self.reads.get().saturating_add(1));
        Poll::Ready(self.offset("read", id).and_then(|at| {
            read_at(&self.file, buf, at).map_err(|e| IoError {
                op: "read",
                block: id,
                kind: e.kind(),
            })
        }))
    }

    fn poll_write_block(
        &mut self,
        _cx: &mut Context<'_>,
        id: u64,
        buf: &[u8],
    ) -> Poll<Result<(), IoError>> {
        self.writes = self.writes.saturating_add(1);
        Poll::Ready(self.offset("write", id).and_then(|at| {
            write_at(&self.file, buf, at).map_err(|e| IoError {
                op: "write",
                block: id,
                kind: e.kind(),
            })
        }))
    }

    fn poll_flush(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), IoError>> {
        if !self.sync {
            return Poll::Ready(Ok(()));
        }
        self.syncs = self.syncs.saturating_add(1);
        Poll::Ready(self.file.sync_data().map_err(|e| IoError {
            op: "sync",
            block: 0,
            kind: e.kind(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use core::task::{Context, Poll, Waker};

    use super::*;

    fn dev(dir: &tempfile::TempDir, sync: bool) -> FileDevice {
        FileDevice::open(&dir.path().join("d.horton"), 4, sync).expect("open")
    }

    #[test]
    fn blocks_round_trip_and_count() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut d = dev(&dir, true);
        let mut cx = Context::from_waker(Waker::noop());
        assert_eq!(d.blocks(), 4);
        let out = vec![7u8; BLOCK];
        assert_eq!(d.poll_write_block(&mut cx, 3, &out), Poll::Ready(Ok(())));
        assert_eq!(d.poll_flush(&mut cx), Poll::Ready(Ok(())));
        let mut back = vec![0u8; BLOCK];
        assert_eq!(
            d.poll_read_block(&mut cx, 3, &mut back),
            Poll::Ready(Ok(()))
        );
        assert_eq!(back, out);
        assert_eq!(
            d.stats(),
            IoStats {
                reads: 1,
                writes: 1,
                syncs: 1
            }
        );
    }

    #[test]
    fn no_sync_skips_fdatasync() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut d = dev(&dir, false);
        let mut cx = Context::from_waker(Waker::noop());
        assert_eq!(d.poll_flush(&mut cx), Poll::Ready(Ok(())));
        assert_eq!(d.stats().syncs, 0);
    }

    #[test]
    fn a_block_past_the_end_is_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut d = dev(&dir, false);
        let mut cx = Context::from_waker(Waker::noop());
        let mut buf = vec![0u8; BLOCK];
        let Poll::Ready(Err(e)) = d.poll_read_block(&mut cx, 4, &mut buf) else {
            panic!("read past the end succeeded");
        };
        assert_eq!(e.kind, io::ErrorKind::InvalidInput);
        assert_eq!(e.to_string(), format!("read of block 4 failed: {}", e.kind));
        assert!(matches!(
            d.poll_write_block(&mut cx, 9, &buf),
            Poll::Ready(Err(IoError {
                op: "write",
                block: 9,
                ..
            }))
        ));
    }

    #[test]
    fn a_second_open_is_locked_and_a_size_overflow_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let _d = dev(&dir, false);
        assert!(matches!(
            FileDevice::open(&dir.path().join("d.horton"), 4, false),
            Err(HortonError::Locked { .. })
        ));
        assert!(matches!(
            FileDevice::open(&dir.path().join("huge.horton"), u64::MAX, false),
            Err(HortonError::Open { .. })
        ));
        assert!(matches!(
            FileDevice::open(dir.path(), 4, false),
            Err(HortonError::Open { .. })
        ));
    }
}
