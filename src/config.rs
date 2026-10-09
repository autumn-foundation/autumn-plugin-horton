//! The `[horton]` config section.
//!
//! Layers, lowest first:
//!
//! 1. [`HortonConfig::default`]
//! 2. The builder setters on [`HortonPlugin`](crate::HortonPlugin)
//! 3. `autumn.toml` `[horton]`
//! 4. `autumn.toml` `[profile.<p>.horton]`
//! 5. `autumn-<p>.toml` `[horton]`
//! 6. `AUTUMN_HORTON__<FIELD>` environment variables (also from `.env`)
//!
//! An unknown key, a bad type or a bad value is an error. The plugin then
//! stops the app at startup.

use std::path::{Path, PathBuf};

use autumn_web::config::Env;
use serde::{Deserialize, Serialize};

use crate::error::HortonError;
use crate::shape::{BLOCK, ShapeDb};

/// The config table name.
pub const SECTION: &str = "horton";

const MIB: u64 = 1024 * 1024;

/// The store settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[non_exhaustive]
pub struct HortonConfig {
    /// Open the store at startup. `false` makes the plugin do nothing.
    pub enabled: bool,
    /// The store file. A relative path starts at the working directory.
    /// The plugin makes the parent directories.
    pub path: PathBuf,
    /// The size of a new store file, in MiB. The file is sparse. An
    /// existing file keeps its size.
    pub size_mib: u64,
    /// `fdatasync` every commit. `false` is faster, but a power cut can
    /// lose the last writes. A process crash loses nothing.
    pub sync: bool,
    /// Keep Autumn sessions in the store.
    pub sessions: bool,
}

impl Default for HortonConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            path: PathBuf::from("data/store.horton"),
            size_mib: 256,
            sync: true,
            sessions: false,
        }
    }
}

impl HortonConfig {
    /// Sets [`enabled`](Self::enabled).
    #[must_use]
    pub const fn with_enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    /// Sets [`path`](Self::path).
    #[must_use]
    pub fn with_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.path = path.into();
        self
    }

    /// Sets [`size_mib`](Self::size_mib).
    #[must_use]
    pub const fn with_size_mib(mut self, size_mib: u64) -> Self {
        self.size_mib = size_mib;
        self
    }

    /// Sets [`sync`](Self::sync).
    #[must_use]
    pub const fn with_sync(mut self, sync: bool) -> Self {
        self.sync = sync;
        self
    }

    /// Sets [`sessions`](Self::sessions).
    #[must_use]
    pub const fn with_sessions(mut self, sessions: bool) -> Self {
        self.sessions = sessions;
        self
    }

    /// The smallest `size_mib` that holds a store.
    #[must_use]
    pub const fn min_size_mib() -> u64 {
        let bytes = ShapeDb::<crate::device::FileDevice>::MIN_DEVICE_BLOCKS * BLOCK as u64;
        bytes.div_ceil(MIB)
    }

    /// Blocks in a new store file.
    #[must_use]
    pub const fn create_blocks(&self) -> u64 {
        self.size_mib.saturating_mul(MIB) / BLOCK as u64
    }

    /// Checks the values.
    ///
    /// # Errors
    ///
    /// [`HortonError::Config`] for an empty path or a size below
    /// [`min_size_mib`](Self::min_size_mib). A disabled store is not
    /// checked.
    pub fn validate(&self) -> Result<(), HortonError> {
        if !self.enabled {
            return Ok(());
        }
        if self.path.as_os_str().is_empty() {
            return Err(HortonError::Config("horton.path must not be empty".into()));
        }
        let min = Self::min_size_mib();
        if self.size_mib < min {
            return Err(HortonError::Config(format!(
                "horton.size_mib is {}; the minimum is {min}",
                self.size_mib
            )));
        }
        Ok(())
    }

    /// Resolves the config for the active profile from the process
    /// environment and `.env` files. `self` gives the builder values.
    ///
    /// # Errors
    ///
    /// [`HortonError::Config`]; see [`resolve_with_env`](Self::resolve_with_env).
    pub fn resolve(self) -> Result<Self, HortonError> {
        autumn_web::dotenv::os_env_with_dotenv().map_or_else(
            |_| self.clone().resolve_with_env(&autumn_web::config::OsEnv),
            |env| self.clone().resolve_with_env(&env),
        )
    }

    /// Resolves the config from `env`. `self` gives the builder values.
    ///
    /// # Errors
    ///
    /// [`HortonError::Config`] when a file cannot be read or parsed, when
    /// `[horton]` has an unknown key or a bad type, when an environment
    /// value does not parse, or when [`validate`](Self::validate) fails.
    pub fn resolve_with_env(self, env: &dyn Env) -> Result<Self, HortonError> {
        let mut merged = toml::Table::new();
        merged.insert(
            SECTION.into(),
            toml::Value::try_from(&self).map_err(|e| HortonError::Config(e.to_string()))?,
        );
        let mut merged = toml::Value::Table(merged);
        let (selected, canonical) = active_profile(env);

        if let Some(base) = read_toml(&find_file("autumn.toml", env))? {
            merge(&mut merged, section_only(&base));
            for name in inline_profile_names(&canonical) {
                if let Some(profile) = base
                    .get("profile")
                    .and_then(|p| p.get(name))
                    .map(section_only)
                {
                    merge(&mut merged, profile);
                }
            }
        }
        for name in autumn_web::config::profile_override_file_lookup_names(&canonical, &selected) {
            if let Some(file) = read_toml(&find_file(&format!("autumn-{name}.toml"), env))? {
                merge(&mut merged, section_only(&file));
                break;
            }
        }

        let section = merged
            .get(SECTION)
            .cloned()
            .unwrap_or_else(|| toml::Value::Table(toml::Table::new()));
        let mut config: Self = section
            .try_into()
            .map_err(|e: toml::de::Error| HortonError::Config(e.message().to_owned()))?;
        config.apply_env(env)?;
        config.validate()?;
        Ok(config)
    }

    fn apply_env(&mut self, env: &dyn Env) -> Result<(), HortonError> {
        if let Some(v) = env_value(env, "ENABLED") {
            self.enabled = parse_bool("ENABLED", &v)?;
        }
        if let Some(v) = env_value(env, "PATH") {
            self.path = PathBuf::from(v);
        }
        if let Some(v) = env_value(env, "SIZE_MIB") {
            self.size_mib = v.parse().map_err(|_| bad_env("SIZE_MIB", &v))?;
        }
        if let Some(v) = env_value(env, "SYNC") {
            self.sync = parse_bool("SYNC", &v)?;
        }
        if let Some(v) = env_value(env, "SESSIONS") {
            self.sessions = parse_bool("SESSIONS", &v)?;
        }
        Ok(())
    }
}

