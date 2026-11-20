//! horton fleets: one horton store for each tenant or slot (feature `fleet`).
//!
//! # Contract
//!
//! [`HortonEngine`] is the horton engine of an Autumn [`DatabaseFleet`]. Autumn core does the
//! keys, the paths, the opens, the eviction, the memory budget, the ownership and the fence.
//! The engine does the rest:
//!
//! - A fleet database is a directory. It holds the store file `store.horton` and the stamp file
//!   `identity`. The stamp file holds the key name and the epoch.
//! - `open` opens the store. The store takes an exclusive file lock, so one process at a time
//!   checks and writes the stamp.
//! - `open` refuses a database of another key ([`FleetError::Misplaced`]) and a newer epoch
//!   ([`FleetError::NewerEpoch`]). It never lowers the stamp.
//! - `close` flushes the store and stops its thread.
//! - `snapshot` writes `dir/<directory name>` with a copy of both files. The store thread
//!   flushes and finishes compaction first, then copies the store file between two requests.
//!   A database with no write since the last snapshot gives the same bytes. All-zero blocks of
//!   the copy stay holes. A closed database is copied as it is on disk.
//! - `remove` deletes the database directory.
//! - The ship mode is [`ShipMode::Generations`].
//! - [`HortonFleetPlugin`] reads `[horton_fleet]`, makes the fleet and puts it in the app state.
//!   [`FleetHorton`] is the extractor. It opens the database of the request tenant.
//! - With `[horton_fleet.ownership]`, the plugin claims its slot ranges before the first open.
//! - The plugin closes idle databases, and closes all databases at shutdown. This runs in each
//!   role: a role that opens a database must also close it.

use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use autumn_web::app::AppBuilder;
use autumn_web::config::{DatabaseFleetConfig, Env};
use autumn_web::db::fleet::{DatabaseFleet, FleetError};
use autumn_web::db::fleet_engine::{FleetEngine, ShipMode};
use autumn_web::db::fleet_ownership::{FleetOwnership, OWNERSHIP_INDICATOR, OwnerId, SlotRange};
use autumn_web::fleet::FleetLease;
use autumn_web::fleet_layout::FleetDbKey;
use autumn_web::lock::FencingToken;
use autumn_web::plugin::Plugin;
use autumn_web::reexports::tokio_util::sync::CancellationToken;
use autumn_web::{AppState, AutumnError};
use serde::{Deserialize, Serialize};

use crate::config::{HortonConfig, layered_section};
use crate::device::copy_closed;
use crate::error::HortonError;
use crate::handle::Horton;

/// The plugin name of [`HortonFleetPlugin`] in Autumn diagnostics.
pub const FLEET_PLUGIN_NAME: &str = "autumn-plugin-horton-fleet";

/// The default section of [`HortonFleetConfig`].
pub const DEFAULT_FLEET_SECTION: &str = "horton_fleet";

/// The default interval between two shipped generations.
pub const DEFAULT_SHIP_EVERY: Duration = Duration::from_mins(1);

/// The default memory that the fleet counts for one open database: 16 MiB.
pub const DEFAULT_RESIDENT_BYTES: u64 = 16 * 1024 * 1024;

/// The default size of a new store file in a fleet, in MiB.
pub const DEFAULT_FLEET_SIZE_MIB: u64 = 128;

/// The store file in a database directory.
pub const STORE_FILE: &str = "store.horton";

/// The stamp file in a database directory.
pub const IDENTITY_FILE: &str = "identity";

/// The horton engine of an Autumn fleet. See the module contract.
pub struct HortonEngine {
    template: HortonConfig,
    ship_every: Duration,
    resident_bytes: u64,
}

impl HortonEngine {
    /// Makes an engine that opens each store with `template`. The fleet sets the path. A fleet
    /// store never keeps sessions.
    #[must_use]
    pub const fn new(template: HortonConfig) -> Self {
        Self {
            template,
            ship_every: DEFAULT_SHIP_EVERY,
            resident_bytes: DEFAULT_RESIDENT_BYTES,
        }
    }

    /// Sets the interval between two shipped generations.
    #[must_use]
    pub const fn ship_every(mut self, every: Duration) -> Self {
        self.ship_every = every;
        self
    }

