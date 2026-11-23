use vstd::prelude::*;

verus! {

// Model of `src/read_cache.rs`. `visible`, `answer` and `fill_until` are
// equivalent to the runtime fns of the same name. The rest is a model:
//
// - horton holds, for each key, its newest version (`Stored`) or nothing.
//   A get at clock `t` gives the value when it is live at `t`.
// - The cache holds an `Entry` for some keys. `says(e, t)` is the cache's
//   answer at clock `t`: unknown, absent, or the value.
// - `coherent(e, s, now)`: at each clock value from `now` on, a known
//   answer of `e` is horton's answer for the stored version `s`.
// - `cache_ok`: every entry is coherent with its key.
//
// The proofs show that each step of the store thread keeps `cache_ok`:
// a put, a delete, a forgotten key (clock and index keys, eviction), a
// fill from a get, a range delete, a clear, and a clock step. Thus a hit
// always gives horton's answer.
//
// A write group commits its ops as one horton batch. Its result equals
// the ops applied one by one, because each op gets a larger sequence
// number. The store thread applies the same ops to the cache one by one,
// and reads no request before it is done. So the per-op proofs cover a
// whole group.
//
// Values are `u64` tokens here; the proofs do not look at the bytes.

/// `visible`: horton's TTL rule. 0 means no expiry.
pub open spec fn spec_visible(expire_at: u64, now: u64) -> bool {
    expire_at == 0 || now < expire_at
}

pub fn visible(expire_at: u64, now: u64) -> (r: bool)
    ensures
        r == spec_visible(expire_at, now),
{
    expire_at == 0 || now < expire_at
}

/// What an entry tells about a key at one clock value.
pub enum Answer {
    Unknown,
    Absent,
    Present,
}

pub open spec fn spec_answer(has_val: bool, expire_at: u64, valid_until: u64, now: u64) -> Answer {
    if now >= valid_until {
        Answer::Unknown
    } else if has_val && spec_visible(expire_at, now) {
        Answer::Present
    } else {
        Answer::Absent
    }
}

pub fn answer(has_val: bool, expire_at: u64, valid_until: u64, now: u64) -> (r: Answer)
    ensures
        r == spec_answer(has_val, expire_at, valid_until, now),
{
    if now >= valid_until {
        Answer::Unknown
    } else if has_val && visible(expire_at, now) {
        Answer::Present
    } else {
        Answer::Absent
    }
}

/// `fill_until`: a value from a get holds for `now` only (saturating);
/// "absent" holds until a write.
pub open spec fn spec_fill_until(found: bool, now: u64) -> u64 {
    if found {
        if now == u64::MAX {
            u64::MAX
        } else {
            (now + 1) as u64
        }
    } else {
        u64::MAX
    }
}

pub fn fill_until(found: bool, now: u64) -> (r: u64)
    ensures
        r == spec_fill_until(found, now),
{
    if found {
        if now == u64::MAX {
            u64::MAX
        } else {
            now + 1
        }
    } else {
        u64::MAX
    }
}

/// The newest version of one key in horton.
pub struct Stored {
    pub val: u64,
    pub expire_at: u64,
}

/// One cache entry. `val == None` means "absent".
pub struct Entry {
    pub val: Option<u64>,
    pub expire_at: u64,
    pub valid_until: u64,
}

/// horton's get at clock `t`.
pub open spec fn horton_get(s: Option<Stored>, t: u64) -> Option<u64> {
    match s {
        Some(x) => if spec_visible(x.expire_at, t) {
            Some(x.val)
        } else {
            None
        },
        None => None,
    }
}

/// The cache's answer at clock `t`: `None` when it does not know.
pub open spec fn says(e: Entry, t: u64) -> Option<Option<u64>> {
    match spec_answer(e.val is Some, e.expire_at, e.valid_until, t) {
        Answer::Unknown => None,
        Answer::Absent => Some(None),
        Answer::Present => Some(e.val),
    }
}

pub open spec fn coherent(e: Entry, s: Option<Stored>, now: u64) -> bool {
    forall|t: u64|
        #![trigger says(e, t)]
        now <= t && says(e, t) is Some ==> says(e, t) == Some(horton_get(s, t))
}

/// The entry that `ReadCache::fill` stores after a get at `now`.
pub open spec fn fill_entry(s: Option<Stored>, now: u64) -> Entry {
    let found = horton_get(s, now);
    Entry { val: found, expire_at: 0, valid_until: spec_fill_until(found is Some, now) }
}

/// The entry that `ReadCache::put` stores for a write.
pub open spec fn put_entry(val: u64, expire_at: u64) -> Entry {
    Entry { val: Some(val), expire_at, valid_until: u64::MAX }
}

/// The entry that `ReadCache::delete` stores for a delete.
pub open spec fn absent_entry() -> Entry {
    Entry { val: None, expire_at: 0, valid_until: u64::MAX }
}

pub proof fn fill_is_coherent(s: Option<Stored>, now: u64)
    ensures
        coherent(fill_entry(s, now), s, now),
{
    let e = fill_entry(s, now);
    assert forall|t: u64|
        #![trigger says(e, t)]
        now <= t && says(e, t) is Some implies says(e, t) == Some(horton_get(s, t)) by {
        if horton_get(s, now) is Some {
            // A value holds for `now` only.
            assert(t == now);
        }
        // "Absent" at `now` stays absent: an expired value stays expired.

    }
}

pub proof fn put_is_coherent(val: u64, expire_at: u64, now: u64)
    ensures
        coherent(put_entry(val, expire_at), Some(Stored { val, expire_at }), now),
{
    let e = put_entry(val, expire_at);
    let s = Some(Stored { val, expire_at });
    assert forall|t: u64|
        #![trigger says(e, t)]
        now <= t && says(e, t) is Some implies says(e, t) == Some(horton_get(s, t)) by {}
}

pub proof fn absent_is_coherent(now: u64)
    ensures
        coherent(absent_entry(), None, now),
{
    let e = absent_entry();
    assert forall|t: u64|
        #![trigger says(e, t)]
        now <= t && says(e, t) is Some implies says(e, t) == Some(horton_get(None, t)) by {}
}

/// The stored version of `k`.
pub open spec fn at(st: Map<Seq<u8>, Stored>, k: Seq<u8>) -> Option<Stored> {
    if st.contains_key(k) {
        Some(st[k])
    } else {
        None
    }
}

pub open spec fn cache_ok(c: Map<Seq<u8>, Entry>, st: Map<Seq<u8>, Stored>, now: u64) -> bool {
    forall|k: Seq<u8>| #[trigger] c.contains_key(k) ==> coherent(c[k], at(st, k), now)
}

/// A step that changes key `k` only. Other entries may go (eviction), but
/// none change. If `k` keeps an entry, it is coherent.
pub proof fn one_key_step_keeps_ok(
    c: Map<Seq<u8>, Entry>,
    st: Map<Seq<u8>, Stored>,
    c2: Map<Seq<u8>, Entry>,
    st2: Map<Seq<u8>, Stored>,
    now: u64,
    k: Seq<u8>,
)
    requires
        cache_ok(c, st, now),
        forall|k2: Seq<u8>| k2 != k ==> #[trigger] at(st2, k2) == at(st, k2),
        forall|k2: Seq<u8>|
            k2 != k && #[trigger] c2.contains_key(k2) ==> c.contains_key(k2) && c2[k2] == c[k2],
        c2.contains_key(k) ==> coherent(c2[k], at(st2, k), now),
    ensures
        cache_ok(c2, st2, now),
{
    assert forall|k2: Seq<u8>| #[trigger] c2.contains_key(k2) implies coherent(
        c2[k2],
        at(st2, k2),
        now,
    ) by {
        if k2 != k {
            assert(c.contains_key(k2));
            assert(at(st2, k2) == at(st, k2));
        }
    }
}

