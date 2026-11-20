# ADR 0005: A fleet database is a directory with a stamp file

## Status

Accepted.

## Context

Autumn 0.11 runs database fleets: one database for each tenant or slot.
The fleet engine must stamp the key and the ownership epoch in each
database, read the epoch of a closed database, and give a snapshot. A
snapshot of a database with no change must be the same bytes, or the fleet
ships a new generation each time.

A horton store is one block-device file. horton has no read-only open, and
an open recovers the WAL. So the engine cannot read a stamp inside a
closed store without a change to it.

## Decision

- A fleet database is a directory. It holds `store.horton` and `identity`.
  `identity` is JSON with the key name and the epoch.
- `open` opens the store first. The store file lock makes the stamp check
  and the stamp write safe between processes. The stamp write is a
  temporary file, a sync and a rename.
- `epoch_stamp` reads `identity` only.
- A snapshot of an open store is a request to the store thread. The thread
  flushes, runs compaction until no job is pending, and copies the file
  through its own handle. No request runs during the copy, so the copy is
  consistent. After the flush and the compaction, a store with no write
  does not change, so the next copy has the same bytes.
- The copy keeps all-zero blocks as holes.
- In-use means a call that waits for the store thread. A clone of the
  handle does not block an idle close.

## Consequences

- A restore puts both files back, so the stamp follows the data.
- A snapshot reads the whole store file. Keep `engine.size_mib` near the
  data size.
- A snapshot flushes the memtable, so it can add a level 0 table.
