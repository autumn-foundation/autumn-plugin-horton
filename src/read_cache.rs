//! The read cache.
//!
//! The store thread keeps the answers of recent gets in memory.
//! A get that the cache can answer does not read horton. horton checks the
//! CRC of each 16 KiB block that a get reads, so a table read costs tens of
//! microseconds. A cache hit costs one map lookup and one copy.
//!
//! Only the store thread uses the cache, so it needs no lock.
//!
//! **Coherence.** Each entry tells what a horton get gives for one key,
//! for each clock value below `valid_until`. The store thread keeps this
//! true:
//!
//! - A write updates the entry of each key it writes, after horton accepts
//!   the write: a put stores the value and its expiry, a delete stores
//!   "absent". A write does not add a key that has no entry: only a get
//!   adds keys. Thus keys that the app writes but never reads do not push
//!   out the keys it reads. The store clock and session index keys are only
//!   forgotten.
//! - A range delete removes the entries in the range. This walks the
//!   whole cache, but range deletes are rare: the app calls them, and the
//!   session sweep runs once a minute.
//! - A failed write clears the whole cache.
//! - A get fills the entry from horton's answer. horton does not tell the
//!   expiry of a value it returns, so a value from a get holds only for the
//!   current clock value. "Absent" holds until a write, because the store
//!   clock never goes back and an expired value stays expired.
//! - When a key misses again after that ([`Lookup::Stale`]), the store
//!   thread also asks horton for the key at clock [`FAR`]. A value that is
//!   live at `FAR` is live at each clock value up to `FAR`, so its entry
//!   holds until a write. A key that a get reads once costs one horton
//!   read; a key that it reads again costs two, one time.
//!
//! `verification/read_cache.rs` proves that these rules keep every entry
//! equal to horton's answer. [`visible`], [`answer`] and [`fill_until`]
//! mirror that spec.
//!
//! **Size.** The cache counts the bytes of each key and value, plus
//! [`ENTRY_OVERHEAD`]. Above the budget it evicts with the CLOCK rule: an
//! entry that a get used since the last pass gets a second chance.
//!
//! **Map.** A hash map with the standard (keyed `SipHash`) hasher: the app
//! chooses the keys, so the hasher must resist collision attacks. A
//! B-tree map measured 15 % slower on writes, because keys with a long
//! common prefix cost many compares.

use std::collections::{HashMap, VecDeque};
use std::rc::Rc;

/// The clock value of the second read of a hot key: the largest clock
/// value below `u64::MAX`. An entry that holds "until a write" has
/// `valid_until == u64::MAX`, so it holds for each clock value up to here.
pub const FAR: u64 = u64::MAX - 1;

/// Bytes each entry costs beyond its key and value: the map node, the
/// shared key header and the eviction queue slot.
pub const ENTRY_OVERHEAD: usize = 128;

/// True when a value with expiry tick `expire_at` is live at clock `now`.
/// This is horton's rule: 0 means no expiry.
#[must_use]
pub const fn visible(expire_at: u64, now: u64) -> bool {
    expire_at == 0 || now < expire_at
}

/// What an entry tells about a key at one clock value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answer {
    /// The entry does not hold at this clock value. Ask horton.
    Unknown,
    /// horton has no live value for the key.
    Absent,
    /// horton gives the entry's value.
    Present,
}

/// The answer of an entry at clock `now`. `has_val` tells if the entry
/// holds a value; `expire_at` is that value's expiry tick.
#[must_use]
pub const fn answer(has_val: bool, expire_at: u64, valid_until: u64, now: u64) -> Answer {
    if now >= valid_until {
        Answer::Unknown
    } else if has_val && visible(expire_at, now) {
        Answer::Present
    } else {
        Answer::Absent
    }
}

/// `valid_until` for an entry that a get at clock `now` fills. A value
/// holds for `now` only, or until a write when it is `lasting` (also live
/// at [`FAR`]). "Absent" holds until a write.
#[must_use]
pub const fn fill_until(found: bool, lasting: bool, now: u64) -> u64 {
    if found && !lasting {
        now.saturating_add(1)
    } else {
        u64::MAX
    }
}

