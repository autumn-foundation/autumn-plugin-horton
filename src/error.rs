//! The plugin's error type.

use std::path::PathBuf;

use autumn_web::AutumnError;
use autumn_web::reexports::http::StatusCode;

use crate::device::IoError;

/// horton's error, with the plugin's device error.
pub type DbError = horton::Error<IoError>;

/// Every failure the plugin can report.
#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum HortonError {
    /// The key is empty.
    #[error("the key is empty")]
    EmptyKey,
    /// The key is longer than the limit.
    #[error("the key is {len} bytes; the limit is {max}")]
    KeyTooLarge {
        /// The key length.
        len: usize,
        /// The limit.
        max: usize,
    },
    /// The value is longer than the limit.
    #[error("the value is {len} bytes; the limit is {max}")]
    ValueTooLarge {
        /// The value length.
        len: usize,
        /// The limit.
        max: usize,
    },
    /// The key starts with byte `0x00`. The plugin keeps these keys.
    #[error("keys that start with byte 0x00 are reserved for the plugin")]
    ReservedKey,
    /// The batch does not fit one WAL block.
    #[error("the batch needs {bytes} bytes; one WAL block holds {max}")]
    BatchTooLarge {
        /// The bytes the batch needs.
        bytes: usize,
        /// The bytes one block holds.
        max: usize,
    },
    /// The store is near full. It refuses new data and keeps the last
    /// table slots for deletes and compaction. Delete data, let TTLs
    /// expire, or use a larger file.
    #[error("the horton store is full: {free} free table slots, {reserve} kept for compaction")]
    StoreFull {
        /// Free table slots.
        free: u32,
        /// Slots the store keeps free.
        reserve: u32,
    },
    /// horton refused the operation, or the device failed.
    #[error("horton: {0}")]
    Store(DbError),
    /// The plugin cannot open the store file.
    #[error("cannot open {path}: {reason}")]
    Open {
        /// The store file.
        path: PathBuf,
        /// Why.
        reason: String,
    },
    /// Another process (or another open in this process) has the store.
    #[error("{path} is in use by another store")]
    Locked {
        /// The store file.
        path: PathBuf,
    },
    /// The configuration is not valid.
    #[error("horton config: {0}")]
    Config(String),
    /// The store is not open yet.
    #[error("the horton store is not open yet")]
    NotStarted,
    /// The store thread has stopped.
    #[error("the horton store is closed")]
    Closed,
}

impl HortonError {
    /// The HTTP status for this error, for a handler that returns it.
    ///
    /// | Error | Status |
    /// |---|---|
    /// | `EmptyKey`, `ReservedKey` | 400 |
    /// | `KeyTooLarge`, `ValueTooLarge`, `BatchTooLarge` | 413 |
    /// | `StoreFull` | 507 |
    /// | `NotStarted`, `Closed`, `Locked` | 503 |
    /// | other | 500 |
    #[must_use]
    pub const fn status(&self) -> StatusCode {
        match self {
            Self::EmptyKey | Self::ReservedKey => StatusCode::BAD_REQUEST,
            Self::KeyTooLarge { .. } | Self::ValueTooLarge { .. } | Self::BatchTooLarge { .. } => {
                StatusCode::PAYLOAD_TOO_LARGE
            }
            Self::StoreFull { .. } => StatusCode::INSUFFICIENT_STORAGE,
            Self::NotStarted | Self::Closed | Self::Locked { .. } => {
                StatusCode::SERVICE_UNAVAILABLE
            }
            Self::Store(_) | Self::Open { .. } | Self::Config(_) => {
                StatusCode::INTERNAL_SERVER_ERROR
            }
        }
    }

    /// An Autumn error with [`status`](Self::status). Use it in a handler:
    /// `db.put(k, v).await.map_err(HortonError::into_autumn)?`. A bare `?`
    /// gives 500 for every error.
    #[must_use]
    pub fn into_autumn(self) -> AutumnError {
        let status = self.status();
        AutumnError::internal_server_error(self).with_status(status)
    }
}

impl From<DbError> for HortonError {
    fn from(e: DbError) -> Self {
        Self::Store(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_map_to_http_status() {
        for (e, code) in [
            (HortonError::EmptyKey, 400),
            (HortonError::ReservedKey, 400),
            (HortonError::KeyTooLarge { len: 2, max: 1 }, 413),
            (HortonError::ValueTooLarge { len: 2, max: 1 }, 413),
            (HortonError::BatchTooLarge { bytes: 2, max: 1 }, 413),
            (
                HortonError::StoreFull {
                    free: 1,
                    reserve: 6,
                },
                507,
            ),
            (HortonError::NotStarted, 503),
            (HortonError::Closed, 503),
            (HortonError::Locked { path: "a".into() }, 503),
            (HortonError::Config("x".into()), 500),
            (HortonError::Store(horton::Error::RegionFull), 500),
        ] {
            assert_eq!(e.status().as_u16(), code, "{e:?}");
            assert_eq!(e.into_autumn().status().as_u16(), code);
        }
    }

    #[test]
    fn horton_errors_convert_and_print() {
        let e: HortonError = horton::Error::<IoError>::WalFull.into();
        assert_eq!(e, HortonError::Store(horton::Error::WalFull));
        assert_eq!(e.to_string(), "horton: WAL is full: flush, then retry");
        assert_eq!(
            HortonError::Locked { path: "a".into() }.to_string(),
            "a is in use by another store"
        );
    }
}
