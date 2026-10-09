# ADR 0004: A store quota and a session sweep

## Status

Accepted.

## Context

The security review filled a store with sessions until horton returned
`RegionFull`. After that every write failed, deletes too, also after every
TTL passed and a reopen. horton purges expired entries only when a
compaction job runs, and no job fits two free slots. An app that saves
anonymous sessions could be locked up for good by a request loop.

## Decision

- At `RESERVE_SLOTS` (6) free table slots, the store refuses writes that
  add data with `StoreFull`. Deletes still work.
- Each session save also writes an index key that holds the expiry. A
  sweep (every 60 s) deletes expired sessions with this index.
- The horton side (reclaim below the level triggers) goes to horton 0.19:
  madmax983/horton#32.

## Consequences

- A full store refuses data but never locks up. Reads, deletes, sweeps,
  close and reopen keep working.
- With horton 0.18, only compaction gives deleted space back. Size the
  store with room to spare.
- A session save writes two keys: about 70 bytes more per save.