    /// Sets the memory that the fleet counts for one open database.
    ///
    /// The fleet budget closes idle databases with it.
    #[must_use]
    pub const fn resident_bytes(mut self, bytes: u64) -> Self {
        self.resident_bytes = bytes;
        self
    }

    fn config_for(&self, dir: &Path) -> HortonConfig {
        let mut config = self.template.clone().with_path(dir.join(STORE_FILE));
        config.enabled = true;
        config.sessions = false;
        config
    }
}

impl std::fmt::Debug for HortonEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HortonEngine")
            .field("size_mib", &self.template.size_mib)
            .field("sync", &self.template.sync)
            .field("ship_every", &self.ship_every)
            .field("resident_bytes", &self.resident_bytes)
            .finish_non_exhaustive()
    }
}

/// Makes an I/O fleet error. The text has the error kind, not stored data.
fn store_error(op: &'static str) -> impl FnOnce(HortonError) -> FleetError {
    move |err| FleetError::Io {
        op,
        detail: err.to_string(),
    }
}

fn io_error(op: &'static str) -> impl FnOnce(std::io::Error) -> FleetError {
    move |err| FleetError::Io {
        op,
        detail: err.to_string(),
    }
}

fn blocking_stopped(op: &'static str) -> FleetError {
    FleetError::Io {
        op,
        detail: "the blocking task stopped".to_owned(),
    }
}

/// The stamp: the key name and the epoch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Identity {
    name: String,
    epoch: u64,
}

/// Reads the stamp file of the database directory `dir`.
fn read_identity(dir: &Path) -> std::io::Result<Option<Identity>> {
    match std::fs::read(dir.join(IDENTITY_FILE)) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err),
    }
}

/// Writes the stamp file: a temporary file, a sync, then a rename.
fn write_identity(dir: &Path, identity: &Identity) -> std::io::Result<()> {
    let bytes = serde_json::to_vec(identity).map_err(std::io::Error::other)?;
    let temp = dir.join(format!("{IDENTITY_FILE}.tmp"));
    {
        let file = std::fs::File::create(&temp)?;
        std::io::Write::write_all(&mut &file, &bytes)?;
        file.sync_all()?;
    }
    std::fs::rename(&temp, dir.join(IDENTITY_FILE))
}

/// Checks and writes the stamp of the database directory `dir`.
fn stamp(dir: &Path, name: &str, epoch: u64) -> Result<(), FleetError> {
    let io = io_error;
    let found = read_identity(dir).map_err(io("stamp database"))?;
    let write = |epoch| {
        write_identity(
            dir,
            &Identity {
                name: name.to_owned(),
                epoch,
            },
        )
    };
    let Some(found) = found else {
        return write(epoch).map_err(io("stamp database"));
    };
    if found.name != name {
        return Err(FleetError::Misplaced {
            name: name.to_owned(),
            found: found.name,
        });
    }
    if epoch > 0 && found.epoch > epoch {
        return Err(FleetError::NewerEpoch {
            name: name.to_owned(),
            epoch,
            found: found.epoch,
        });
    }
    if epoch > found.epoch {
        write(epoch).map_err(io("stamp database"))?;
    }
    Ok(())
}

/// Makes the database directory. On Unix a new directory has mode `0700`.
fn make_dir(dir: &Path) -> std::io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(dir)
}

/// Opens the store in `dir` and checks the stamp. Runs on a blocking thread.
fn open_blocking(
    config: &HortonConfig,
    dir: &Path,
    name: &str,
    epoch: u64,
) -> Result<Horton, FleetError> {
    make_dir(dir).map_err(io_error("open database"))?;
    let store = Horton::open(config).map_err(store_error("open database"))?;
    if let Err(err) = stamp(dir, name, epoch) {
        if let Err(close) = store.close_blocking() {
            tracing::warn!(error = %close, "a horton fleet store did not close cleanly");
        }
        return Err(err);
    }
    Ok(store)
}

impl FleetEngine for HortonEngine {
    type Handle = Horton;

