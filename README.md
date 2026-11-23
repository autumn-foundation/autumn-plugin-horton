# autumn-plugin-horton

An [Autumn](https://crates.io/crates/autumn-web) plugin for
[horton](https://github.com/madmax983/horton), an embedded LSM key-value
store. It gives your app a durable key-value store and a session store in
one local file. It needs no external service.

- **Durable.** A write returns after it is in the write-ahead log. With
  `sync = true` (the default) the log is on disk.
- **Async.** One store thread does all disk I/O. Tokio workers never
  block.
- **Sessions.** Optional. Sessions stay after a restart and expire after
  `session.max_age_secs`.
- **Observable.** A `horton` health indicator and `horton_*` Prometheus
  metrics.
- **Safe.** An OS lock stops a second process from opening the file. Key
  limits, value limits and batch limits are typed errors. A full store
  refuses new data but never locks up.

## Install

```toml
[dependencies]
autumn-plugin-horton = "0.1"
```

## Quick start

Condensed from [`examples/visits.rs`](examples/visits.rs), which also
counts visits per session. Run it with `cargo run --example visits`.

```rust,no_run
use autumn_plugin_horton::{Horton, HortonError, HortonPlugin};
use autumn_web::prelude::*;
use autumn_web::reexports::axum::extract::State;

#[get("/visits")]
async fn visits(State(state): State<AppState>) -> AutumnResult<String> {
    let Some(db) = state.extension::<Horton>() else {
        return Err(AutumnError::service_unavailable_msg("horton is not open"));
    };
    let count = db
        .get(b"visits")
        .await
        .map_err(HortonError::into_autumn)?
        .and_then(|v| <[u8; 8]>::try_from(v).ok())
        .map_or(0, u64::from_le_bytes)
        + 1;
    db.put(b"visits", &count.to_le_bytes())
        .await
        .map_err(HortonError::into_autumn)?;
    Ok(count.to_string())
}

#[autumn_web::main]
async fn main() {
    autumn_web::app()
        .plugin(HortonPlugin::new().path("target/visits.horton"))
        .routes(routes![visits])
        .run()
        .await;
}
```

## The `Horton` handle

Get it with `state.extension::<Horton>()`. It is `Clone + Send + Sync`.

| Method | What it does |
|---|---|
| `get(key)` | Reads one key. An expired value reads as `None`. |
| `put(key, val)` | Writes one key. |
| `put_with_ttl(key, val, ttl)` | Writes one key that expires after `ttl`. |
| `delete(key)` | Deletes one key. |
| `delete_range(start, end)` | Deletes every key in `[start, end)`. |
| `write(batch)` | Applies a `Batch` of puts and deletes atomically. |
| `scan(start, end, limit)` | Reads keys in `[start, end)`, ascending. |
| `scan_rev(start, end, limit)` | Reads keys in `[start, end)`, descending. |
| `flush()`, `compact()` | Runs maintenance now. The store also does it by itself. |
| `stats()`, `snapshot()` | Returns counters. `snapshot()` does not wait. |
| `close()` | Flushes and releases the file. The plugin calls it at shutdown. |

One scan returns at most `SCAN_MAX` (10,000) entries. To read more, start
the next scan after the last key. The store thread does one request at a
time, so keep pages small.

### Errors in handlers

`HortonError::status()` gives the HTTP status, and `into_autumn()` makes an
Autumn error with it. A bare `?` gives 500 for every error.

| Error | Status |
|---|---|
| `EmptyKey`, `ReservedKey` | 400 |
| `KeyTooLarge`, `ValueTooLarge`, `BatchTooLarge` | 413 |
| `StoreFull` | 507 |
| `NotStarted`, `Closed`, `Locked` | 503 |
| other | 500 |

## Configuration

Put the settings in `autumn.toml`:

```toml
[horton]
enabled = true                 # false: the plugin does nothing
path = "data/store.horton"     # the store file
size_mib = 256                 # size of a new file; it is sparse
sync = true                    # fdatasync every commit
sessions = false               # keep Autumn sessions in the store
cache_mib = 16                 # read cache in memory; 0 turns it off
```

The plugin reads these layers. A later layer wins:

1. The defaults above.
2. The builder: `HortonPlugin::new().path(..).size_mib(..).sync(..).sessions(..).cache_mib(..)`.
3. `[horton]` in `autumn.toml`.
4. `[profile.<p>.horton]` in `autumn.toml`.
5. `[horton]` in `autumn-<p>.toml`.
6. `AUTUMN_HORTON__ENABLED`, `__PATH`, `__SIZE_MIB`, `__SYNC`,
   `__SESSIONS` and `__CACHE_MIB`. `.env` files count.

`HortonPlugin::new().fixed_config(config)` uses `config` and reads no
file or variable. An unknown key or a bad value stops the app at
startup. A bad `AUTUMN_HORTON__*` value also stops it (Autumn core ignores
one); booleans accept `true`, `false`, `1`, `0`, `yes`, `no`, `on`, `off`.
The plugin does not see a misspelled variable name.

With `server.strict_config = true`, Autumn refuses
`[profile.<p>.horton]` in `autumn.toml`. Put profile values in
`autumn-<p>.toml` instead.

## Limits

| Limit | Value |
|---|---|
| Key | 1 to 128 bytes. A key must not start with byte `0x00`. |
| Value | 0 to 4096 bytes. A session's JSON must fit too. |
| Batch | One WAL block (16 KiB). Each op adds 23 bytes, or 31 with a TTL. |
| File | At least 86 MiB. Sparse on Linux and macOS, full size on Windows. An existing file keeps its size. |
| Processes | One process opens a file at a time. |
| Stores | One per app: a second `HortonPlugin` is skipped with a warning. |
| TTL | A value lives at least its TTL, and less than one second more. |

horton is before 1.0. A new minor version can change the file format.
horton then refuses the old file. It never misreads it.

### A full store

When 6 table slots are left (`RESERVE_SLOTS`), the store refuses writes
that add data with `StoreFull`. Reads and deletes still work. This keeps
room for compaction. With horton 0.18, deleted and expired data comes back
as free space only when compaction runs on its tables
([madmax983/horton#32](https://github.com/madmax983/horton/issues/32)).
Give the store a size with room to spare.

## Sessions

With `sessions = true` the plugin replaces Autumn's `[session] backend`
(it logs a warning when that backend is not `memory`).

- The store keeps the SHA-256 of each session id, not the id.
- A session expires `session.max_age_secs` after its last save.
- Every 60 s the plugin deletes expired sessions (`SWEEP_EVERY`). You can
  also call `HortonSessionStore::sweep()`.
- The JSON form of a session must fit 4096 bytes. A larger session fails
  to save: Autumn answers 503, and the plugin logs the size. Keep large
  data in the store, not in the session.
- A bad session cookie starts a new session.
- `TestApp` always uses memory sessions. Test the store with
  `HortonSessionStore` and `SessionLayer`, as `tests/session.rs` does.

## Operations

- **Restart.** Use stop, then start. Autumn's in-place upgrade (SIGUSR2)
  starts the new process while the old one holds the file lock, so the new
  process stops at startup and the old one keeps serving.
- **Files.** On Unix the plugin makes the store file with mode `0600` and
  new directories with mode `0700`. Block 0 marks the file as a plugin
  store; the plugin refuses any other file.
- **Clock.** The store clock is in Unix seconds and never goes back, also
  after a restart.

## Fleets

The `fleet` feature gives each tenant (or each slot) its own horton store.
Autumn core runs the fleet: keys, paths, opens, idle closes, the memory
budget, slot ownership and fencing. The plugin gives the horton engine and
the `FleetHorton` extractor. The fleet needs an Autumn database backend:
turn on the `postgres` or the `sqlite` feature of `autumn-web`.

```toml
[dependencies]
autumn-plugin-horton = { version = "0.1", features = ["fleet"] }
```

```rust,no_run
# #[cfg(feature = "fleet")]
# mod example {
use autumn_plugin_horton::HortonError;
use autumn_plugin_horton::fleet::{FleetHorton, HortonFleetPlugin};
use autumn_web::prelude::*;

#[get("/visits")]
async fn visits(store: FleetHorton) -> AutumnResult<String> {
    let seen = store
        .get(b"visits")
        .await
        .map_err(HortonError::into_autumn)?
        .and_then(|bytes| <[u8; 8]>::try_from(bytes.as_slice()).ok())
        .map_or(0, u64::from_be_bytes);
    let count = seen + 1;
    store
        .put(b"visits", &count.to_be_bytes())
        .await
        .map_err(HortonError::into_autumn)?;
    Ok(count.to_string())
}

pub async fn run() {
    autumn_web::app()
        .plugin(HortonFleetPlugin::new())
        .routes(routes![visits])
        .run()
        .await;
}
# }
# fn main() {}
```

The plugin reads `[horton_fleet]`. The top keys are the Autumn fleet keys,
as in `[database.fleet]`. `engine` has the `size_mib`, `sync` and
`cache_mib` keys of `[horton]`:

```toml
[horton_fleet]
mode = "tenant"                      # one store for each tenant; "slot" gives one for each slot
root = "data/tenants"
create_on_demand = true
max_resident_bytes = 1073741824      # the memory budget of all open stores
ship_every_secs = 60                 # the interval between two replica generations
resident_bytes = 16777216            # the memory that one open store counts

[horton_fleet.engine]
size_mib = 128                       # the size of a new store file (sparse on Linux and macOS)
sync = true
cache_mib = 1                        # the read cache of each open store; keep it below resident_bytes

[horton_fleet.ownership]
owner = "host-a"
```

`AUTUMN_HORTON_FLEET__ROOT` and `AUTUMN_HORTON_FLEET__OWNERSHIP__OWNER` set
`root` and `ownership.owner`.

- A fleet database is a directory with two files: `store.horton` and
  `identity`. `identity` holds the tenant key and the ownership epoch. A
  store of another key, or of a newer epoch, does not open.
- The extractor opens the store of the request tenant. It gives `400` with
  no tenant and `421` when another host owns the slot. In a job, use
  `FleetHorton::for_key(&state, tenant)`.
- A snapshot of an open store runs on the store thread. The thread flushes,
  finishes compaction, then copies the store file. A store with no write
  since the last snapshot gives the same bytes, so the fleet ships nothing.
  All-zero blocks stay holes in the copy.
- A fleet store never keeps sessions. Use `HortonPlugin` for sessions.
- The plugin closes idle stores, and all stores at shutdown, in each role.
- The engine passes the Autumn fleet conformance kit
  (`autumn_web::plugin_conformance::fleet`).

## Performance

One store thread serves every call. A read cache in front of horton
(`cache_mib`) answers repeat gets from memory. Writes go in group commits:
one WAL block and one `fdatasync` for many callers. See
[`docs/performance.md`](docs/performance.md) for the benchmark, the
numbers and the costs that are left in horton.

## Health and metrics

- `/actuator/health` shows `components.horton`. It is `UP` while the
  store thread runs. It is `DOWN` when the store is not open or has
  stopped. The indicator is in the readiness group.
- `/actuator/prometheus` shows `horton_up`, `horton_writes_total`,
  `horton_write_groups_total`, `horton_io_{reads,writes,syncs}_total`,
  `horton_flushes_total`, `horton_compaction_{jobs,steps}_total`,
  `horton_cache_{hits,misses}_total` (horton's block cache),
  `horton_read_cache_{hits,misses}_total`,
  `horton_read_cache_{entries,bytes}`, `horton_device_blocks`,
  `horton_slots{state}` and `horton_level_tables{level}`.

## Development

See [`CLAUDE.md`](CLAUDE.md) for the checks CI runs, and
[`docs/adr/`](docs/adr/) for the design decisions.

## License

MIT or Apache-2.0, at your option.