/// The result of [`ReadCache::lookup`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Lookup {
    /// The cache knows horton's answer: the value, or `None` for absent.
    Hit(Option<Vec<u8>>),
    /// horton must answer. The cache has no entry for the key.
    Miss,
    /// horton must answer. The key has an entry, but it no longer holds:
    /// a get read the key in an earlier clock second. The key is hot.
    Stale,
}

/// What the cache knows about one key.
#[derive(Debug)]
struct Entry {
    /// The value, or `None` for "absent".
    val: Option<Box<[u8]>>,
    /// The value's expiry tick. 0 means no expiry.
    expire_at: u64,
    /// The entry holds for clock values below this.
    valid_until: u64,
    /// The entry's number in the eviction queue.
    generation: u64,
    /// A get used the entry since the last eviction pass.
    referenced: bool,
}

impl Entry {
    fn cost(&self, key: &[u8]) -> usize {
        key.len() + self.val.as_ref().map_or(0, |v| v.len()) + ENTRY_OVERHEAD
    }
}

/// Read cache counters.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct CacheCounts {
    /// Gets that the cache answered.
    pub hits: u64,
    /// Gets that went to horton.
    pub misses: u64,
    /// Entries in the cache.
    pub entries: u64,
    /// Bytes the entries take, by the cache's count.
    pub bytes: u64,
}

/// A bounded map from key to horton's answer. See the module docs.
#[derive(Debug)]
pub struct ReadCache {
    map: HashMap<Rc<[u8]>, Entry>,
    /// Keys in eviction order, with their generation. An item whose
    /// generation is not the entry's is stale and is skipped.
    queue: VecDeque<(Rc<[u8]>, u64)>,
    bytes: usize,
    budget: usize,
    next_generation: u64,
    hits: u64,
    misses: u64,
}

impl ReadCache {
    /// A cache that holds at most `budget` bytes. 0 turns the cache off.
    #[must_use]
    pub fn new(budget: usize) -> Self {
        Self {
            map: HashMap::new(),
            queue: VecDeque::new(),
            bytes: 0,
            budget,
            next_generation: 0,
            hits: 0,
            misses: 0,
        }
    }

    /// The cached answer for `key` at clock `now`.
    pub fn lookup(&mut self, key: &[u8], now: u64) -> Lookup {
        if self.budget == 0 {
            return Lookup::Miss;
        }
        let found = self.map.get_mut(key).map_or(Lookup::Miss, |e| {
            match answer(e.val.is_some(), e.expire_at, e.valid_until, now) {
                Answer::Unknown => Lookup::Stale,
                Answer::Absent => {
                    e.referenced = true;
                    Lookup::Hit(None)
                }
                Answer::Present => {
                    e.referenced = true;
                    Lookup::Hit(e.val.as_deref().map(<[u8]>::to_vec))
                }
            }
        });
        if let Lookup::Hit(_) = found {
            self.hits += 1;
        } else {
            self.misses += 1;
        }
        found
    }

    /// Stores horton's answer `found` for a get of `key` at clock `now`.
    /// `lasting` tells that horton also has the value live at [`FAR`].
    pub fn fill(&mut self, key: &[u8], found: Option<&[u8]>, now: u64, lasting: bool) {
        let valid_until = fill_until(found.is_some(), lasting, now);
        self.set(key, found, 0, valid_until, true);
    }

    /// Updates the entry of `key`, if it has one, to a value that a write
    /// gave it, with expiry tick `expire_at` (0: none).
    pub fn put(&mut self, key: &[u8], val: &[u8], expire_at: u64) {
        self.set(key, Some(val), expire_at, u64::MAX, false);
    }

    /// Updates the entry of `key`, if it has one, to "absent".
    pub fn delete(&mut self, key: &[u8]) {
        self.set(key, None, 0, u64::MAX, false);
    }

    /// Removes the entry of `key`.
    pub fn forget(&mut self, key: &[u8]) {
        if let Some(e) = self.map.remove(key) {
            self.bytes -= e.cost(key);
        }
    }