    async fn open(
        &self,
        key: &FleetDbKey,
        path: &Path,
        epoch: Option<FencingToken>,
    ) -> Result<Horton, FleetError> {
        let config = self.config_for(path);
        let dir = path.to_path_buf();
        let name = key.name();
        let epoch = epoch.map_or(0, FencingToken::get);
        tokio::task::spawn_blocking(move || open_blocking(&config, &dir, &name, epoch))
            .await
            .map_err(|_| blocking_stopped("open database"))?
    }

    fn close(&self, handle: &Horton) {
        if let Err(err) = handle.close_blocking() {
            tracing::warn!(error = %err, "a horton fleet store did not close cleanly");
        }
    }

    fn in_use(&self, handle: &Horton) -> bool {
        handle.is_busy()
    }

    async fn snapshot(
        &self,
        handle: &Horton,
        path: &Path,
        dir: &Path,
    ) -> Result<Vec<PathBuf>, FleetError> {
        let op = "snapshot database";
        let name = path.file_name().ok_or_else(|| FleetError::Io {
            op,
            detail: "the path has no file name".to_owned(),
        })?;
        let copy = dir.join(name);
        make_dir(&copy).map_err(io_error(op))?;
        let store = copy.join(STORE_FILE);
        if handle.is_closed() {
            let (from, to) = (path.join(STORE_FILE), store.clone());
            tokio::task::spawn_blocking(move || copy_closed(&from, &to))
                .await
                .map_err(|_| blocking_stopped(op))?
                .map_err(io_error(op))?;
        } else {
            handle
                .copy_to(store.clone())
                .await
                .map_err(store_error(op))?;
        }
        let identity = copy.join(IDENTITY_FILE);
        let from = path.join(IDENTITY_FILE);
        let to = identity.clone();
        tokio::task::spawn_blocking(move || std::fs::copy(from, to))
            .await
            .map_err(|_| blocking_stopped(op))?
            .map_err(io_error(op))?;
        Ok(vec![identity, store])
    }

    fn remove(&self, _key: &FleetDbKey, path: &Path) -> Result<bool, FleetError> {
        match std::fs::remove_dir_all(path) {
            Ok(()) => Ok(true),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(err) => Err(io_error("delete database")(err)),
        }
    }

    fn resident_bytes(&self, _handle: &Horton) -> u64 {
        self.resident_bytes
    }

    fn ship_mode(&self) -> ShipMode {
        ShipMode::Generations {
            every: self.ship_every,
        }
    }

    fn exists(&self, path: &Path) -> bool {
        path.join(STORE_FILE).is_file()
    }

    fn epoch_stamp(&self, _key: &FleetDbKey, path: &Path) -> Result<Option<u64>, FleetError> {
        read_identity(path)
            .map(|identity| identity.map(|identity| identity.epoch))
            .map_err(io_error("read epoch stamp"))
    }
}

/// The horton store of the request tenant, from a [`HortonFleetPlugin`] fleet.
///
/// It derefs to [`Horton`]. The fleet keeps the store open while the value lives.
///
/// The rejection is the fleet error status: `400` with no tenant, `421` when another host owns the
/// slot, and `503` with no fleet.
#[derive(Debug, Clone)]
pub struct FleetHorton(pub FleetLease<HortonEngine>);

impl FleetHorton {
    /// Opens the store of `key` with no request, for example in a job or a task.
    ///
    /// # Errors
    ///
    /// Returns the [`FleetError`] of [`autumn_web::fleet::resolve_key`].
    pub async fn for_key(state: &AppState, key: &str) -> Result<Self, FleetError> {
        autumn_web::fleet::resolve_key::<HortonEngine>(state, key)
            .await
            .map(Self)
    }
}

impl std::ops::Deref for FleetHorton {
    type Target = Horton;

    fn deref(&self) -> &Horton {
        self.0.handle()
    }
}

impl axum::extract::FromRequestParts<AppState> for FleetHorton {
    type Rejection = AutumnError;

    async fn from_request_parts(
        parts: &mut http::request::Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        autumn_web::fleet::resolve::<HortonEngine>(parts, state)
            .await
            .map(Self)
            .map_err(AutumnError::from)
    }
}

