# Changelog

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).
The crate follows [Semantic Versioning](https://semver.org/).

## [Unreleased]

## [0.1.0]

### Added

- `HortonPlugin`: opens a horton store at startup, puts a `Horton` handle
  in `AppState`, and closes the store at shutdown. It declares
  `autumn_web = "0.8"`.
- `Horton`: an async, cloneable handle. `get`, `put`, `put_with_ttl`,
  `delete`, `delete_range`, `write` (atomic `Batch`), `scan`, `scan_rev`,
  `flush`, `compact`, `stats`, `snapshot` and `close`.
- One store thread does all disk I/O, with group commit, automatic flush
  and background compaction.
- TTL values on a monotonic store clock. Compaction purges expired values.
- `HortonSessionStore`: Autumn sessions in the store (`sessions = true`).
- The `horton` health indicator and the `horton_*` metrics.
- `[horton]` config with profile layers and `AUTUMN_HORTON__*` variables.
- An exclusive file lock: a second open of one store fails.
- Verus proofs for TTL expiry, the store clock, key namespaces and
  group-commit sizing.
