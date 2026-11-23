# ADR 0006: A read cache on the store thread

## Status

Accepted.

## Context

A get that reads a table is slow: about 60 µs in `benches/throughput.rs`,
or 16,000 gets per second for the whole store. A profile shows 41 % of
the time in `horton::sstable::check_block_crc`. horton checks the CRC of
each 16 KiB block that a get reads: the footer, the bloom filter, the
index and the data block of each table that can hold the key. It checks
it also when the block comes from its own block cache. The plugin cannot
change horton.

ADR 0002 rejects an Autumn `Cache` backend. That decision is about the
Autumn API. This one is about the plugin's own read path.

## Decision

- The store thread keeps a bounded map of recent answers in front of
  horton (`src/read_cache.rs`). The size is `cache_mib` (default 16 MiB;
  1 MiB for each fleet store). 0 turns it off.
- A write updates the cache after horton accepts it. A put stores the
  value and the expiry tick that horton got. A delete stores "absent". A
  write changes only a key that has an entry: only a get adds keys. So
  keys that the app writes but does not read do not push out the keys it
  reads, and a write-only load costs one hash lookup per key. A range
  delete removes the range. A failed write clears the cache.
- A get on a miss stores horton's answer. horton does not return the
  expiry of a value, so a value from a get holds only for the current
  clock second. "Absent" holds until a write: the store clock never goes
  back, so an expired value stays expired.
- The map is a `HashMap` with the standard `SipHash` hasher, because the
  app chooses the keys. Eviction uses the CLOCK rule. A range delete
  walks the whole map; range deletes are rare.
- `verification/read_cache.rs` proves that each step keeps every entry
  equal to horton's answer. A property test runs random calls on a store
  with the cache, one without it, and a model, and checks every get.

## Consequences

- A hit costs one hash lookup and one copy. With 128 callers,
  `get_cached` in the benchmark serves about 670,000 gets per second and
  `get_table` (cache off) about 16,500: 40 times more.
- A key that a get reads but no write touched since open goes back to
  horton once per clock second. A hot key still hits almost always.
- Writes do a little more work: one hash lookup per key. The store thread
  updates the cache after it replies, so the callers do not wait for it.
  An earlier design that added every written key cost 5 % of put
  throughput with 128 callers.
- The first get of a key after open goes to horton. A session store reads
  a session before it saves it, so its next load is a hit.
- Memory use grows by up to `cache_mib`. A fleet counts `resident_bytes`
  for each open store; keep `resident_bytes` above `cache_mib`.
- A corrupt block that the cache hides is found at the next miss or
  compaction, not at the get.
