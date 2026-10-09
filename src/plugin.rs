//! [`HortonPlugin`]: mounts the store on an Autumn app.

use std::borrow::Cow;
use std::sync::Arc;
use std::time::Duration;

use autumn_web::app::AppBuilder;
use autumn_web::plugin::Plugin;
use autumn_web::plugin_contract::PluginContract;
use autumn_web::session::SessionBackend;
use autumn_web::{AppState, AutumnError};

use crate::config::{HortonConfig, SECTION};
use crate::error::HortonError;
use crate::handle::Horton;
use crate::health::HortonHealth;
use crate::metrics::HortonMetrics;
use crate::session::{HortonSessionStore, SWEEP_EVERY};
use crate::slot::Slot;

/// The plugin and crate name.
pub const NAME: &str = "autumn-plugin-horton";

/// The health indicator and metrics source name.
pub const COMPONENT: &str = "horton";

/// Mounts a horton store on an Autumn app.
///
/// At startup the plugin opens the store and puts a [`Horton`] handle in
/// `AppState`. Get it with `state.extension::<Horton>()`. The plugin also
/// adds the `horton` health indicator and the `horton_*` metrics. With
/// `sessions = true` it keeps Autumn sessions in the store. At shutdown it
/// flushes and closes the store.
///
/// ```no_run
/// use autumn_plugin_horton::HortonPlugin;
///
/// # fn demo(app: autumn_web::app::AppBuilder) -> autumn_web::app::AppBuilder {
/// app.plugin(HortonPlugin::new().path("data/app.horton"))
/// # }
/// ```
#[derive(Debug, Clone, Default)]
#[must_use]
pub struct HortonPlugin {
    config: HortonConfig,
    fixed: bool,
}

impl HortonPlugin {
    /// The plugin with the default config. `[horton]` in `autumn.toml`,
    /// the profile files and `AUTUMN_HORTON__*` variables override it.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the store file.
    pub fn path(mut self, path: impl Into<std::path::PathBuf>) -> Self {
        self.config.path = path.into();
        self
    }

    /// Sets the size of a new store file, in MiB.
    pub const fn size_mib(mut self, size_mib: u64) -> Self {
        self.config.size_mib = size_mib;
        self
    }

    /// Sets `fdatasync` on every commit.
    pub const fn sync(mut self, sync: bool) -> Self {
        self.config.sync = sync;
        self
    }

    /// Keeps Autumn sessions in the store.
    pub const fn sessions(mut self, sessions: bool) -> Self {
        self.config.sessions = sessions;
        self
    }

    /// Uses `config` with no change. The plugin then reads no file and no
    /// environment variable.
    pub fn fixed_config(mut self, config: HortonConfig) -> Self {
        self.config = config;
        self.fixed = true;
        self
    }

    fn resolve(&self) -> Result<HortonConfig, HortonError> {
        if self.fixed {
            self.config.validate().map(|()| self.config.clone())
        } else {
            self.config.clone().resolve()
        }
    }
}

impl Plugin for HortonPlugin {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed(NAME)
    }

    fn contract(&self) -> Option<PluginContract> {
        Some(
            PluginContract::new(NAME)
                .plugin_version(env!("CARGO_PKG_VERSION"))
                .autumn_web("0.8"),
        )
    }

    fn build(self, app: AppBuilder) -> AppBuilder {
        let app = app.config_section(SECTION);
        let config = match self.resolve() {
            Ok(config) => config,
            Err(e) => {
                // Report the error where Autumn reports startup failures.
                let message = e.to_string();
                return app.on_startup(move |_| {
                    let message = message.clone();
                    async move { Err(AutumnError::service_unavailable_msg(message)) }
                });
            }
        };
        if !config.enabled {
            tracing::info!("horton is disabled; the plugin does nothing");
            return app;
        }

        let slot = Slot::default();
        let sessions = config
            .sessions
            .then(|| HortonSessionStore::pending(slot.clone()));
        let mut app = app
            .health_indicator(COMPONENT, Arc::new(HortonHealth::new(slot.clone())))
            .metrics_source(COMPONENT, Arc::new(HortonMetrics::new(slot.clone())));
        if let Some(store) = &sessions {
            app = app.with_session_store(store.clone());
        }
        let start_slot = slot.clone();
        app.on_startup(move |state| {
            start(state, config.clone(), start_slot.clone(), sessions.clone())
        })
        .on_shutdown(move || stop(slot.clone()))
    }
}