/// The configuration of a horton fleet.
///
/// ```toml
/// [horton_fleet]
/// mode = "tenant"
/// root = "data/tenants"
/// path = "{bucket}/{tenant}"
/// max_resident_bytes = 1073741824
/// ship_every_secs = 60
/// resident_bytes = 16777216
///
/// [horton_fleet.engine]
/// size_mib = 128
/// sync = true
///
/// [horton_fleet.ownership]
/// owner = "host-a"
/// ```
///
/// The top keys are the Autumn fleet keys (`[database.fleet]`). `engine` has the `size_mib` and
/// `sync` keys of `[horton]`. The variables `AUTUMN_HORTON_FLEET__ROOT` and
/// `AUTUMN_HORTON_FLEET__OWNERSHIP__OWNER` set `root` and `ownership.owner`.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct HortonFleetConfig {
    /// The Autumn fleet keys.
    pub fleet: DatabaseFleetConfig,
    /// The store configuration of each database. The fleet sets the path.
    pub engine: HortonConfig,
    /// The interval between two shipped generations.
    pub ship_every: Duration,
    /// The memory that the fleet counts for one open database.
    pub resident_bytes: u64,
}

/// The `engine` keys of a fleet.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EngineKeys {
    size_mib: Option<u64>,
    sync: Option<bool>,
}

/// The default store configuration of a fleet database.
fn default_engine() -> HortonConfig {
    HortonConfig {
        size_mib: DEFAULT_FLEET_SIZE_MIB,
        ..HortonConfig::default()
    }
}

impl HortonFleetConfig {
    /// Makes a configuration with `fleet`, the default engine and the default limits.
    #[must_use]
    pub fn new(fleet: DatabaseFleetConfig) -> Self {
        Self {
            fleet,
            engine: default_engine(),
            ship_every: DEFAULT_SHIP_EVERY,
            resident_bytes: DEFAULT_RESIDENT_BYTES,
        }
    }

    /// Reads `[section]` from the app files and the environment.
    ///
    /// # Errors
    ///
    /// Returns [`HortonError::Config`] if a file is not valid TOML or a value is not valid.
    pub fn resolve(section: &str) -> Result<Self, HortonError> {
        autumn_web::dotenv::os_env_with_dotenv().map_or_else(
            |_| Self::resolve_with_env(section, &autumn_web::config::OsEnv),
            |env| Self::resolve_with_env(section, &env),
        )
    }

    /// Reads `[section]` with `env` as the environment.
    ///
    /// # Errors
    ///
    /// See [`resolve`](Self::resolve).
    pub fn resolve_with_env(section: &str, env: &dyn Env) -> Result<Self, HortonError> {
        let layered = layered_section(section, toml::Value::Table(toml::Table::new()), env)?;
        let toml::Value::Table(mut table) = layered else {
            return Err(HortonError::Config(format!("[{section}] must be a table")));
        };
        let prefix = env_prefix(section);
        if let Ok(root) = env.var(&format!("{prefix}__ROOT")) {
            table.insert("root".to_owned(), toml::Value::String(root));
        }
        if let Ok(owner) = env.var(&format!("{prefix}__OWNERSHIP__OWNER")) {
            let ownership = table
                .entry("ownership")
                .or_insert_with(|| toml::Value::Table(toml::Table::new()));
            if let toml::Value::Table(ownership) = ownership {
                ownership.insert("owner".to_owned(), toml::Value::String(owner));
            }
        }
        Self::from_table(section, table)
    }

    /// Reads a configuration from the TOML table of `[section]`.
    ///
    /// # Errors
    ///
    /// Returns [`HortonError::Config`] for an unknown key or a value that is not valid.
    pub fn from_table(section: &str, mut table: toml::Table) -> Result<Self, HortonError> {
        let bad = |key: &str, err: &dyn std::fmt::Display| {
            HortonError::Config(format!("[{section}] {key}: {err}"))
        };
        let mut engine = default_engine();
        match table.remove("engine") {
            None => {}
            Some(value @ toml::Value::Table(_)) => {
                let keys: EngineKeys = value.try_into().map_err(|err| bad("engine", &err))?;
                if let Some(size_mib) = keys.size_mib {
                    engine.size_mib = size_mib;
                }
                if let Some(sync) = keys.sync {
                    engine.sync = sync;
                }
            }
            Some(_) => {
                return Err(HortonError::Config(format!(
                    "{section}.engine must be a table"
                )));
            }
        }
        let ship_every = match table.remove("ship_every_secs") {
            None => DEFAULT_SHIP_EVERY,
            Some(value) => Duration::from_secs(
                value
                    .as_integer()
                    .and_then(|v| u64::try_from(v).ok())
                    .ok_or_else(|| bad("ship_every_secs", &"must be a whole number of seconds"))?,
            ),
        };
        let resident_bytes = match table.remove("resident_bytes") {
            None => DEFAULT_RESIDENT_BYTES,
            Some(value) => value
                .as_integer()
                .and_then(|v| u64::try_from(v).ok())
                .ok_or_else(|| bad("resident_bytes", &"must be a whole number"))?,
        };
        let fleet: DatabaseFleetConfig = toml::Value::Table(table)
            .try_into()
            .map_err(|err| bad("fleet", &err))?;
        let config = Self {
            fleet,
            engine,
            ship_every,
            resident_bytes,
        };
        config.validate_section(section)?;
        Ok(config)
    }

