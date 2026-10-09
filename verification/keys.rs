use vstd::prelude::*;

verus! {

// Model of `src/keys.rs`. App keys and plugin keys share one horton
// keyspace. The proofs show the two sets are disjoint and that every
// plugin key sorts before every app key, so an app scan that starts at
// `[0x01]` never sees a plugin key (R6).

pub const KEY_MAX: usize = 128;
pub const RESERVED: u8 = 0;

/// A key the app API accepts.
pub open spec fn is_app_key(k: Seq<u8>) -> bool {
    0 < k.len() <= KEY_MAX && k[0] != RESERVED
}

/// A key the plugin writes for itself.
pub open spec fn is_plugin_key(k: Seq<u8>) -> bool {
    0 < k.len() && k[0] == RESERVED
}

/// The plugin key for a session id: `[0x00] ++ prefix ++ id`.
pub open spec fn session_key(prefix: Seq<u8>, id: Seq<u8>) -> Seq<u8> {
    seq![RESERVED] + prefix + id
}

/// Byte-wise lexicographic order, as horton compares keys.
pub open spec fn lex_lt(a: Seq<u8>, b: Seq<u8>) -> bool
    decreases a.len(),
{
    if b.len() == 0 {
        false
    } else if a.len() == 0 {
        true
    } else if a[0] != b[0] {
        a[0] < b[0]
    } else {
        lex_lt(a.drop_first(), b.drop_first())
    }
}

/// The result of the runtime check.
pub enum KeyCheck {
    Accepted,
    Empty,
    TooLarge,
    Reserved,
}

/// `check_app_key`: the same branch order as the runtime fn.
pub fn check_app_key(key: &[u8]) -> (r: KeyCheck)
    ensures
        (r is Accepted) <==> is_app_key(key@),
        (r is Empty) <==> key@.len() == 0,
{
    if key.len() == 0 {
        KeyCheck::Empty
    } else if key.len() > KEY_MAX {
        KeyCheck::TooLarge
    } else if key[0] == RESERVED {
        KeyCheck::Reserved
    } else {
        KeyCheck::Accepted
    }
}

/// Every session key is a plugin key.
pub proof fn session_keys_are_plugin_keys(prefix: Seq<u8>, id: Seq<u8>)
    ensures is_plugin_key(session_key(prefix, id)),
{
    assert(session_key(prefix, id)[0] == RESERVED);
}

/// No key is both a plugin key and an app key.
pub proof fn namespaces_are_disjoint(k: Seq<u8>)
    ensures !(is_plugin_key(k) && is_app_key(k)),
{}

/// Every plugin key sorts before `[0x01]`, and `[0x01]` sorts at or
/// before every app key. So the app scan floor `[0x01]` hides plugin keys.
pub proof fn plugin_keys_sort_below_the_floor(p: Seq<u8>)
    requires is_plugin_key(p),
    ensures lex_lt(p, seq![1u8]),
{
    assert(seq![1u8][0] == 1u8);
}

pub proof fn the_floor_sorts_at_or_below_app_keys(k: Seq<u8>)
    requires is_app_key(k),
    ensures k[0] >= 1, !lex_lt(k, seq![1u8]),
{
    let floor = seq![1u8];
    assert(floor.len() == 1);
    if k[0] != floor[0] {
        assert(k[0] > 1u8);
        assert(!lex_lt(k, floor));
    } else {
        assert(k.drop_first().len() + 1 == k.len());
        assert(floor.drop_first().len() == 0);
        assert(!lex_lt(k.drop_first(), floor.drop_first()));
        assert(!lex_lt(k, floor));
    }
}

fn main() {}

} // verus!
