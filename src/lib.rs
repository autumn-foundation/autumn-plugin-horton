//! An [Autumn](https://crates.io/crates/autumn-web) plugin for
//! [horton](https://crates.io/crates/horton), an embedded LSM key-value
//! store.
//!
//! Add [`HortonPlugin`] to the app. At startup it opens the store and puts
//! a [`Horton`] handle in `AppState`. See the README for config and limits.

pub mod config;
pub mod device;
mod engine;
pub mod error;
mod group;
mod handle;
mod health;
pub mod keys;
mod metrics;
mod plugin;
mod session;
mod shape;
mod slot;
pub mod ttl;

pub use config::HortonConfig;
pub use engine::{MAX_BATCH_BYTES, RESERVE_SLOTS};
pub use error::HortonError;
pub use handle::{Batch, Clock, Horton, SCAN_MAX, Stats};
pub use health::HortonHealth;
pub use keys::{KEY_MAX, VAL_MAX};
pub use metrics::HortonMetrics;
pub use plugin::{COMPONENT, HortonPlugin, NAME};
pub use session::{HortonSessionStore, SWEEP_EVERY};
