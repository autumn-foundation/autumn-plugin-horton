# Changelog

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).
The crate follows [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Performance

- A read cache on the store thread (`cache_mib`, default 16 MiB; 1 MiB for
  each fleet store; 0 turns it off). A repeat get does not read horton.
  Writes update the cache. Verus proves the cache rules; a property test
  checks every get against a store without the cache. See ADR 0006.
- The store thread serves a get and up to three queued gets, then
  publishes the stats once and replies. Fewer runtime wake-ups.
- The idle store thread polls the channel for 50 µs before it sleeps (not
  on a one-CPU machine). One caller in a loop waits about half as long.
- The stats publish updates the shared copy in place, and only once for
  each request.
- A get no longer allocates and zeroes a 4 KiB buffer. A returned value
  now has the capacity of its length, not 4 KiB.
- `benches/throughput.rs`: Criterion benchmarks for put, put with sync,
  get from the memtable, from tables and through the cache, and scans.
  `docs/performance.md` has the audit and the numbers.

### Added

- `HortonConfig::cache_mib`, `with_cache_mib`, `cache_bytes`,
  `HortonPlugin::cache_mib`, `AUTUMN_HORTON__CACHE_MIB` and the fleet key
  `engine.cache_mib`.
- `Stats::read_cache_{hits,misses,entries,bytes}` and the metrics
  `horton_read_cache_{hits,misses}_total` and
  `horton_read_cache_{entries,bytes}`.

### Changed

- Support `autumn-web` 0.11. The plugin now needs `autumn-web` 0.11 and
  Rust 1.97.

### Added

- The `fleet` feature: one horton store for each tenant or slot, on the
  Autumn database fleet. `HortonFleetPlugin` reads `[horton_fleet]`, and
  `FleetHorton` is the extractor. A snapshot is a copy of the store file
  that the store thread makes after a flush and a full compaction. The
  engine passes the Autumn fleet conformance kit, with replication.
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
- Verus proofs for TTL expiry, the store clock, the purge limit, key
  namespaces and group-commit sizing.
- A store quota: at `RESERVE_SLOTS` free slots, writes that add data fail
  with `StoreFull`. Reads and deletes still work.
- A session sweep (`HortonSessionStore::sweep`, every `SWEEP_EVERY`).
- `HortonError::status` and `HortonError::into_autumn`.
- The store keeps the SHA-256 of each session id, not the id.
- The store clock is on disk and never goes back after a restart.
