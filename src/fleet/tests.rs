#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::HashMap;

use autumn_web::config::{DatabaseConfig, DatabaseFleetConfig};
use autumn_web::db::fleet_replication::FleetReplication;
use autumn_web::db::fleet_tombstones::FleetTombstones;
use autumn_web::db::{RuntimeConnection, create_pool};
use autumn_web::plugin_conformance::CheckStatus;
use autumn_web::plugin_conformance::fleet::{FleetFixture, run_fleet_conformance};
use autumn_web::reexports::diesel_async::pooled_connection::deadpool::Pool;
use autumn_web::replication::{FileDestination, ReplicaDestination, ReplicationSettings};
use axum::extract::FromRequestParts as _;

use super::*;

/// The kit fixture. With `replicated`, generations go to a local directory.
struct Fixture {
    replicated: bool,
}

/// The fixture with no replica.
const PLAIN: Fixture = Fixture { replicated: false };

/// The key prefix of the kit values.
const KV: &[u8] = b"kv/";

fn control_pool(dir: &Path) -> Pool<RuntimeConnection> {
    create_pool(&DatabaseConfig {
        url: Some(format!("sqlite://{}", dir.join("control.db").display())),
        ..Default::default()
    })
    .unwrap()
    .unwrap()
}

fn small_engine() -> HortonEngine {
    HortonEngine::new(HortonConfig {
        size_mib: HortonConfig::min_size_mib(),
        sync: false,
        ..HortonConfig::default()
    })
}

impl FleetFixture for Fixture {
    type Engine = HortonEngine;

    // The trait needs an `async fn`. This body has no `.await`, and Clippy
    // flags that. Older Clippy releases do not know the lint.
    #[allow(
        unknown_lints,
        clippy::unused_async_trait_impl,
        reason = "the FleetFixture trait sets the signature"
    )]
    async fn fleet(
        &self,
        config: DatabaseFleetConfig,
    ) -> Result<DatabaseFleet<HortonEngine>, String> {
        let scratch = Path::new(&config.root)
            .parent()
            .ok_or("the root has no parent")?
            .to_path_buf();
        let engine = small_engine()
            .resident_bytes(1024)
            .ship_every(Duration::from_hours(1));
        let fleet = DatabaseFleet::with_engine(&config, engine).map_err(|e| e.to_string())?;
        if self.replicated {
            let destination: Arc<dyn ReplicaDestination> = Arc::new(
                FileDestination::new(scratch.join("replicas")).map_err(|e| e.to_string())?,
            );
            let clock = Arc::new(autumn_web::time::SystemClock);
            let tombstones = Arc::new(FleetTombstones::new(
                control_pool(&scratch),
                Arc::clone(&clock) as _,
            ));
            let replication = Arc::new(
                FleetReplication::new(
                    destination,
                    ReplicationSettings {
                        database_path: PathBuf::new(),
                        root: "app/kit".to_owned(),
                        sync_interval: Duration::from_secs(1),
                        snapshot_interval: Duration::from_hours(1),
                        max_wal_bytes: 4 * 1024 * 1024,
                        retention: Duration::from_hours(24),
                        verify_interval: None,
                    },
                    Duration::from_secs(30),
                    false,
                    clock,
                )
                .with_tombstones(tombstones),
            );
            fleet
                .install_replication(replication)
                .map_err(|_| "replication is installed twice".to_owned())?;
        }
        Ok(fleet)
    }

    async fn restore(
        &self,
        fleet: &DatabaseFleet<HortonEngine>,
        key: &FleetDbKey,
    ) -> Option<Result<(), FleetError>> {
        if self.replicated {
            Some(fleet.restore_latest(key).await)
        } else {
            None
        }
    }

    async fn write(&self, handle: &Horton, value: &str) -> Result<(), String> {
        let count = handle
            .scan(KV, Some(b"kv0"), SCAN_LIMIT)
            .await
            .map_err(|e| e.to_string())?
            .len();
        let key = format!("kv/{count:08}");
        handle
            .put(key.as_bytes(), value.as_bytes())
            .await
            .map_err(|e| e.to_string())
    }

    async fn read(&self, handle: &Horton) -> Result<Vec<String>, String> {
        let rows = handle
            .scan(KV, Some(b"kv0"), SCAN_LIMIT)
            .await
            .map_err(|e| e.to_string())?;
        Ok(rows
            .into_iter()
            .map(|(_, v)| String::from_utf8_lossy(&v).into_owned())
            .collect())
    }
}

