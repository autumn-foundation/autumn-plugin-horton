//! An [Autumn](https://crates.io/crates/autumn-web) plugin for
//! [horton](https://crates.io/crates/horton), an embedded LSM key-value
//! store.

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
pub use error::HortonError;
pub use handle::{Batch, BatchOp, Clock, Horton, Stats};
pub use health::HortonHealth;
pub use keys::{KEY_MAX, VAL_MAX};
pub use metrics::HortonMetrics;
pub use plugin::{COMPONENT, HortonPlugin, NAME};
pub use session::HortonSessionStore;