    /// Checks each value.
    ///
    /// # Errors
    ///
    /// Returns [`HortonError::Config`] that names the first key that is not valid.
    pub fn validate(&self) -> Result<(), HortonError> {
        self.validate_section(DEFAULT_FLEET_SECTION)
    }

    fn validate_section(&self, section: &str) -> Result<(), HortonError> {
        let fail =
            |key: &str, rule: &str| Err(HortonError::Config(format!("{section}.{key} {rule}")));
        if self.fleet.root.trim().is_empty() {
            return fail("root", "must be a directory");
        }
        if self.resident_bytes == 0 {
            return fail("resident_bytes", "must be 1 or more");
        }
        if self.ship_every.is_zero() {
            return fail("ship_every_secs", "must be 1 or more");
        }
        let min = HortonConfig::min_size_mib();
        if self.engine.size_mib < min {
            return fail(
                "engine.size_mib",
                &format!("is {}; the minimum is {min}", self.engine.size_mib),
            );
        }
        Ok(())
    }

    /// Makes the engine of this configuration.
    #[must_use]
    pub fn engine(&self) -> HortonEngine {
        HortonEngine::new(self.engine.clone())
            .ship_every(self.ship_every)
            .resident_bytes(self.resident_bytes)
    }
}

fn env_prefix(section: &str) -> String {
    let name: String = section
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect();
    format!("AUTUMN_{name}")
}

enum FleetSource {
    Section(String),
    Explicit(Box<HortonFleetConfig>),
}

/// The running parts of a fleet: the idle sweeper and the ownership loop.
struct FleetRuntime {
    fleet: DatabaseFleet<HortonEngine>,
    stop: CancellationToken,
    maintenance: tokio::task::JoinHandle<()>,
    ownership: Option<(CancellationToken, tokio::task::JoinHandle<()>)>,
}

impl FleetRuntime {
    async fn shutdown(self) {
        self.stop.cancel();
        let _ = self.maintenance.await;
        if let Some((stop, handle)) = self.ownership {
            stop.cancel();
            let _ = handle.await;
            if self.fleet.release_ownership().await {
                tracing::info!("the horton fleet released its slot ranges");
            }
        }
    }
}

/// Makes a fleet of horton stores and puts it in the app state.
///
/// ```rust,no_run
/// use autumn_plugin_horton::fleet::{FleetHorton, HortonFleetPlugin};
///
/// #[autumn_web::get("/visits")]
/// async fn visits(store: FleetHorton) -> autumn_web::AutumnResult<String> {
///     let seen = store
///         .get(b"visits")
///         .await
///         .map_err(autumn_plugin_horton::HortonError::into_autumn)?;
///     Ok(format!("{} bytes", seen.map_or(0, |v| v.len())))
/// }
///
/// # async fn run() {
/// autumn_web::app()
///     .plugin(HortonFleetPlugin::new())
///     .routes(autumn_web::routes![visits])
///     .run()
///     .await;
/// # }
/// ```
#[must_use]
pub struct HortonFleetPlugin {
    source: FleetSource,
}

impl Default for HortonFleetPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl HortonFleetPlugin {
    /// Makes a plugin that reads `[horton_fleet]`.
    pub fn new() -> Self {
        Self {
            source: FleetSource::Section(DEFAULT_FLEET_SECTION.to_owned()),
        }
    }

