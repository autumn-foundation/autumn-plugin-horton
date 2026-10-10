//! [`HortonSessionStore`]: Autumn sessions in the horton store.
//!
//! A session is one JSON object under the key `0x00 "session/" <hash>`,
//! where `<hash>` is the SHA-256 of the session id. The
//! app API cannot read, write or scan these keys. Each save resets the
//! session's TTL to `session.max_age_secs`. Each save also writes an index
//! key with the expiry. The sweep uses the index to delete expired
//! sessions, and compaction then gives their space back.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use autumn_web::session::{SessionStore, SessionStoreError};

use crate::error::HortonError;
use crate::handle::{Batch, Horton, Space};
use crate::keys::{session_hash, session_key_bytes};
use crate::slot::Slot;
use crate::ttl::ttl_secs;

/// Autumn's default `session.max_age_secs`.
const DEFAULT_MAX_AGE: u64 = 86_400;

/// How often the plugin sweeps expired sessions.
pub const SWEEP_EVERY: Duration = Duration::from_secs(60);

/// A [`SessionStore`] over a [`Horton`] store.
///
/// The JSON form of a session must fit one value (4096 bytes). A larger
/// session fails to save with [`HortonError::ValueTooLarge`].
#[derive(Clone)]
pub struct HortonSessionStore {
    slot: Slot,
    max_age_secs: Arc<AtomicU64>,
}

impl std::fmt::Debug for HortonSessionStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HortonSessionStore")
            .field("open", &self.slot.get().is_some())
            .field("max_age_secs", &self.max_age_secs.load(Ordering::Relaxed))
            .finish()
    }
}

impl HortonSessionStore {
    /// A session store over `horton`. Sessions expire `max_age` after
    /// their last save.
    #[must_use]
    pub fn new(horton: Horton, max_age: Duration) -> Self {
        Self {
            slot: Slot::with(horton),
            max_age_secs: Arc::new(AtomicU64::new(ttl_secs(max_age))),
        }
    }

    /// A store that waits for the plugin to open the database.
    pub(crate) fn pending(slot: Slot) -> Self {
        Self {
            slot,
            max_age_secs: Arc::new(AtomicU64::new(DEFAULT_MAX_AGE)),
        }
    }

    /// Sets the session lifetime.
    pub(crate) fn set_max_age(&self, max_age: Duration) {
        self.max_age_secs
            .store(ttl_secs(max_age), Ordering::Relaxed);
    }

    /// Deletes every expired session. Then compaction can give their
    /// space back. Returns how many sessions it deleted. The plugin runs
    /// this every [`SWEEP_EVERY`].
    ///
    /// # Errors
    ///
    /// [`HortonError::NotStarted`], or an error from the store.
    pub async fn sweep(&self) -> Result<u64, HortonError> {
        self.slot
            .get()
            .ok_or(HortonError::NotStarted)?
            .sweep_sessions()
            .await
    }

    fn horton(&self, op: &'static str) -> Result<&Horton, SessionStoreError> {
        self.slot
            .get()
            .ok_or_else(|| SessionStoreError::backend(op, HortonError::NotStarted))
    }
}

impl SessionStore for HortonSessionStore {
    async fn load(&self, id: &str) -> Result<Option<HashMap<String, String>>, SessionStoreError> {
        // The id comes from a cookie. An empty id names no session, so
        // the user gets a new one (as with `MemoryStore`).
        if id.is_empty() {
            return Ok(None);
        }
        let key = session_key_bytes(&session_hash(id));
        let Some(bytes) = self
            .horton("load")?
            .get_in(&key)
            .await
            .map_err(|e| SessionStoreError::backend("load", e))?
        else {
            return Ok(None);
        };
        // The plugin drops a session that does not parse. The user gets a
        // new session.
        Ok(serde_json::from_slice(&bytes)
            .inspect_err(
                |e| tracing::warn!(error = %e, "dropping a horton session that does not parse"),
            )
            .ok())
    }

    async fn save(&self, id: &str, data: HashMap<String, String>) -> Result<(), SessionStoreError> {
        if id.is_empty() {
            return Err(SessionStoreError::backend("save", HortonError::EmptyKey));
        }
        let hash = session_hash(id);
        let json = serde_json::to_vec(&data).map_err(|e| SessionStoreError::backend("save", e))?;
        if json.len() > crate::keys::VAL_MAX {
            tracing::warn!(
                bytes = json.len(),
                max = crate::keys::VAL_MAX,
                "a horton session is too large to save; store less in the session"
            );
        }
        let mut batch = Batch::new();
        batch.session_put(
            session_key_bytes(&hash),
            &hash,
            &json,
            Duration::from_secs(self.max_age_secs.load(Ordering::Relaxed)),
        );
        self.horton("save")?
            .write_in(batch, Space::Plugin)
            .await
            .map_err(|e| SessionStoreError::backend("save", e))
    }

    async fn destroy(&self, id: &str) -> Result<(), SessionStoreError> {
        // An empty id names no session: nothing to delete.
        if id.is_empty() {
            return Ok(());
        }
        let key = session_key_bytes(&session_hash(id));
        let mut batch = Batch::new();
        batch.delete(&key);
        self.horton("destroy")?
            .write_in(batch, Space::Plugin)
            .await
            .map_err(|e| SessionStoreError::backend("destroy", e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_session_that_does_not_parse_reads_as_none() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = crate::HortonConfig::default()
            .with_path(dir.path().join("c.horton"))
            .with_size_mib(crate::HortonConfig::min_size_mib())
            .with_sync(false);
        let db = Horton::open(&config).expect("open");
        let store = HortonSessionStore::new(db.clone(), Duration::from_secs(60));
        let mut batch = Batch::new();
        batch.put(&session_key_bytes(&session_hash("bad")), b"{not json");
        db.write_in(batch, crate::handle::Space::Plugin)
            .await
            .expect("write");
        assert_eq!(store.load("bad").await, Ok(None));
        assert_eq!(store.load("").await, Ok(None));
        db.close().await.expect("close");
        assert_eq!(store.sweep().await, Err(HortonError::Closed));
        let pending = HortonSessionStore::pending(Slot::default());
        assert_eq!(pending.sweep().await, Err(HortonError::NotStarted));
        assert!(pending.load("x").await.is_err());
    }
}