    /// Removes every entry in `[start, end)`.
    pub fn remove_range(&mut self, start: &[u8], end: &[u8]) {
        if start >= end || self.map.is_empty() {
            return;
        }
        let mut freed = 0;
        self.map.retain(|k, e| {
            let inside = start <= &**k && &**k < end;
            if inside {
                freed += e.cost(k);
            }
            !inside
        });
        self.bytes -= freed;
    }

    /// Removes every entry. The counters stay.
    pub fn clear(&mut self) {
        self.map.clear();
        self.queue.clear();
        self.bytes = 0;
    }

    /// The counters.
    #[must_use]
    pub fn counts(&self) -> CacheCounts {
        CacheCounts {
            hits: self.hits,
            misses: self.misses,
            entries: self.map.len() as u64,
            bytes: self.bytes as u64,
        }
    }

    /// Stores an entry for `key`. With `add == false`, only a key that has
    /// an entry changes.
    fn set(&mut self, key: &[u8], val: Option<&[u8]>, expire_at: u64, valid_until: u64, add: bool) {
        if self.budget == 0 || (!add && self.map.is_empty()) {
            return;
        }
        let cost = key.len() + val.map_or(0, <[u8]>::len) + ENTRY_OVERHEAD;
        if cost > self.budget {
            self.forget(key);
            return;
        }
        if let Some(e) = self.map.get_mut(key) {
            // In place: the queue item and its generation stay valid.
            let old = e.cost(key);
            match (&mut e.val, val) {
                // Same length: reuse the allocation.
                (Some(have), Some(new)) if have.len() == new.len() => have.copy_from_slice(new),
                (slot, new) => *slot = new.map(Box::from),
            }
            e.expire_at = expire_at;
            e.valid_until = valid_until;
            self.bytes = self.bytes - old + cost;
        } else if !add {
            return;
        } else {
            let key: Rc<[u8]> = Rc::from(key);
            let generation = self.next_generation;
            self.next_generation += 1;
            self.queue.push_back((Rc::clone(&key), generation));
            self.map.insert(
                key,
                Entry {
                    val: val.map(Box::from),
                    expire_at,
                    valid_until,
                    generation,
                    referenced: false,
                },
            );
            self.bytes += cost;
        }
        self.evict();
        self.trim_queue();
    }

    /// Evicts entries until the cache fits its budget.
    fn evict(&mut self) {
        while self.bytes > self.budget {
            let Some((key, generation)) = self.queue.pop_front() else {
                return;
            };
            let Some(e) = self.map.get_mut(&key) else {
                continue;
            };
            if e.generation != generation {
                continue;
            }
            if e.referenced {
                e.referenced = false;
                self.queue.push_back((key, generation));
            } else {
                self.forget(&key);
            }
        }
    }

