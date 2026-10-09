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
scripts/verify-verus.sh                        # Verus proofs (CI installs Verus)
```

`.github/workflows/ci.yml` is the source of truth.

## Rules the code keeps

- No disk I/O on an async task. Only the store thread (`src/engine.rs`)
  touches the `Db`.
- A reply goes out only after the write is in the WAL.
- No `unwrap`, `expect` or `panic!` outside tests.
- `src/ttl.rs`, `src/keys.rs` and `src/group.rs` mirror the Verus specs in
  `verification/`. Change both together.
- `src/device.rs` has `cfg(unix)` and `cfg(windows)` halves. Keep them in
  step.
- Every new behavior gets a test. Every critical invariant gets a spec.
- Docs and comments use ASD-STE100: short sentences, active voice.

## horton dependency

Until horton 0.17 is on crates.io, `Cargo.toml` pins its release commit on
GitHub. After the release, change it to `horton = "0.17"`. Then
`cargo publish --dry-run` passes.