/// A put of `k`: horton stores the value and its expiry; the cache stores
/// the same (or drops `k` when the entry is larger than the budget).
pub proof fn put_keeps_ok(
    c: Map<Seq<u8>, Entry>,
    st: Map<Seq<u8>, Stored>,
    c2: Map<Seq<u8>, Entry>,
    st2: Map<Seq<u8>, Stored>,
    now: u64,
    k: Seq<u8>,
    val: u64,
    expire_at: u64,
)
    requires
        cache_ok(c, st, now),
        at(st2, k) == Some(Stored { val, expire_at }),
        forall|k2: Seq<u8>| k2 != k ==> #[trigger] at(st2, k2) == at(st, k2),
        forall|k2: Seq<u8>|
            k2 != k && #[trigger] c2.contains_key(k2) ==> c.contains_key(k2) && c2[k2] == c[k2],
        c2.contains_key(k) ==> c2[k] == put_entry(val, expire_at),
    ensures
        cache_ok(c2, st2, now),
{
    put_is_coherent(val, expire_at, now);
    one_key_step_keeps_ok(c, st, c2, st2, now, k);
}

/// A delete of `k`: horton has no version; the cache stores "absent".
pub proof fn delete_keeps_ok(
    c: Map<Seq<u8>, Entry>,
    st: Map<Seq<u8>, Stored>,
    c2: Map<Seq<u8>, Entry>,
    st2: Map<Seq<u8>, Stored>,
    now: u64,
    k: Seq<u8>,
)
    requires
        cache_ok(c, st, now),
        at(st2, k) == None::<Stored>,
        forall|k2: Seq<u8>| k2 != k ==> #[trigger] at(st2, k2) == at(st, k2),
        forall|k2: Seq<u8>|
            k2 != k && #[trigger] c2.contains_key(k2) ==> c.contains_key(k2) && c2[k2] == c[k2],
        c2.contains_key(k) ==> c2[k] == absent_entry(),
    ensures
        cache_ok(c2, st2, now),
{
    absent_is_coherent(now);
    one_key_step_keeps_ok(c, st, c2, st2, now, k);
}

