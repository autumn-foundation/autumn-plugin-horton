//! Time-to-live and store-clock arithmetic.
//!
//! The store clock counts Unix seconds. horton reads `expire_at == 0` as
//! "no expiry", so a TTL must never give 0. The clock must never go back,
//! because compaction purges with it. `verification/ttl.rs` proves both.

use std::time::Duration;

/// Whole seconds in `ttl`, rounded up. A non-zero TTL gives at least 1.
#[must_use]
pub const fn ttl_secs(ttl: Duration) -> u64 {
    let secs = ttl.as_secs();
    if ttl.subsec_nanos() > 0 {
        if secs == u64::MAX { u64::MAX } else { secs + 1 }
    } else {
        secs
    }
}

/// The expiry tick for a value written at `now` with `ttl_secs`.
/// The result is never 0 and never less than `now`.
#[must_use]
pub const fn expire_at(now: u64, ttl_secs: u64) -> u64 {
    if ttl_secs > u64::MAX - now {
        u64::MAX
    } else if now + ttl_secs == 0 {
        1
    } else {
        now + ttl_secs
    }
}

/// The expiry tick for a value that must live at least `ttl_secs`.
///
/// The store clock counts whole seconds and drops the fraction, so the
/// value lives at least `ttl_secs` and less than `ttl_secs + 1` seconds.
#[must_use]
pub const fn deadline(now: u64, ttl_secs: u64) -> u64 {
    let plus_one = if ttl_secs == u64::MAX {
        u64::MAX
    } else {
        ttl_secs + 1
    };
    expire_at(now, plus_one)
}

/// The purge limit: the store clock, but no later than the clock on disk.
///
/// Compaction purges entries that expired at or before this limit. After a
/// restart the clock starts from the value on disk, so a purge never
/// removes a value that a later read can see.
#[must_use]
pub const fn purge_limit(now: u64, durable: u64) -> u64 {
    if durable < now { durable } else { now }
}

/// The next store-clock value. It never goes back.
#[must_use]
pub const fn advance(last: u64, wall: u64) -> u64 {
    if wall > last { wall } else { last }
}

/// The wall clock, in Unix seconds. A clock before 1970 reads as 0.
#[must_use]
pub fn wall_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ttl_rounds_up() {
        assert_eq!(ttl_secs(Duration::ZERO), 0);
        assert_eq!(ttl_secs(Duration::from_millis(1)), 1);
        assert_eq!(ttl_secs(Duration::from_secs(5)), 5);
        assert_eq!(ttl_secs(Duration::from_millis(5001)), 6);
        assert_eq!(ttl_secs(Duration::MAX), u64::MAX);
    }

    #[test]
    fn expiry_is_never_zero() {
        assert_eq!(expire_at(0, 0), 1);
        assert_eq!(expire_at(10, 5), 15);
        assert_eq!(expire_at(u64::MAX, 5), u64::MAX);
        assert_eq!(expire_at(7, 0), 7);
    }

    #[test]
    fn deadline_adds_one_second_for_the_dropped_fraction() {
        assert_eq!(deadline(10, 1), 12);
        assert_eq!(deadline(10, 0), 11);
        assert_eq!(deadline(u64::MAX, u64::MAX), u64::MAX);
    }

    #[test]
    fn purge_limit_is_the_smaller_clock() {
        assert_eq!(purge_limit(10, 7), 7);
        assert_eq!(purge_limit(7, 10), 7);
    }

    #[test]
    fn clock_never_goes_back() {
        assert_eq!(advance(10, 20), 20);
        assert_eq!(advance(20, 10), 20);
        assert_eq!(advance(0, 0), 0);
    }

    proptest::proptest! {
        #[test]
        fn expiry_props(now: u64, secs: u64) {
            let e = expire_at(now, secs);
            proptest::prop_assert!(e != 0);
            proptest::prop_assert!(e >= now);
        }

        #[test]
        fn clock_props(last: u64, wall: u64) {
            let next = advance(last, wall);
            proptest::prop_assert!(next >= last);
            proptest::prop_assert!(next >= wall);
        }
    }
}
