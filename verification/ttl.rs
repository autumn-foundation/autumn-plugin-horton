use vstd::prelude::*;

verus! {

// Model of `src/ttl.rs`. Each exec fn is equivalent to the runtime fn of
// the same name. horton reads `expire_at == 0` as "no expiry". Compaction
// purges with the purge limit. The proofs show:
// - a TTL never gives expiry 0 (R11);
// - a value lives at least its TTL;
// - the store clock never goes back (R12);
// - a purge never removes a value that a later read can see.

/// `ttl_secs`: whole seconds, rounded up. `secs` and `nanos` are the parts
/// of a `Duration`.
pub fn ttl_secs(secs: u64, nanos: u32) -> (r: u64)
    requires nanos < 1_000_000_000,
    ensures
        nanos == 0 ==> r == secs,
        nanos > 0 && secs < u64::MAX ==> r == secs + 1,
        nanos > 0 && secs == u64::MAX ==> r == u64::MAX,
        (secs > 0 || nanos > 0) ==> r > 0,
{
    if nanos > 0 {
        if secs == u64::MAX { u64::MAX } else { secs + 1 }
    } else {
        secs
    }
}

/// `expire_at`: the expiry tick for a write at `now` with `ttl` seconds.
pub fn expire_at(now: u64, ttl: u64) -> (r: u64)
    ensures
        r != 0,
        r >= now,
        now as int + ttl as int <= u64::MAX as int && now as int + ttl as int > 0
            ==> r as int == now as int + ttl as int,
        now as int + ttl as int > u64::MAX as int ==> r == u64::MAX,
{
    if ttl > u64::MAX - now {
        u64::MAX
    } else if now + ttl == 0 {
        1
    } else {
        now + ttl
    }
}

/// `deadline`: the expiry for a write at clock `now` that must live at
/// least `ttl` seconds. The clock drops the fraction of a second, so it
/// adds one second.
pub fn deadline(now: u64, ttl: u64) -> (r: u64)
    ensures
        r != 0,
        r >= now,
        now as int + ttl as int + 1 <= u64::MAX as int ==> r as int == now as int + ttl as int + 1,
{
    let plus_one = if ttl == u64::MAX { u64::MAX } else { ttl + 1 };
    expire_at(now, plus_one)
}

/// A value with expiry `now + ttl + 1` is visible at every clock from
/// `now` to `now + ttl`: it lives at least `ttl` seconds.
pub proof fn lives_at_least(now: u64, ttl: u64, t: u64)
    requires
        now as int + ttl as int + 1 <= u64::MAX as int,
        now <= t,
        t as int <= now as int + ttl as int,
    ensures visible((now as int + ttl as int + 1) as u64, t),
{}

/// `purge_limit`: compaction purges entries that expired at or before
/// this. It is the store clock, but no later than the clock on disk.
pub fn purge_limit(now: u64, durable: u64) -> (r: u64)
    ensures r <= now, r <= durable, r == now || r == durable,
{
    if durable < now { durable } else { now }
}

/// After a restart the store clock starts at `advance(durable, wall)`.
/// That is at least every purge limit before the restart, so an entry a
/// purge removed was already invisible to every later read.
pub proof fn purge_survives_restart(now: u64, durable: u64, wall: u64, expire: u64)
    requires purged(expire, if durable < now { durable } else { now }),
    ensures !visible(expire, advance_spec(durable, wall)),
{}

/// `advance`: the next store-clock value.
pub fn advance(last: u64, wall: u64) -> (r: u64)
    ensures
        r >= last,
        r >= wall,
        r == advance_spec(last, wall),
{
    if wall > last { wall } else { last }
}

/// A value read with clock `now` is visible when `expire_at == 0` or
/// `expire_at > now` (horton's rule).
pub open spec fn visible(expire: u64, now: u64) -> bool {
    expire == 0 || expire > now
}

/// Compaction turns an entry into a tombstone when `expire <= purge`.
pub open spec fn purged(expire: u64, purge: u64) -> bool {
    expire != 0 && expire <= purge
}

/// The purge rule is safe: compaction uses the clock `purge`, and every
/// later read uses a clock `now >= purge` (the clock never goes back). An
/// entry that compaction purges is already invisible to that read.
pub proof fn purge_is_invisible(expire: u64, purge: u64, now: u64)
    requires purged(expire, purge), now >= purge,
    ensures !visible(expire, now),
{}

/// The store clock after a purge at clock `purge` reads the wall clock
/// `wall` with `advance`. An entry the purge removed stays invisible,
/// even when the wall clock went back.
pub proof fn advance_keeps_purge_safe(purge: u64, wall: u64, expire: u64)
    requires purged(expire, purge),
    ensures !visible(expire, advance_spec(purge, wall)),
{}

pub open spec fn advance_spec(last: u64, wall: u64) -> u64 {
    if wall > last { wall } else { last }
}

fn main() {}

} // verus!