/// The most kit values one scan reads.
const SCAN_LIMIT: usize = 256;

#[tokio::test(flavor = "multi_thread")]
async fn the_engine_passes_the_fleet_conformance_kit() {
    let scratch = tempfile::tempdir().unwrap();
    let report = run_fleet_conformance("horton", &PLAIN, scratch.path()).await;
    assert!(report.passed(), "{}", report.to_text_report());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_replicated_engine_passes_every_check() {
    let scratch = tempfile::tempdir().unwrap();
    let fixture = Fixture { replicated: true };
    let report = run_fleet_conformance("horton", &fixture, scratch.path()).await;
    assert!(report.passed(), "{}", report.to_text_report());
    for check in &report.checks {
        assert_eq!(
            check.status,
            CheckStatus::Pass,
            "{}: {}",
            check.name,
            check.message
        );
    }
}

fn fleet_config(root: &Path) -> DatabaseFleetConfig {
    let mut table = toml::Table::new();
    table.insert("mode".to_owned(), "tenant".into());
    table.insert("root".to_owned(), root.display().to_string().into());
    table.insert("create_on_demand".to_owned(), true.into());
    toml::Value::Table(table).try_into().unwrap()
}

fn key(fleet: &DatabaseFleet<HortonEngine>, tenant: &str) -> FleetDbKey {
    fleet.key_for(tenant).unwrap()
}

fn contents(files: &[PathBuf]) -> Vec<(String, Vec<u8>)> {
    files
        .iter()
        .map(|f| {
            (
                f.file_name().unwrap().to_string_lossy().into_owned(),
                std::fs::read(f).unwrap(),
            )
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unchanged_database_snapshots_to_the_same_bytes() {
    let scratch = tempfile::tempdir().unwrap();
    let fleet = PLAIN
        .fleet(fleet_config(&scratch.path().join("fleet")))
        .await
        .unwrap();
    let key = key(&fleet, "acme");
    let db = fleet.open(&key).await.unwrap();
    PLAIN.write(db.handle(), "one").await.unwrap();
    let path = fleet.path_of(&key);
    let snap = |n: &str| {
        let dir = scratch.path().join(n);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    };
    let engine = fleet.engine();
    let first = engine
        .snapshot(db.handle(), &path, &snap("a"))
        .await
        .unwrap();
    let second = engine
        .snapshot(db.handle(), &path, &snap("b"))
        .await
        .unwrap();
    assert_eq!(first.len(), 2);
    assert_eq!(contents(&first), contents(&second));
    PLAIN.write(db.handle(), "two").await.unwrap();
    let third = engine
        .snapshot(db.handle(), &path, &snap("c"))
        .await
        .unwrap();
    assert_ne!(contents(&first), contents(&third));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_closed_database_snapshots_its_files() {
    let scratch = tempfile::tempdir().unwrap();
    let fleet = PLAIN
        .fleet(fleet_config(&scratch.path().join("fleet")))
        .await
        .unwrap();
    let key = key(&fleet, "acme");
    let db = fleet.open(&key).await.unwrap();
    PLAIN.write(db.handle(), "kept").await.unwrap();
    let handle = db.handle().clone();
    drop(db);
    fleet.close(&key).await;
    assert!(handle.is_closed());
    let dir = scratch.path().join("copy");
    std::fs::create_dir_all(&dir).unwrap();
    let path = fleet.path_of(&key);
    let files = fleet.engine().snapshot(&handle, &path, &dir).await.unwrap();
    assert_eq!(files.len(), 2);
    assert_eq!(
        std::fs::read(&files[1]).unwrap(),
        std::fs::read(path.join(STORE_FILE)).unwrap()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_epoch_stamp_is_the_stamp_of_the_open() {
    let scratch = tempfile::tempdir().unwrap();
    let fleet =
        DatabaseFleet::with_engine(&fleet_config(&scratch.path().join("f")), small_engine())
            .unwrap();
    let key = key(&fleet, "acme");
    let path = fleet.path_of(&key);
    assert_eq!(fleet.engine().epoch_stamp(&key, &path).unwrap(), None);
    assert!(!fleet.engine().exists(&path));
    let token = FencingToken::try_from(7_i64).unwrap();
    let db = fleet.engine().open(&key, &path, Some(token)).await.unwrap();
    assert!(fleet.engine().exists(&path));
    fleet.engine().close(&db);
    drop(db);
    assert_eq!(fleet.engine().epoch_stamp(&key, &path).unwrap(), Some(7));
    let older = FencingToken::try_from(3_i64).unwrap();
    let err = fleet
        .engine()
        .open(&key, &path, Some(older))
        .await
        .unwrap_err();
    assert!(
        matches!(err, FleetError::NewerEpoch { found: 7, .. }),
        "{err}"
    );
    // A newer epoch raises the stamp.
    let newer = FencingToken::try_from(9_i64).unwrap();
    let db = fleet.engine().open(&key, &path, Some(newer)).await.unwrap();
    fleet.engine().close(&db);
    assert_eq!(fleet.engine().epoch_stamp(&key, &path).unwrap(), Some(9));
    assert!(fleet.engine().remove(&key, &path).unwrap());
    assert!(!fleet.engine().remove(&key, &path).unwrap());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_database_of_another_key_is_refused() {
    let scratch = tempfile::tempdir().unwrap();
    let fleet =
        DatabaseFleet::with_engine(&fleet_config(&scratch.path().join("f")), small_engine())
            .unwrap();
    let (acme, other) = (key(&fleet, "acme"), key(&fleet, "other"));
    let path = fleet.path_of(&acme);
    let db = fleet.engine().open(&acme, &path, None).await.unwrap();
    fleet.engine().close(&db);
    let err = fleet.engine().open(&other, &path, None).await.unwrap_err();
    assert!(matches!(err, FleetError::Misplaced { .. }), "{err}");
    // The refused open released the store lock.
    let db = fleet.engine().open(&acme, &path, None).await.unwrap();
    fleet.engine().close(&db);
}

#[test]
fn a_bad_stamp_gives_an_epoch_stamp_error() {
    let scratch = tempfile::tempdir().unwrap();
    std::fs::write(scratch.path().join(IDENTITY_FILE), b"not json").unwrap();
    let fleet = DatabaseFleet::with_engine(&fleet_config(scratch.path()), small_engine()).unwrap();
    let key = key(&fleet, "acme");
    let err = fleet
        .engine()
        .epoch_stamp(&key, scratch.path())
        .unwrap_err();
    assert!(matches!(err, FleetError::Io { .. }), "{err}");
}

#[test]
fn the_engine_reports_its_settings() {
    let engine = small_engine()
        .ship_every(Duration::from_secs(5))
        .resident_bytes(2048);
    let text = format!("{engine:?}");
    assert!(text.contains("resident_bytes: 2048"), "{text}");
    assert!(matches!(
        engine.ship_mode(),
        ShipMode::Generations { every } if every == Duration::from_secs(5)
    ));
    assert_eq!(engine.sidecars(Path::new("x")), Vec::<PathBuf>::new());
}

struct MapEnv(HashMap<String, String>);

impl Env for MapEnv {
    fn var(&self, key: &str) -> Result<String, std::env::VarError> {
        self.0
            .get(key)
            .cloned()
            .ok_or(std::env::VarError::NotPresent)
    }
}

fn table(text: &str) -> toml::Table {
    text.parse().unwrap()
}

#[test]
fn the_config_reads_the_fleet_and_the_engine_keys() {
    let config = HortonFleetConfig::from_table(
        "horton_fleet",
        table(
            r#"
            mode = "tenant"
            root = "data/tenants"
            max_resident_bytes = 4096
            ship_every_secs = 5
            resident_bytes = 1024

            [engine]
            size_mib = 100
            sync = false
            cache_mib = 3
            "#,
        ),
    )
    .unwrap();
    assert_eq!(config.fleet.root, "data/tenants");
    assert_eq!(config.ship_every, Duration::from_secs(5));
    assert_eq!(config.resident_bytes, 1024);
    assert_eq!(config.engine.size_mib, 100);
    assert!(!config.engine.sync);
    assert_eq!(config.engine.cache_mib, 3);
    let engine = config.engine();
    assert_eq!(engine.ship_every, Duration::from_secs(5));
    let defaults =
        HortonFleetConfig::from_table("horton_fleet", table("mode = \"tenant\"\nroot = \"r\""))
            .unwrap();
    assert_eq!(defaults.engine.size_mib, DEFAULT_FLEET_SIZE_MIB);
    assert_eq!(defaults.engine.cache_mib, DEFAULT_FLEET_CACHE_MIB);
    const { assert!(DEFAULT_FLEET_CACHE_MIB * 1024 * 1024 < DEFAULT_RESIDENT_BYTES) };
    assert_eq!(defaults.resident_bytes, DEFAULT_RESIDENT_BYTES);
}

#[test]
fn the_config_refuses_bad_values() {
    let cases = [
        ("mode = \"tenant\"\nroot = \"\"", "root"),
        (
            "mode = \"tenant\"\nroot = \"r\"\nresident_bytes = 0",
            "resident_bytes",
        ),
        (
            "mode = \"tenant\"\nroot = \"r\"\nresident_bytes = -1",
            "resident_bytes",
        ),
        (
            "mode = \"tenant\"\nroot = \"r\"\nship_every_secs = 0",
            "ship_every_secs",
        ),
        (
            "mode = \"tenant\"\nroot = \"r\"\nship_every_secs = \"x\"",
            "ship_every_secs",
        ),
        ("mode = \"tenant\"\nroot = \"r\"\nengine = 3", "engine"),
        (
            "mode = \"tenant\"\nroot = \"r\"\n[engine]\npath = \"x\"",
            "engine",
        ),
        (
            "mode = \"tenant\"\nroot = \"r\"\n[engine]\nsize_mib = 0",
            "size_mib",
        ),
        ("mode = \"nope\"\nroot = \"r\"", "fleet"),
    ];
    for (text, key) in cases {
        let err = HortonFleetConfig::from_table("horton_fleet", table(text)).unwrap_err();
        assert!(err.to_string().contains(key), "{text}: {err}");
    }
}

#[test]
fn variables_set_the_root_and_the_owner() {
    let scratch = tempfile::tempdir().unwrap();
    std::fs::write(
        scratch.path().join("autumn.toml"),
        "[horton_fleet]\nmode = \"tenant\"\nroot = \"from-file\"\n",
    )
    .unwrap();
    let env = MapEnv(HashMap::from([
        (
            "AUTUMN_MANIFEST_DIR".to_owned(),
            scratch.path().display().to_string(),
        ),
        (
            "AUTUMN_HORTON_FLEET__ROOT".to_owned(),
            "from-env".to_owned(),
        ),
        (
            "AUTUMN_HORTON_FLEET__OWNERSHIP__OWNER".to_owned(),
            "host-b".to_owned(),
        ),
    ]));
    let config = HortonFleetConfig::resolve_with_env("horton_fleet", &env).unwrap();
    assert_eq!(config.fleet.root, "from-env");
    assert_eq!(config.fleet.ownership.unwrap().owner, "host-b");
    let env = MapEnv(HashMap::from([(
        "AUTUMN_MANIFEST_DIR".to_owned(),
        scratch.path().display().to_string(),
    )]));
    let config = HortonFleetConfig::resolve_with_env("horton_fleet", &env).unwrap();
    assert_eq!(config.fleet.root, "from-file");
}

#[test]
fn the_plugin_resolves_an_explicit_config() {
    let good = HortonFleetConfig::new(fleet_config(Path::new("data")));
    assert!(HortonFleetPlugin::resolve(&FleetSource::Explicit(Box::new(good.clone()))).is_ok());
    let mut bad = good.clone();
    bad.resident_bytes = 0;
    assert!(HortonFleetPlugin::resolve(&FleetSource::Explicit(Box::new(bad))).is_err());
    let plugin = HortonFleetPlugin::default().config_section("tenants");
    let text = format!("{plugin:?}");
    assert!(text.contains("tenants"), "{text}");
    assert_eq!(plugin.name(), FLEET_PLUGIN_NAME);
    let explicit = format!("{:?}", HortonFleetPlugin::new().config(good));
    assert!(explicit.contains("(explicit)"), "{explicit}");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_runtime_closes_every_database_at_shutdown() {
    let scratch = tempfile::tempdir().unwrap();
    let mut config = HortonFleetConfig::new(fleet_config(&scratch.path().join("fleet")));
    config.engine.size_mib = HortonConfig::min_size_mib();
    let state = AppState::for_test();
    let runtime = start(&state, &config, config.engine()).await.unwrap();
    let lease = FleetHorton::for_key(&state, "acme").await.unwrap();
    lease.put(b"k", b"v").await.unwrap();
    let handle = (*lease).clone();
    assert_eq!(runtime.fleet.open_keys().len(), 1);
    assert!(!runtime.fleet.engine().in_use(&handle));
    drop(lease);
    runtime.shutdown().await;
    assert!(handle.is_closed());
}

#[tokio::test(flavor = "multi_thread")]
async fn the_extractor_opens_the_store_of_the_request_key() {
    let scratch = tempfile::tempdir().unwrap();
    let mut config = HortonFleetConfig::new(fleet_config(&scratch.path().join("fleet")));
    config.engine.size_mib = HortonConfig::min_size_mib();
    let state = AppState::for_test();
    let runtime = start(&state, &config, config.engine()).await.unwrap();

    let (mut parts, ()) = http::Request::new(()).into_parts();
    parts
        .extensions
        .insert(autumn_web::sharding::ShardKeyOverride("acme".to_owned()));
    let store = FleetHorton::from_request_parts(&mut parts, &state)
        .await
        .unwrap();
    store.put(b"k", b"v").await.unwrap();
    assert_eq!(store.get(b"k").await.unwrap(), Some(b"v".to_vec()));
    drop(store);

    // With no key and no config, the request gets a 503.
    let (mut parts, ()) = http::Request::new(()).into_parts();
    let err = FleetHorton::from_request_parts(&mut parts, &state)
        .await
        .err()
        .unwrap();
    assert_eq!(err.status(), http::StatusCode::SERVICE_UNAVAILABLE);
    runtime.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_extractor_needs_a_fleet() {
    let state = AppState::for_test();
    let (mut parts, ()) = http::Request::new(()).into_parts();
    parts
        .extensions
        .insert(autumn_web::sharding::ShardKeyOverride("acme".to_owned()));
    let err = FleetHorton::from_request_parts(&mut parts, &state)
        .await
        .err()
        .unwrap();
    assert_eq!(err.status(), http::StatusCode::SERVICE_UNAVAILABLE);
    assert!(FleetHorton::for_key(&state, "acme").await.is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn ownership_needs_a_store() {
    let scratch = tempfile::tempdir().unwrap();
    let mut table = toml::Table::new();
    table.insert("mode".to_owned(), "tenant".into());
    table.insert(
        "root".to_owned(),
        scratch.path().display().to_string().into(),
    );
    let mut ownership = toml::Table::new();
    ownership.insert("owner".to_owned(), "host-a".into());
    table.insert("ownership".to_owned(), ownership.into());
    let config = HortonFleetConfig::new(toml::Value::Table(table).try_into().unwrap());
    let state = AppState::for_test();
    let err = start(&state, &config, config.engine()).await.err().unwrap();
    assert!(err.contains("ownership"), "{err}");
}