    /// Reads `[section]` instead of `[horton_fleet]`.
    pub fn config_section(mut self, section: impl Into<String>) -> Self {
        self.source = FleetSource::Section(section.into());
        self
    }

    /// Uses `config` and reads no files or variables.
    pub fn config(mut self, config: HortonFleetConfig) -> Self {
        self.source = FleetSource::Explicit(Box::new(config));
        self
    }

    fn resolve(source: &FleetSource) -> Result<HortonFleetConfig, HortonError> {
        match source {
            FleetSource::Section(section) => HortonFleetConfig::resolve(section),
            FleetSource::Explicit(config) => {
                config.validate()?;
                Ok((**config).clone())
            }
        }
    }
}

impl std::fmt::Debug for HortonFleetPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let source = match &self.source {
            FleetSource::Section(section) => section.as_str(),
            FleetSource::Explicit(_) => "(explicit)",
        };
        f.debug_struct("HortonFleetPlugin")
            .field("config", &source)
            .finish()
    }
}

/// Starts the fleet: ownership first, then the idle sweeper.
async fn start(
    state: &AppState,
    config: &HortonFleetConfig,
    engine: HortonEngine,
) -> Result<FleetRuntime, String> {
    let fleet = DatabaseFleet::with_engine(&config.fleet, engine).map_err(|e| e.to_string())?;
    let ownership = match &config.fleet.ownership {
        None => None,
        Some(settings) => {
            let store = autumn_web::fleet::ownership_store(state)
                .ok_or("fleet ownership needs a control database or a fleet_ownership_store")?;
            store.ensure_schema().await.map_err(|e| e.to_string())?;
            let ranges = settings
                .ranges()?
                .into_iter()
                .map(|(start, end)| SlotRange::new(start, end))
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| e.to_string())?;
            let owner = OwnerId::new(settings.owner.as_str()).map_err(|e| e.to_string())?;
            let ownership = Arc::new(
                FleetOwnership::new(
                    store,
                    owner,
                    ranges,
                    Duration::from_secs(settings.lease_secs),
                )
                .map_err(|e| e.to_string())?,
            );
            fleet
                .start_ownership(Arc::clone(&ownership))
                .await
                .map_err(|e| e.to_string())?;
            if let Err(err) = state.health_indicator_registry().register(
                OWNERSHIP_INDICATOR,
                autumn_web::actuator::IndicatorGroup::HealthOnly,
                ownership.indicator(),
            ) {
                tracing::warn!("{err}");
            }
            let stop = CancellationToken::new();
            let handle = fleet.spawn_ownership(stop.clone());
            Some((stop, handle))
        }
    };
    let stop = CancellationToken::new();
    let maintenance = fleet.spawn_maintenance(stop.clone());
    state.insert_extension(fleet.clone());
    Ok(FleetRuntime {
        fleet,
        stop,
        maintenance,
        ownership,
    })
}

impl Plugin for HortonFleetPlugin {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed(FLEET_PLUGIN_NAME)
    }

    fn build(self, app: AppBuilder) -> AppBuilder {
        let resolved = Arc::new(Self::resolve(&self.source));
        let runtime: Arc<Mutex<Option<FleetRuntime>>> = Arc::default();
        let on_start = Arc::clone(&runtime);
        app.on_startup(move |state| {
            let resolved = Arc::clone(&resolved);
            let runtime = Arc::clone(&on_start);
            async move {
                let fail = |err: &dyn std::fmt::Display| {
                    AutumnError::internal_server_error_msg(format!("{FLEET_PLUGIN_NAME}: {err}"))
                };
                let config = resolved.as_ref().clone().map_err(|err| fail(&err))?;
                let started = start(&state, &config, config.engine())
                    .await
                    .map_err(|err| fail(&err))?;
                *runtime.lock().unwrap_or_else(PoisonError::into_inner) = Some(started);
                tracing::info!("the horton fleet is ready");
                Ok(())
            }
        })
        .on_shutdown(move || {
            let runtime = runtime
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take();
            async move {
                if let Some(runtime) = runtime {
                    runtime.shutdown().await;
                }
            }
        })
    }
}

#[cfg(test)]
mod tests;