/// A write of any kind to `k` (the clock key, a session index key) with
/// the entry of `k` forgotten. Also an eviction, with `st2 == st`.
pub proof fn forget_keeps_ok(
    c: Map<Seq<u8>, Entry>,
    st: Map<Seq<u8>, Stored>,
    c2: Map<Seq<u8>, Entry>,
    st2: Map<Seq<u8>, Stored>,
    now: u64,
    k: Seq<u8>,
)
    requires
        cache_ok(c, st, now),
        forall|k2: Seq<u8>| k2 != k ==> #[trigger] at(st2, k2) == at(st, k2),
        forall|k2: Seq<u8>|
            k2 != k && #[trigger] c2.contains_key(k2) ==> c.contains_key(k2) && c2[k2] == c[k2],
        !c2.contains_key(k),
    ensures
        cache_ok(c2, st2, now),
{
    one_key_step_keeps_ok(c, st, c2, st2, now, k);
}

/// A get of `k` that missed: horton answers and the cache keeps it.
pub proof fn fill_keeps_ok(
    c: Map<Seq<u8>, Entry>,
    st: Map<Seq<u8>, Stored>,
    c2: Map<Seq<u8>, Entry>,
    now: u64,
    k: Seq<u8>,
)
    requires
        cache_ok(c, st, now),
        forall|k2: Seq<u8>|
            k2 != k && #[trigger] c2.contains_key(k2) ==> c.contains_key(k2) && c2[k2] == c[k2],
        c2.contains_key(k) ==> c2[k] == fill_entry(at(st, k), now),
    ensures
        cache_ok(c2, st, now),
{
    fill_is_coherent(at(st, k), now);
    one_key_step_keeps_ok(c, st, c2, st, now, k);
}

/// A range delete: horton drops every key in `r`; so does the cache.
pub proof fn range_delete_keeps_ok(
    c: Map<Seq<u8>, Entry>,
    st: Map<Seq<u8>, Stored>,
    c2: Map<Seq<u8>, Entry>,
    st2: Map<Seq<u8>, Stored>,
    now: u64,
    r: spec_fn(Seq<u8>) -> bool,
)
    requires
        cache_ok(c, st, now),
        forall|k: Seq<u8>| r(k) ==> #[trigger] at(st2, k) == None::<Stored>,
        forall|k: Seq<u8>| !r(k) ==> #[trigger] at(st2, k) == at(st, k),
        forall|k: Seq<u8>| #[trigger]
            c2.contains_key(k) ==> !r(k) && c.contains_key(k) && c2[k] == c[k],
    ensures
        cache_ok(c2, st2, now),
{
    assert forall|k: Seq<u8>| #[trigger] c2.contains_key(k) implies coherent(
        c2[k],
        at(st2, k),
        now,
    ) by {
        assert(!r(k));
        assert(c.contains_key(k));
        assert(at(st2, k) == at(st, k));
    }
}

/// A clear (after a failed write, whatever horton then holds).
pub proof fn clear_keeps_ok(c2: Map<Seq<u8>, Entry>, st2: Map<Seq<u8>, Stored>, now: u64)
    requires
        forall|k: Seq<u8>| !(#[trigger] c2.contains_key(k)),
    ensures
        cache_ok(c2, st2, now),
{
}

/// The store clock moves forward.
pub proof fn tick_keeps_ok(c: Map<Seq<u8>, Entry>, st: Map<Seq<u8>, Stored>, now: u64, later: u64)
    requires
        cache_ok(c, st, now),
        now <= later,
    ensures
        cache_ok(c, st, later),
{
    assert forall|k: Seq<u8>| #[trigger] c.contains_key(k) implies coherent(
        c[k],
        at(st, k),
        later,
    ) by {
        let e = c[k];
        let s = at(st, k);
        assert(coherent(e, s, now));
        assert forall|t: u64|
            #![trigger says(e, t)]
            later <= t && says(e, t) is Some implies says(e, t) == Some(horton_get(s, t)) by {
            assert(now <= t);
        }
    }
}

/// The goal: a hit gives horton's answer.
pub proof fn a_hit_is_hortons_answer(
    c: Map<Seq<u8>, Entry>,
    st: Map<Seq<u8>, Stored>,
    now: u64,
    k: Seq<u8>,
)
    requires
        cache_ok(c, st, now),
        c.contains_key(k),
        says(c[k], now) is Some,
    ensures
        says(c[k], now) == Some(horton_get(at(st, k), now)),
{
    assert(coherent(c[k], at(st, k), now));
}

fn main() {
}

} // verus!