/// A non-blank `AUTUMN_HORTON__<field>` value.
fn env_value(env: &dyn Env, field: &str) -> Option<String> {
    env.var(&format!("AUTUMN_HORTON__{field}"))
        .ok()
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty())
}

fn bad_env(field: &str, value: &str) -> HortonError {
    HortonError::Config(format!("AUTUMN_HORTON__{field}={value} does not parse"))
}

fn parse_bool(field: &str, value: &str) -> Result<bool, HortonError> {
    match value.to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => Ok(true),
        "false" | "0" | "no" | "off" => Ok(false),
        _ => Err(bad_env(field, value)),
    }
}

/// The non-blank value of `key` in `env`.
fn env_trimmed(env: &dyn Env, key: &str) -> Option<String> {
    env.var(key)
        .ok()
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty())
}

/// `(selected spelling, canonical name)` of the active profile, in core's
/// order: `AUTUMN_ENV`, `AUTUMN_PROFILE`, `--profile`, `AUTUMN_IS_DEBUG=0`
/// (prod), then `dev`.
fn active_profile(env: &dyn Env) -> (String, String) {
    let selected = env_trimmed(env, "AUTUMN_ENV")
        .or_else(|| env_trimmed(env, "AUTUMN_PROFILE"))
        .or_else(profile_arg)
        .unwrap_or_else(|| {
            if env_trimmed(env, "AUTUMN_IS_DEBUG").as_deref() == Some("0") {
                "prod".to_owned()
            } else {
                "dev".to_owned()
            }
        });
    let canonical =
        autumn_web::config::normalize_profile_name(&selected).unwrap_or_else(|| "dev".to_owned());
    (selected, canonical)
}

/// `--profile <name>` or `--profile=<name>` on the command line.
fn profile_arg() -> Option<String> {
    let args: Vec<String> = std::env::args().collect();
    args.iter().enumerate().find_map(|(i, arg)| {
        let value = if arg == "--profile" {
            args.get(i + 1).map(String::as_str)
        } else {
            arg.strip_prefix("--profile=")
        };
        value
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_owned)
    })
}

/// `AUTUMN_MANIFEST_DIR/<name>` when that file exists, else `<name>` in
/// the working directory. Core finds its config files the same way.
fn find_file(name: &str, env: &dyn Env) -> PathBuf {
    if let Some(dir) = env_trimmed(env, "AUTUMN_MANIFEST_DIR") {
        let candidate = PathBuf::from(dir).join(name);
        if candidate.exists() {
            return candidate;
        }
    }
    PathBuf::from(name)
}

/// The parsed file, or `None` when it does not exist.
fn read_toml(path: &Path) -> Result<Option<toml::Value>, HortonError> {
    match std::fs::read_to_string(path) {
        Ok(text) => text
            .parse::<toml::Table>()
            .map(|t| Some(toml::Value::Table(t)))
            .map_err(|e| HortonError::Config(format!("cannot parse {}: {e}", path.display()))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(HortonError::Config(format!(
            "cannot read {}: {e}",
            path.display()
        ))),
    }
}

/// Only the `[horton]` table of a document, so other sections never merge.
fn section_only(doc: &toml::Value) -> toml::Value {
    let mut out = toml::Table::new();
    if let Some(section) = doc.get(SECTION) {
        out.insert(SECTION.into(), section.clone());
    }
    toml::Value::Table(out)
}

/// Inline `[profile.<name>]` names, alias first so the canonical name wins.
fn inline_profile_names(canonical: &str) -> Vec<&str> {
    match canonical {
        "prod" => vec!["production", "prod"],
        "dev" => vec!["development", "dev"],
        other => vec![other],
    }
}

/// Merges `overlay` into `base`: tables merge, other values replace.
fn merge(base: &mut toml::Value, overlay: toml::Value) {
    match (base, overlay) {
        (toml::Value::Table(base), toml::Value::Table(overlay)) => {
            for (key, value) in overlay {
                match base.get_mut(&key) {
                    Some(slot) if slot.is_table() && value.is_table() => merge(slot, value),
                    _ => {
                        base.insert(key, value);
                    }
                }
            }
        }
        (base, overlay) => *base = overlay,
    }
}
