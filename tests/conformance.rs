//! The plugin conformance checks Autumn runs for its plugin index.
//!
//! The plugin mounts no routes. Autumn's library harness has no
//! "no routes" option, so `route-attribution` fails here by design; the CLI
//! passes it with `autumn plugin-check --no-routes`. Every other check
//! must pass or skip.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    reason = "tests"
)]

use autumn_plugin_horton::{HortonConfig, HortonPlugin, NAME};
use autumn_web::plugin::Plugin;
use autumn_web::plugin_conformance::{CheckStatus, ConformanceConfig, run_conformance};
use autumn_web::plugin_contract::{AUTUMN_WEB_VERSION, ContractVerdict, evaluate};

fn plugin() -> HortonPlugin {
    HortonPlugin::new().fixed_config(HortonConfig::default().with_path("target/conformance.horton"))
}

#[test]
fn the_contract_admits_this_autumn_web() {
    let contract = plugin().contract().expect("contract");
    assert!(matches!(
        evaluate(&contract, AUTUMN_WEB_VERSION),
        ContractVerdict::Compatible
    ));
}

#[test]
fn the_plugin_mounts_no_routes_and_opens_nothing_at_build() {
    let app = autumn_web::app().plugin(plugin());
    let routes = app.plugin_route_infos().expect("routes");
    assert!(
        routes.iter().all(|r| !format!("{r:?}").contains(NAME)),
        "{routes:?}"
    );
    assert!(!std::path::Path::new("target/conformance.horton").exists());
}

#[test]
fn every_check_but_route_attribution_passes() {
    let app = autumn_web::app().plugin(plugin());
    let routes = app.plugin_route_infos().expect("routes");
    let config = ConformanceConfig::new(NAME).contract(plugin().contract().expect("contract"));
    let report = run_conformance(&config, &routes);
    for check in &report.checks {
        if check.name == "route-attribution" {
            continue;
        }
        assert_ne!(
            check.status,
            CheckStatus::Fail,
            "{}: {}",
            check.name,
            check.message
        );
    }
}
