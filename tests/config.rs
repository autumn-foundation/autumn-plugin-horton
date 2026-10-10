//! `[horton]` config resolution.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    reason = "tests"
)]

use std::path::PathBuf;

use autumn_plugin_horton::{HortonConfig, HortonError};
use autumn_web::config::MockEnv;

/// A project dir with `files`, and an `Env` that points at it.
fn project(files: &[(&str, &str)]) -> (tempfile::TempDir, MockEnv) {
    let dir = tempfile::tempdir().expect("tempdir");
    for (name, contents) in files {
        std::fs::write(dir.path().join(name), contents).expect("write");
    }
    let env = MockEnv::new().with("AUTUMN_MANIFEST_DIR", &dir.path().display().to_string());
    (dir, env)
}

#[test]
fn defaults() {
    let c = HortonConfig::default();
    assert!(c.enabled);
    assert_eq!(c.path, PathBuf::from("data/store.horton"));
    assert_eq!(c.size_mib, 256);
    assert!(c.sync);
    assert!(!c.sessions);
}

#[test]
fn no_file_gives_the_builder_values() {
    let (_dir, env) = project(&[]);
    let base = HortonConfig::default()
        .with_size_mib(100)
        .with_sessions(true);
    assert_eq!(base.clone().resolve_with_env(&env), Ok(base));
}

#[test]
fn the_file_overrides_the_builder_and_keeps_the_rest() {
    let (_dir, env) = project(&[(
        "autumn.toml",
        "[server]\nport = 3000\n\n[horton]\npath = \"var/kv.horton\"\nsync = false\n",
    )]);
    let c = HortonConfig::default()
        .with_size_mib(100)
        .resolve_with_env(&env)
        .expect("resolve");
    assert_eq!(c.path, PathBuf::from("var/kv.horton"));
    assert!(!c.sync);
    assert_eq!(
        c.size_mib, 100,
        "a key the file leaves out keeps the builder value"
    );
}

#[test]
fn profiles_and_env_layer_in_order() {
    let (_dir, env) = project(&[
        (
            "autumn.toml",
            "[horton]\nsize_mib = 100\n\n[profile.prod.horton]\nsize_mib = 128\nsessions = true\n",
        ),
        ("autumn-prod.toml", "[horton]\nsize_mib = 512\n"),
    ]);
    let dev = HortonConfig::default().resolve_with_env(&env).expect("dev");
    assert_eq!(dev.size_mib, 100);
    assert!(!dev.sessions);

    let prod_env = env.with("AUTUMN_ENV", "prod");
    let prod = HortonConfig::default()
        .resolve_with_env(&prod_env)
        .expect("prod");
    assert_eq!(
        prod.size_mib, 512,
        "the profile file wins over the inline profile"
    );
    assert!(prod.sessions);

    let env_wins = prod_env
        .with("AUTUMN_HORTON__SIZE_MIB", "1024")
        .with("AUTUMN_HORTON__ENABLED", "false")
        .with("AUTUMN_HORTON__PATH", "/srv/kv.horton")
        .with("AUTUMN_HORTON__SYNC", "0")
        .with("AUTUMN_HORTON__SESSIONS", "no");
    let c = HortonConfig::default()
        .resolve_with_env(&env_wins)
        .expect("env");
    assert_eq!(c.size_mib, 1024);
    assert!(!c.enabled);
    assert_eq!(c.path, PathBuf::from("/srv/kv.horton"));
    assert!(!c.sync);
    assert!(!c.sessions);
}

#[test]
fn a_typo_fails() {
    let (_dir, env) = project(&[("autumn.toml", "[horton]\nsize_mb = 64\n")]);
    assert!(matches!(
        HortonConfig::default().resolve_with_env(&env),
        Err(HortonError::Config(_))
    ));
}

#[test]
fn a_bad_type_or_value_fails() {
    for body in [
        "[horton]\nsize_mib = \"big\"\n",
        "[horton]\nsize_mib = 0\n",
        "[horton]\npath = \"\"\n",
        "[horton\n",
    ] {
        let (_dir, env) = project(&[("autumn.toml", body)]);
        assert!(
            matches!(
                HortonConfig::default().resolve_with_env(&env),
                Err(HortonError::Config(_))
            ),
            "{body}"
        );
    }
}

#[test]
fn a_bad_env_value_fails() {
    for (key, value) in [
        ("AUTUMN_HORTON__SIZE_MIB", "lots"),
        ("AUTUMN_HORTON__SYNC", "maybe"),
    ] {
        let (_dir, env) = project(&[]);
        let env = env.with(key, value);
        assert!(
            matches!(
                HortonConfig::default().resolve_with_env(&env),
                Err(HortonError::Config(_))
            ),
            "{key}={value}"
        );
    }
}

#[test]
fn a_disabled_store_skips_the_size_check() {
    let c = HortonConfig::default().with_enabled(false).with_size_mib(0);
    assert_eq!(c.validate(), Ok(()));
}

#[test]
fn the_size_must_hold_the_store() {
    let c = HortonConfig::default().with_size_mib(1);
    assert!(matches!(c.validate(), Err(HortonError::Config(_))));
    let min = HortonConfig::min_size_mib();
    assert_eq!(
        HortonConfig::default().with_size_mib(min).validate(),
        Ok(())
    );
}
