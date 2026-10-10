//! The handle the plugin opens at startup, shared with parts it builds
//! before startup (session store, health, metrics).

use std::sync::{Arc, OnceLock};

use crate::handle::Horton;

/// Empty until startup opens the store.
#[derive(Clone, Default)]
pub struct Slot(Arc<OnceLock<Horton>>);

impl Slot {
    /// A slot that already holds `horton`.
    pub fn with(horton: Horton) -> Self {
        let slot = Self::default();
        slot.set(horton);
        slot
    }

    /// Stores the handle. A second call keeps the first handle.
    pub fn set(&self, horton: Horton) {
        let _ = self.0.set(horton);
    }

    /// The handle, once startup has opened the store.
    pub fn get(&self) -> Option<&Horton> {
        self.0.get()
    }
}
