//! The plugin's error type.

use std::path::PathBuf;

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
    /// horton refused the operation, or the device failed.
    #[error("horton: {0}")]
    Store(DbError),
    /// The store file could not be opened.
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

impl From<DbError> for HortonError {
    fn from(e: DbError) -> Self {
        Self::Store(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
