//! `HortonSessionStore`: the trait contract and Autumn's real session layer.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    reason = "tests"
)]

use std::collections::HashMap;
use std::time::Duration;

use autumn_plugin_horton::{Horton, HortonConfig, HortonSessionStore};
use autumn_web::AppState;
use autumn_web::reexports::axum::{Router, routing::get};
use autumn_web::session::{Session, SessionConfig, SessionLayer, SessionStore};
use autumn_web::test::{TestApp, TestClient};

fn config(dir: &tempfile::TempDir) -> HortonConfig {
    HortonConfig::default()
        .with_path(dir.path().join("sessions.horton"))
        .with_size_mib(HortonConfig::min_size_mib())
        .with_sync(false)
}

fn data(pairs: &[(&str, &str)]) -> HashMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect()
}

#[tokio::test]
async fn save_load_destroy() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = Horton::open(&config(&dir)).expect("open");
    let store = HortonSessionStore::new(db.clone(), Duration::from_secs(3600));
    assert_eq!(store.load("abc").await, Ok(None));
    store
        .save("abc", data(&[("user", "42"), ("role", "admin")]))
        .await
        .expect("save");
    assert_eq!(
        store.load("abc").await,
        Ok(Some(data(&[("user", "42"), ("role", "admin")])))
    );
    store.destroy("abc").await.expect("destroy");
    assert_eq!(store.load("abc").await, Ok(None));
    db.close().await.expect("close");
}

#[tokio::test]
async fn sessions_never_show_in_app_scans_or_reads() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = Horton::open(&config(&dir)).expect("open");
    let store = HortonSessionStore::new(db.clone(), Duration::from_secs(3600));
    store.save("s1", data(&[("k", "v")])).await.expect("save");
    db.put(b"app", b"v").await.expect("put");
    let keys: Vec<_> = db
        .scan(b"", None, 100)
        .await
        .expect("scan")
        .into_iter()
        .map(|(k, _)| k)
        .collect();
    assert_eq!(keys, vec![b"app".to_vec()]);
    let rev = db.scan_rev(b"", None, 100).await.expect("scan_rev");
    assert_eq!(rev.len(), 1);
    // An app range delete over everything cannot reach a session.
    db.delete_range(&[1], &[0xff]).await.expect("delete_range");
    assert_eq!(store.load("s1").await, Ok(Some(data(&[("k", "v")]))));
    db.close().await.expect("close");
}

#[tokio::test]
async fn bad_ids_and_large_sessions_are_errors() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = Horton::open(&config(&dir)).expect("open");
    let store = HortonSessionStore::new(db.clone(), Duration::from_secs(60));
    assert!(store.load("").await.is_err());
    assert!(store.load(&"x".repeat(200)).await.is_err());
    let big = data(&[("blob", &"x".repeat(5000))]);
    assert!(store.save("big", big).await.is_err());
    db.close().await.expect("close");
    assert!(
        store.load("abc").await.is_err(),
        "a closed store is an error"
    );
}

#[tokio::test]
async fn sessions_expire_after_max_age() {
    let dir = tempfile::tempdir().expect("tempdir");
    let now = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(100));
    let read = std::sync::Arc::clone(&now);
    let db = Horton::open_with_clock(
        &config(&dir),
        std::sync::Arc::new(move || read.load(std::sync::atomic::Ordering::SeqCst)),
    )
    .expect("open");
    let store = HortonSessionStore::new(db.clone(), Duration::from_secs(30));
    store.save("s", data(&[("k", "v")])).await.expect("save");
    now.store(129, std::sync::atomic::Ordering::SeqCst);
    assert!(store.load("s").await.expect("load").is_some());
    now.store(130, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(store.load("s").await, Ok(None));
    db.close().await.expect("close");
}

async fn bump(session: Session) -> String {
    let n: u32 = session
        .get("n")
        .await
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
        + 1;
    session.insert("n", n.to_string()).await;
    n.to_string()
}

fn client(store: HortonSessionStore) -> TestClient {
    let router = Router::new()
        .route("/bump", get(bump))
        .layer(SessionLayer::new(store, SessionConfig::default()));
    TestApp::from_router(router, AppState::for_test())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn autumn_sessions_survive_a_restart() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = Horton::open(&config(&dir)).expect("open");
    let app = client(HortonSessionStore::new(
        db.clone(),
        Duration::from_secs(3600),
    ));
    let first = app.get("/bump").send().await;
    assert_eq!(first.text(), "1");
    let cookie = first
        .header("set-cookie")
        .and_then(|c| c.split(';').next())
        .expect("session cookie")
        .to_owned();
    assert_eq!(app.get("/bump").send().await.text(), "2");
    db.close().await.expect("close");

    // A new store over the same file: the old cookie still works.
    let db = Horton::open(&config(&dir)).expect("reopen");
    let app = client(HortonSessionStore::new(
        db.clone(),
        Duration::from_secs(3600),
    ));
    let third = app.get("/bump").header("cookie", &cookie).send().await;
    assert_eq!(third.text(), "3");
    db.close().await.expect("close");
}
