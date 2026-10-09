//! [`HortonSessionStore`]: Autumn sessions in the horton store.
//!
//! A session is one JSON object under the key `0x00 "session/" <id>`. The
//! app API cannot read, write or scan these keys. Each save resets the
//! session's TTL to `session.max_age_secs`. Compaction purges expired
//! sessions.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use autumn_web::session::{SessionStore, SessionStoreError};

use crate::error::HortonError;
use crate::handle::{Batch, Horton, Space};
use crate::keys::session_key;
use crate::slot::Slot;
use crate::ttl::ttl_secs;

/// Autumn's default `session.max_age_secs`.
const DEFAULT_MAX_AGE: u64 = 86_400;

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

    fn horton(&self, op: &'static str) -> Result<&Horton, SessionStoreError> {
        self.slot
            .get()
            .ok_or_else(|| SessionStoreError::backend(op, HortonError::NotStarted))
    }
}

fn key(op: &'static str, id: &str) -> Result<Vec<u8>, SessionStoreError> {
    session_key(id).map_err(|e| SessionStoreError::backend(op, e))
}

impl SessionStore for HortonSessionStore {
    async fn load(&self, id: &str) -> Result<Option<HashMap<String, String>>, SessionStoreError> {
        let key = key("load", id)?;
        let Some(bytes) = self
            .horton("load")?
            .get_in(&key)
            .await
            .map_err(|e| SessionStoreError::backend("load", e))?
        else {
            return Ok(None);
        };
        // A session that does not parse is lost, not fatal: the user gets
        // a new session.
        Ok(serde_json::from_slice(&bytes)
            .inspect_err(
                |e| tracing::warn!(error = %e, "dropping a horton session that does not parse"),
            )
            .ok())
    }

    async fn save(&self, id: &str, data: HashMap<String, String>) -> Result<(), SessionStoreError> {
        let key = key("save", id)?;
        let json = serde_json::to_vec(&data).map_err(|e| SessionStoreError::backend("save", e))?;
        let mut batch = Batch::new();
        batch.put_with_ttl(
            &key,
            &json,
            Duration::from_secs(self.max_age_secs.load(Ordering::Relaxed)),
        );
        self.horton("save")?
            .write_in(batch, Space::Plugin)
            .await
            .map_err(|e| SessionStoreError::backend("save", e))
    }

    async fn destroy(&self, id: &str) -> Result<(), SessionStoreError> {
        let key = key("destroy", id)?;
        let mut batch = Batch::new();
        batch.delete(&key);
        self.horton("destroy")?
            .write_in(batch, Space::Plugin)
            .await
            .map_err(|e| SessionStoreError::backend("destroy", e))
    }
}