    /// Rebuilds the queue when stale items outnumber the entries, so the
    /// queue stays in proportion to the map.
    fn trim_queue(&mut self) {
        if self.queue.len() > 2 * self.map.len() + 64 {
            self.queue = self
                .map
                .iter()
                .map(|(k, e)| (Rc::clone(k), e.generation))
                .collect();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use proptest::prelude::*;

    use super::*;

    #[test]
    fn answer_follows_validity_then_expiry() {
        assert_eq!(answer(true, 0, 10, 9), Answer::Present);
        assert_eq!(answer(true, 0, 10, 10), Answer::Unknown);
        assert_eq!(answer(true, 5, u64::MAX, 4), Answer::Present);
        assert_eq!(answer(true, 5, u64::MAX, 5), Answer::Absent);
        assert_eq!(answer(false, 0, u64::MAX, 7), Answer::Absent);
        assert_eq!(fill_until(true, false, 7), 8);
        assert_eq!(fill_until(true, false, u64::MAX), u64::MAX);
        assert_eq!(fill_until(true, true, 7), u64::MAX);
        assert_eq!(fill_until(false, false, 7), u64::MAX);
        assert_eq!(fill_until(false, true, 7), u64::MAX);
    }

    #[test]
    fn a_filled_value_holds_for_its_clock_value_only() {
        let mut c = ReadCache::new(1 << 20);
        assert_eq!(c.lookup(b"k", 5), Lookup::Miss);
        c.fill(b"k", Some(b"v"), 5, false);
        assert_eq!(c.lookup(b"k", 5), Lookup::Hit(Some(b"v".to_vec())));
        assert_eq!(
            c.lookup(b"k", 6),
            Lookup::Stale,
            "unknown at the next second"
        );
        c.fill(b"k", Some(b"v"), 6, true);
        assert_eq!(
            c.lookup(b"k", 1_000),
            Lookup::Hit(Some(b"v".to_vec())),
            "a lasting value holds until a write"
        );
        c.fill(b"gone", None, 5, false);
        assert_eq!(c.lookup(b"gone", 1_000), Lookup::Hit(None), "absent holds");
        assert_eq!(
            c.counts(),
            CacheCounts {
                // Hits at 5, 1_000 (lasting) and "gone"; misses at 5 and 6.
                hits: 3,
                misses: 2,
                entries: 2,
                bytes: (1 + 1 + 4 + 2 * ENTRY_OVERHEAD) as u64,
            }
        );
    }

    #[test]
    fn writes_update_and_ranges_remove() {
        let mut c = ReadCache::new(1 << 20);
        for k in [b"a", b"b", b"c"] {
            c.fill(k, None, 0, false);
        }
        c.put(b"a", b"1", 0);
        c.put(b"b", b"2", 10);
        c.put(b"c", b"3", 0);
        assert_eq!(c.lookup(b"b", 9), Lookup::Hit(Some(b"2".to_vec())));
        assert_eq!(c.lookup(b"b", 10), Lookup::Hit(None), "expired");
        c.delete(b"a");
        assert_eq!(c.lookup(b"a", 0), Lookup::Hit(None));
        c.remove_range(b"a", b"c");
        assert_eq!(c.lookup(b"a", 0), Lookup::Miss);
        assert_eq!(c.lookup(b"b", 0), Lookup::Miss);
        assert_eq!(c.lookup(b"c", 0), Lookup::Hit(Some(b"3".to_vec())));
        c.remove_range(b"z", b"a");
        c.forget(b"c");
        assert_eq!(c.counts().entries, 0);
        assert_eq!(c.counts().bytes, 0);
    }

    #[test]
    fn a_zero_budget_turns_the_cache_off() {
        let mut c = ReadCache::new(0);
        c.put(b"a", b"1", 0);
        c.fill(b"b", None, 0, false);
        assert_eq!(c.lookup(b"a", 0), Lookup::Miss);
        assert_eq!(c.counts(), CacheCounts::default());
    }

    #[test]
    fn an_entry_larger_than_the_budget_is_not_kept() {
        let mut c = ReadCache::new(ENTRY_OVERHEAD + 4);
        c.fill(b"k", Some(b"ab"), 0, false);
        assert_eq!(c.counts().entries, 1);
        c.put(b"k", b"abcdefgh", 0);
        assert_eq!(c.lookup(b"k", 0), Lookup::Miss, "the old value is gone too");
        assert_eq!(c.counts().bytes, 0);
    }

    #[test]
    fn eviction_keeps_the_budget_and_spares_used_entries() {
        let per = 2 + 8 + ENTRY_OVERHEAD;
        let mut c = ReadCache::new(per * 4);
        for k in [b"k0", b"k1", b"k2", b"k3"] {
            c.fill(k, Some(&[0; 8]), 0, false);
        }
        // k0 is used, so it gets a second chance; k1 goes first.
        assert_ne!(c.lookup(b"k0", 0), Lookup::Miss);
        c.fill(b"k4", Some(&[0; 8]), 0, false);
        assert!(c.counts().bytes <= (per * 4) as u64);
        assert_eq!(c.lookup(b"k1", 0), Lookup::Miss);
        assert_ne!(c.lookup(b"k0", 0), Lookup::Miss);
        assert_ne!(c.lookup(b"k4", 0), Lookup::Miss);
    }

    #[test]
    fn a_write_updates_only_a_key_that_has_an_entry() {
        let mut c = ReadCache::new(1 << 20);
        c.put(b"new", b"v", 0);
        c.delete(b"gone");
        assert_eq!(c.counts().entries, 0, "writes add no key");
        c.fill(b"k", None, 0, false);
        c.put(b"k", b"v", 0);
        assert_eq!(c.lookup(b"k", 0), Lookup::Hit(Some(b"v".to_vec())));
        c.put(b"other", b"v", 0);
        assert_eq!(c.counts().entries, 1);
        c.delete(b"k");
        assert_eq!(c.lookup(b"k", 0), Lookup::Hit(None));
    }

    #[test]
    fn churn_keeps_the_queue_small() {
        let mut c = ReadCache::new(1 << 20);
        for i in 0..10_000u32 {
            let k = (i % 8).to_be_bytes();
            c.fill(&k, Some(b"v"), 0, false);
            c.forget(&k);
        }
        assert!(c.queue.len() <= 64 + 2 * c.map.len() + 1);
    }

    /// One step of the model test.
    #[derive(Debug, Clone)]
    enum Step {
        Put(u8, u8, u64),
        Delete(u8),
        Range(u8, u8),
        Get(u8),
        Tick(u64),
        Clear,
    }

    fn step() -> impl Strategy<Value = Step> {
        prop_oneof![
            (
                0u8..12,
                any::<u8>(),
                prop_oneof![Just(0u64), 1u64..40, Just(99u64)]
            )
                .prop_map(|(k, v, e)| Step::Put(k, v, e)),
            (0u8..12).prop_map(Step::Delete),
            (0u8..12, 0u8..12).prop_map(|(a, b)| Step::Range(a, b)),
            (0u8..12).prop_map(Step::Get),
            (0u64..4).prop_map(Step::Tick),
            Just(Step::Clear),
        ]
    }

    /// horton's answer in the model: the value if it is live at `now`.
    fn model_get(
        store: &BTreeMap<Vec<u8>, (Vec<u8>, u64)>,
        key: &[u8],
        now: u64,
    ) -> Option<Vec<u8>> {
        store
            .get(key)
            .filter(|(_, e)| visible(*e, now))
            .map(|(v, _)| v.clone())
    }

    proptest! {
        /// Under any sequence of writes, range deletes, gets, clock steps
        /// and evictions, a hit gives the model store's answer.
        #[test]
        fn a_hit_always_matches_the_store(
            steps in proptest::collection::vec(step(), 1..200),
            budget in prop_oneof![Just(0usize), 200usize..2_000],
        ) {
            let mut store: BTreeMap<Vec<u8>, (Vec<u8>, u64)> = BTreeMap::new();
            let mut cache = ReadCache::new(budget);
            let mut now = 1u64;
            for s in steps {
                match s {
                    Step::Put(k, v, ttl) => {
                        // 99 stands for the largest expiry tick.
                        let expire = match ttl {
                            0 => 0,
                            99 => u64::MAX,
                            t => now + t,
                        };
                        store.insert(vec![k], (vec![v; usize::from(k)], expire));
                        cache.put(&[k], &vec![v; usize::from(k)], expire);
                    }
                    Step::Delete(k) => {
                        store.remove(&vec![k]);
                        cache.delete(&[k]);
                    }
                    Step::Range(a, b) => {
                        store.retain(|key, _| !(key.as_slice() >= [a].as_slice() && key.as_slice() < [b].as_slice()));
                        cache.remove_range(&[a], &[b]);
                    }
                    Step::Get(k) => {
                        let want = model_get(&store, &[k], now);
                        match cache.lookup(&[k], now) {
                            Lookup::Hit(hit) => prop_assert_eq!(hit, want),
                            Lookup::Miss => cache.fill(&[k], want.as_deref(), now, false),
                            // The engine's rule: a hot key also reads at FAR.
                            Lookup::Stale => {
                                let lasting = want.is_some()
                                    && model_get(&store, &[k], FAR).is_some();
                                cache.fill(&[k], want.as_deref(), now, lasting);
                            }
                        }
                    }
                    Step::Tick(d) => now += d,
                    Step::Clear => cache.clear(),
                }
                prop_assert!(cache.counts().bytes <= budget as u64);
                for k in 0u8..12 {
                    if let Lookup::Hit(hit) = cache.lookup(&[k], now) {
                        prop_assert_eq!(hit, model_get(&store, &[k], now));
                    }
                }
            }
        }
    }
}
