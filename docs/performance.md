# Performance audit: the hot path

This file records an audit of the read and write paths, the changes it
made, and the work that is left. The benchmark is
`benches/throughput.rs`. Run it with:

```sh
cargo bench --bench throughput
```

Each benchmark runs 1, 16 or 128 callers on a Tokio runtime with four
workers. One iteration is one call. The store is 256 MiB with `sync =
false`, except `put_sync`. Values are 100 bytes. The key space is 20,000
keys.

| Benchmark | What it measures |
|---|---|
| `put`, `put_sync` | `Horton::put` over the key space, without and with `fdatasync` |
| `get_memtable` | `get` of 2,000 keys that are in the memtable, read cache off |
| `get_table` | `get` of 20,000 keys after `compact`, read cache off |
| `get_cached` | the `get_table` keys through the read cache (default size) |
| `scan_100` | `scan` of 100 entries from a random key |

## How a call travels

```mermaid
sequenceDiagram
  participant T as Tokio task
  participant C as Channel (1024)
  participant S as Store thread
  participant R as Read cache
  participant H as horton
  T->>C: Request + oneshot
  C->>S: recv (poll 50 µs, then sleep)
  S->>R: lookup (get)
  alt hit
    R-->>S: value
  else miss
    S->>H: get_with_time (CRC of each block)
    H-->>S: value
    S->>R: fill
  end
  S->>S: up to 3 more queued gets, then publish stats once
  S-->>T: replies (oneshot)
```

## What the profiles showed

`perf` on the store thread, before the changes:

| Path | Where the time went |
|---|---|
| Table get | 41 % of all samples in `horton::sstable::check_block_crc`. horton checks the CRC of each 16 KiB block a get reads (footer, bloom, index, data), also on a block-cache hit. About 64 KiB of CRC for each table a get looks at. |
| Memtable get | About 35 % of the store thread in kernel wake-ups (`_raw_spin_unlock_irqrestore`): each reply woke a parked runtime thread. 9 % in `MemTable::max_covering_rdel`, which scans all 4,096 memtable slots on each get. A 4 KiB `calloc` on each get. |
| One caller | 44 µs for each round trip, almost all thread sleep and wake-up on both sides. |
| Stats | `publish` ran two times for each request, and each time built a new `Stats` with a new `Vec`. |
| Scan | 42 % in the CRC check again. The 44 KiB scan object is not a cost. |

## Changes

| Change | Effect |
|---|---|
| Read cache (ADR 0006) | A repeat get does not read horton, so it skips the CRC checks. |
| Read batches of 4 | One stats publish and one burst of replies for up to four gets. Batches of 8, 16 and 256 were slower at 16 callers: the callers wait for the whole batch. |
| Poll 50 µs before sleep | A caller that sends again at once finds the store thread awake. At most half the CPUs poll at the same time, over all stores of the process. Not on a one-CPU machine. |
| Publish once, in place | No allocation and one mutex lock for each request. |
| Reused value buffer | No 4 KiB `calloc` per get. A returned value has the capacity of its length. |
| Writes update only cached keys | A write costs one hash lookup per op in the cache. Keys that the app writes but does not read take no space. The update runs before the stats publish, so the stats match each reply. |
| Hot keys last | A key that misses in two clock seconds is also read at clock `FAR`. A value live there holds until a write, so a hot key costs two horton reads one time. |

## Results

The mean of two interleaved runs of each build on one 4-CPU VM, Rust
1.97, release profile. "Before" is commit `00349c6`. Before that commit
there was no read cache, so `get_cached` before is the same path as
`get_table`. `put_sync` depends on the disk's `fdatasync` time and moves
by up to 30 % between runs. The "after" put rows come from the first
cache design, which added each written key to the cache. Writes now only
update keys that are in the cache, which is less work.

| Benchmark | Before (calls/s) | After (calls/s) | Change |
|---|--:|--:|--:|
| `put/1` | 19,164 | 27,834 | +45 % |
| `put/16` | 162,175 | 165,035 | +2 % |
| `put/128` | 262,395 | 249,685 | −5 % |
| `put_sync/1` | 3,813 | 4,182 | +10 % |
| `put_sync/16` | 32,670 | 35,490 | +9 % |
| `put_sync/128` | 114,635 | 120,990 | +6 % |
| `get_memtable/1` | 20,338 | 46,253 | 2.3× |
| `get_memtable/16` | 214,355 | 365,210 | +70 % |
| `get_memtable/128` | 207,940 | 397,375 | +91 % |
| `get_table/1` | 8,468 | 10,894 | +29 % |
| `get_table/16` | 14,400 | 16,790 | +17 % |
| `get_table/128` | 14,482 | 16,573 | +14 % |
| `get_cached/1` | 8,959 | 56,947 | 6.4× |
| `get_cached/16` | 14,108 | 529,445 | 37.5× |
| `get_cached/128` | 14,434 | 670,905 | 46.5× |
| `scan_100/1` | 3,623 | 3,686 | +2 % |
| `scan_100/16` | 4,765 | 4,745 | 0 % |

## Work that is left: horton

These costs are inside horton 0.18. The plugin cannot change them.

1. **CRC on a cache hit.** `read_block_cached` returns the raw image, and
   every caller checks its CRC again. A block cache that holds only
   checked blocks could skip the check on a hit. With a slicing-by-8 CRC
   over 16 KiB, this is most of a table get.
2. **A faster CRC.** `crc32` is a table-driven slicing-by-8 loop. A
   carry-less multiply (PCLMULQDQ, ARMv8 PMULL) version is several times
   faster on the same polynomial.
3. **`max_covering_rdel`** scans every memtable slot on each get, also
   when the memtable holds no range delete. A count of range deletes, or
   a separate list, makes the common case free.
4. **Group commit across WAL blocks.** One write batch must fit one 16 KiB
   WAL block, so one `fdatasync` covers at most about 120 puts of 100
   bytes. horton could let one sync cover more than one block.

## Work that is left: the plugin

- Gets still pass through the store thread. Reads that do not touch the
  store thread would need a `Sync` read path in horton.
- A `get_many` call would put many keys in one request and one reply.
