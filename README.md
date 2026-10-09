# autumn-plugin-horton

An [Autumn](https://crates.io/crates/autumn-web) plugin for
[horton](https://github.com/madmax983/horton), an embedded LSM key-value
store. It gives your app a durable key-value store and a session store in
one local file. It needs no external service.

- **Durable.** A write returns after it is in the write-ahead log and on
  disk.
- **Async.** One store thread does all disk I/O. Tokio workers never
  block.
- **Sessions.** Optional. Sessions stay after a restart and expire after
  `session.max_age_secs`.
- **Observable.** A `horton` health indicator and `horton_*` Prometheus
  metrics.
- **Safe.** An OS lock stops a second process from opening the file. Key
  limits, value limits and batch limits are typed errors.

## Install

```toml
[dependencies]
autumn-plugin-horton = { git = "https://github.com/autumn-foundation/autumn-plugin-horton" }
```

The crate goes to crates.io after horton 0.17 is there.

## Quick start

From [`examples/visits.rs`](examples/visits.rs). Run it with
`cargo run --example visits`.

```rust,ignore
use autumn_plugin_horton::{Horton, HortonPlugin};
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
        .map_err(AutumnError::internal_server_error)?
        .and_then(|v| <[u8; 8]>::try_from(v).ok())
        .map_or(0, u64::from_le_bytes)
        + 1;
    db.put(b"visits", &count.to_le_bytes())
        .await
        .map_err(AutumnError::internal_server_error)?;
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
| `stats()`, `snapshot()` | Returns counters. |
| `close()` | Flushes and releases the file. The plugin calls it at shutdown. |

## Configuration

Put the settings in `autumn.toml`:

```toml
[horton]
enabled = true                 # false: the plugin does nothing
path = "data/store.horton"     # the store file
size_mib = 256                 # size of a new file; it is sparse
sync = true                    # fdatasync every commit
sessions = false               # keep Autumn sessions in the store
```

The plugin reads these layers. A later layer wins:

1. The defaults above.
2. The builder: `HortonPlugin::new().path(..).size_mib(..).sync(..).sessions(..)`.
3. `[horton]` in `autumn.toml`.
4. `[profile.<p>.horton]` in `autumn.toml`.
5. `[horton]` in `autumn-<p>.toml`.
6. `AUTUMN_HORTON__ENABLED`, `__PATH`, `__SIZE_MIB`, `__SYNC` and
   `__SESSIONS`. `.env` files count.

`HortonPlugin::new().fixed_config(config)` uses `config` and reads no
file or variable. An unknown key or a bad value stops the app at
startup.

## Limits

| Limit | Value |
|---|---|
| Key | 1 to 128 bytes. A key must not start with byte `0x00`. |
| Value | 0 to 4096 bytes. A session's JSON must fit too. |
| Batch | One WAL block (16 KiB). Each op adds 23 bytes, or 31 with a TTL. |
| File | At least 86 MiB (sparse). An existing file keeps its size. |
| Processes | One process opens a file at a time. |

horton is before 1.0. A new minor version can change the file format.
horton then refuses the old file. It never misreads it.

## Health and metrics

- `/actuator/health` shows `components.horton`. It is `UP` while the
  store thread runs. It is `DOWN` when the store is not open or has
  stopped. The indicator is in the readiness group.
- `/actuator/prometheus` shows `horton_up`, `horton_writes_total`,
  `horton_write_groups_total`, `horton_io_{reads,writes,syncs}_total`,
  `horton_flushes_total`, `horton_compaction_{jobs,steps}_total`,
  `horton_cache_{hits,misses}_total`, `horton_device_blocks`,
  `horton_slots{state}` and `horton_level_tables{level}`.

## Development

See [`CLAUDE.md`](CLAUDE.md) for the checks CI runs, and
[`docs/adr/`](docs/adr/) for the design decisions.

## License

MIT or Apache-2.0, at your option.
