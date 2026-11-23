# autumn-plugin-horton

An Autumn 0.11 plugin for the horton key-value store. Read `README.md`
(use), `docs/plan.md` (plan and risks) and `docs/adr/` (decisions).

## Before you push: what CI runs

```sh
cargo fmt --all --check
cargo +stable clippy --all-targets -- -D warnings   # CI uses stable: new lints land there
cargo +stable clippy --all-targets --features fleet -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --features fleet,autumn-web/sqlite
cargo test --all-targets --features fleet && cargo test --doc --features fleet   # Linux, macOS, Windows in CI
cargo +1.97 check --lib                        # MSRV
cargo +1.97 check --lib --features fleet,autumn-web/sqlite
cargo llvm-cov --all-targets --features fleet --fail-under-lines 85 --summary-only
cargo publish --dry-run
cargo build --example visits && scripts/smoke-sessions.sh   # real server, sessions
scripts/verify-verus.sh                        # Verus proofs (CI installs Verus)
```

`.github/workflows/ci.yml` is the source of truth.

## Rules the code keeps

- README Rust snippets are doctests. Make them compile; use `no_run`, not `ignore`.
  `tests/doc_snippets.rs` checks the TOML snippets and the version pins in the docs.
- No disk I/O on an async task. Only the store thread (`src/engine.rs`)
  touches the `Db`.
- The store thread replies only after the write is in the WAL.
- No `unwrap`, `expect` or `panic!` outside tests.
- `src/ttl.rs`, `src/keys.rs`, `src/group.rs` and `src/read_cache.rs`
  mirror the Verus specs in `verification/`. Change both together.
- Every engine path that changes horton data also updates the read cache
  (`Engine::cache_writes`, `delete_range`); a failed write clears it.
- `src/fleet.rs` (feature `fleet`) is the Autumn fleet engine. A fleet
  database is a directory: `store.horton` and the `identity` stamp file
  (ADR 0005). A snapshot runs on the store thread (`Request::Snapshot`).
- `src/device.rs` has `cfg(unix)` and `cfg(windows)` halves. Keep them in
  step.
- Every new behavior gets a test. Every critical invariant gets a spec.
- Docs and comments use ASD-STE100: short sentences, active voice.

## horton dependency

`horton = "0.18"` from crates.io. horton is before 1.0: a new minor
version can change the file format. Move the pin on purpose, and say so in
the CHANGELOG. madmax983/horton#32 (horton 0.19) tracks space reclaim in a
full store.
