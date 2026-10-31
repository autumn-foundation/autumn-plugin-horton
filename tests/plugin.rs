//! `HortonPlugin` mounted on an Autumn test app.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    reason = "tests"
)]

use autumn_plugin_horton::{Horton, HortonConfig, HortonPlugin};
use autumn_web::plugin::Plugin;
use autumn_web::test::TestApp;

fn plugin(dir: &tempfile::TempDir) -> HortonPlugin {
    HortonPlugin::new().fixed_config(
        HortonConfig::default()
            .with_path(dir.path().join("app.horton"))
            .with_size_mib(HortonConfig::min_size_mib())
            .with_sync(false),
    )
}

#[test]
fn the_contract_names_the_crate_and_autumn_0_9() {
    let contract = HortonPlugin::new().contract().expect("contract");
    assert_eq!(contract.plugin, "autumn-plugin-horton");
    assert_eq!(
        contract.plugin_version.as_deref(),
        Some(env!("CARGO_PKG_VERSION"))
    );
    assert_eq!(contract.autumn_web.as_deref(), Some("0.9"));
    assert_eq!(HortonPlugin::new().name(), "autumn-plugin-horton");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn startup_puts_a_working_handle_in_app_state() {
    let dir = tempfile::tempdir().expect("tempdir");
    let app = TestApp::new().plugin(plugin(&dir)).build();
    let db = app.state().extension::<Horton>().expect("handle");
    db.put(b"greeting", b"hello").await.expect("put");
    assert_eq!(db.get(b"greeting").await, Ok(Some(b"hello".to_vec())));
    assert!(dir.path().join("app.horton").exists());
    db.close().await.expect("close");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn health_reports_up_then_down() {
    let dir = tempfile::tempdir().expect("tempdir");
    // Autumn 0.9 caches health results for 1 s. The test reads twice.
    let mut config = autumn_web::config::AutumnConfig::default();
    config.health.cache_ttl_ms = 0;
    let app = TestApp::new().config(config).plugin(plugin(&dir)).build();
    let body = app.get("/actuator/health").send().await.text();
    let json: serde_json::Value = serde_json::from_str(&body).expect("json");
    assert_eq!(json["components"]["horton"]["status"], "UP", "{body}");

    let db = app.state().extension::<Horton>().expect("handle");
    db.close().await.expect("close");
    let body = app.get("/actuator/health").send().await.text();
    let json: serde_json::Value = serde_json::from_str(&body).expect("json");
    assert_eq!(json["components"]["horton"]["status"], "DOWN", "{body}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn metrics_export_horton_families() {
    let dir = tempfile::tempdir().expect("tempdir");
    let app = TestApp::new().plugin(plugin(&dir)).build();
    let db = app.state().extension::<Horton>().expect("handle");
    db.put(b"k", b"v").await.expect("put");
    let body = app.get("/actuator/prometheus").send().await.text();
    for family in [
        "horton_up 1",
        "horton_writes_total 1",
        "horton_io_writes_total",
        "horton_flushes_total",
        "horton_compaction_steps_total",
        "horton_slots{state=\"free\"}",
        "horton_level_tables{level=\"0\"}",
    ] {
        assert!(body.contains(family), "{family} missing in:\n{body}");
    }
    db.close().await.expect("close");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_disabled_plugin_does_nothing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let off = HortonPlugin::new().fixed_config(
        HortonConfig::default()
            .with_enabled(false)
            .with_path(dir.path().join("off.horton")),
    );
    let app = TestApp::new().plugin(off).build();
    assert!(app.state().extension::<Horton>().is_none());
    assert!(!dir.path().join("off.horton").exists());
    let body = app.get("/actuator/health").send().await.text();
    assert!(!body.contains("horton"), "{body}");
}

#[test]
#[should_panic(expected = "startup hook failed")]
fn a_bad_config_stops_startup() {
    let bad = HortonPlugin::new().fixed_config(HortonConfig::default().with_size_mib(1));
    let _app = TestApp::new().plugin(bad).build();
}

#[test]
#[should_panic(expected = "startup hook failed")]
fn a_locked_store_stops_startup() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = HortonConfig::default()
        .with_path(dir.path().join("locked.horton"))
        .with_size_mib(HortonConfig::min_size_mib())
        .with_sync(false);
    let _holder = Horton::open(&config).expect("open");
    let _app = TestApp::new()
        .plugin(HortonPlugin::new().fixed_config(config))
        .build();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sessions_on_opens_the_store_and_starts_the_sweep() {
    let dir = tempfile::tempdir().expect("tempdir");
    let on = HortonPlugin::new().fixed_config(
        HortonConfig::default()
            .with_path(dir.path().join("s.horton"))
            .with_size_mib(HortonConfig::min_size_mib())
            .with_sync(false)
            .with_sessions(true),
    );
    let app = TestApp::new().plugin(on).build();
    let db = app.state().extension::<Horton>().expect("handle");
    // Session keys are plugin keys: the app sees none.
    assert_eq!(db.scan(b"", None, 10).await, Ok(Vec::new()));
    db.close().await.expect("close");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_builder_path_resolves_files_and_env() {
    // No `autumn.toml` in the crate root and no AUTUMN_HORTON__* in CI:
    // the builder values win.
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("b.horton");
    let plugin = HortonPlugin::new()
        .path(&path)
        .size_mib(HortonConfig::min_size_mib())
        .sync(false);
    let app = TestApp::new().plugin(plugin).build();
    let db = app.state().extension::<Horton>().expect("handle");
    assert!(path.exists());
    db.close().await.expect("close");
}