/// Opens the store and publishes the handle.
async fn start(
    state: AppState,
    config: HortonConfig,
    slot: Slot,
    sessions: Option<HortonSessionStore>,
) -> Result<(), AutumnError> {
    let fail = |e: HortonError| AutumnError::service_unavailable_msg(format!("horton: {e}"));
    let path = config.path.clone();
    let db = tokio::task::spawn_blocking(move || Horton::open(&config))
        .await
        .map_err(|e| AutumnError::internal_server_error_msg(format!("horton open task: {e}")))?
        .map_err(fail)?;
    slot.set(db.clone());
    if let Some(store) = &sessions {
        let session = state.config().session;
        if session.backend != SessionBackend::Memory {
            tracing::warn!(
                backend = ?session.backend,
                "horton sessions = true: the horton store replaces the [session] backend"
            );
        }
        store.set_max_age(Duration::from_secs(session.max_age_secs));
        tokio::spawn(sweep_loop(store.clone()));
    }
    state.insert_extension(db);
    tracing::info!(path = %path.display(), "horton store open");
    Ok(())
}

/// Sweeps expired sessions every [`SWEEP_EVERY`] until the store closes.
async fn sweep_loop(store: HortonSessionStore) {
    let mut every = tokio::time::interval(SWEEP_EVERY);
    every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        every.tick().await;
        match store.sweep().await {
            Ok(0) => {}
            Ok(n) => tracing::debug!(sessions = n, "horton swept expired sessions"),
            Err(HortonError::Closed | HortonError::NotStarted) => return,
            Err(e) => tracing::warn!(error = %e, "horton session sweep failed"),
        }
    }
}

/// Flushes and closes the store.
async fn stop(slot: Slot) {
    if let Some(db) = slot.get()
        && let Err(e) = db.close().await
    {
        tracing::error!(error = %e, "horton close failed; the WAL keeps the data");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(dir: &tempfile::TempDir) -> HortonConfig {
        HortonConfig::default()
            .with_path(dir.path().join("p.horton"))
            .with_size_mib(HortonConfig::min_size_mib())
            .with_sync(false)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stop_closes_the_store_and_releases_the_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let slot = Slot::default();
        start(AppState::for_test(), config(&dir), slot.clone(), None)
            .await
            .expect("start");
        let db = slot.get().expect("open").clone();
        db.put(b"k", b"v").await.expect("put");
        stop(slot.clone()).await;
        assert!(db.is_closed());
        let again = Horton::open(&config(&dir)).expect("the lock is free");
        assert_eq!(again.get(b"k").await, Ok(Some(b"v".to_vec())));
        again.close().await.expect("close");
        stop(Slot::default()).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn start_sets_the_session_lifetime_from_autumn_config() {
        let dir = tempfile::tempdir().expect("tempdir");
        let slot = Slot::default();
        let store = HortonSessionStore::pending(slot.clone());
        start(
            AppState::for_test(),
            config(&dir),
            slot.clone(),
            Some(store.clone()),
        )
        .await
        .expect("start");
        assert!(format!("{store:?}").contains("max_age_secs: 86400"));
        stop(slot).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_sweep_loop_ends_when_the_store_closes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let slot = Slot::default();
        let store = HortonSessionStore::pending(slot.clone());
        // Not open yet: the loop ends at once.
        sweep_loop(store.clone()).await;
        start(AppState::for_test(), config(&dir), slot.clone(), None)
            .await
            .expect("start");
        let task = tokio::spawn(sweep_loop(store));
        stop(slot).await;
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("the loop ends")
            .expect("join");
    }

    #[test]
    fn builder_setters_set_the_config() {
        let p = HortonPlugin::new()
            .path("x.horton")
            .size_mib(300)
            .sync(false)
            .sessions(true);
        assert_eq!(p.config.path, std::path::PathBuf::from("x.horton"));
        assert_eq!(p.config.size_mib, 300);
        assert!(!p.config.sync);
        assert!(p.config.sessions);
        assert!(!p.fixed);
    }
}
