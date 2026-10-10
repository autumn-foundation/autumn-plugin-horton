# ADR 0001: One store thread owns the database

## Status

Accepted.

## Context

horton's `Db` takes `&mut self` for writes. Its futures are not `Send`.
Its `FileDevice` does blocking file I/O. Autumn runs handlers on Tokio
worker threads, which must not block.

## Decision

One dedicated thread owns the `Db`. Callers send requests on a bounded
Tokio channel and wait on a oneshot reply. The thread:

- commits queued writes as one group (one WAL block, one `fdatasync`);
- flushes and compacts when horton asks for room;
- runs bounded compaction steps when it is idle.

The design is horton's `examples/kvstore/store.rs`, made async.

## Consequences

- No Tokio worker does disk I/O. The handle works on any runtime.
- Writes are serial. Group commit gives back most of the throughput.
- A full channel makes callers wait. This is backpressure.
- When the last handle drops, the thread stops. The WAL keeps the data.
