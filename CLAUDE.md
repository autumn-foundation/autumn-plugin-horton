# autumn-plugin-horton

An Autumn 0.8 plugin for the horton key-value store. Read `README.md`
(use), `docs/plan.md` (plan and risks) and `docs/adr/` (decisions).

## Before you push: what CI runs

```sh
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings      # pedantic + nursery from Cargo.toml
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps
cargo test --all-targets && cargo test --doc   # Linux, macOS, Windows in CI
cargo +1.89 check --lib                        # MSRV
cargo llvm-cov --all-targets --fail-under-lines 85 --summary-only
cargo publish --dry-run
cargo build --example visits && scripts/smoke-sessions.sh   # real server, sessions
scripts/verify-verus.sh                        # Verus proofs (CI installs Verus)
```

`.github/workflows/ci.yml` is the source of truth.

## Rules the code keeps

- No disk I/O on an async task. Only the store thread (`src/engine.rs`)
  touches the `Db`.
- The store thread replies only after the write is in the WAL.
- No `unwrap`, `expect` or `panic!` outside tests.
- `src/ttl.rs`, `src/keys.rs` and `src/group.rs` mirror the Verus specs in
  `verification/`. Change both together.
- `src/device.rs` has `cfg(unix)` and `cfg(windows)` halves. Keep them in
  step.
- Every new behavior gets a test. Every critical invariant gets a spec.
- Docs and comments use ASD-STE100: short sentences, active voice.

## horton dependency

`horton = "0.18"` from crates.io. horton is before 1.0: a new minor
version can change the file format. Move the pin on purpose, and say so in
the CHANGELOG. madmax983/horton#32 (horton 0.19) tracks space reclaim in a
full store.
