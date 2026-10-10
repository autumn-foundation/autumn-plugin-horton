//! Key namespaces.
//!
//! App keys and plugin keys share one horton keyspace. Byte `0x00` starts
//! every plugin key, and the app API refuses such keys. Thus the two sets
//! are disjoint. `verification/keys.rs` proves this.
//!
//! Plugin keys:
//!
//! | Key | Value |
//! |---|---|
//! | `0x00 "clock"` | The store clock, 8 bytes big-endian |
//! | `0x00 "session/" <SHA-256 of the id>` | The session JSON, with a TTL |
//! | `0x00 "sexp/" <expiry, 8 bytes big-endian> <SHA-256 of the id>` | Empty: the sweep index |
//!
//! The store holds a hash of each session id, not the id. A copy of the
//! file does not give a usable session cookie.

use crate::error::HortonError;

/// The longest key, in bytes.
pub const KEY_MAX: usize = 128;

/// The longest value, in bytes.
pub const VAL_MAX: usize = 4096;

/// The first byte of every plugin key.
pub const RESERVED: u8 = 0x00;

/// Checks an app key.
///
/// # Errors
///
/// [`HortonError::EmptyKey`], [`HortonError::KeyTooLarge`] or
/// [`HortonError::ReservedKey`].
pub const fn check_app_key(key: &[u8]) -> Result<(), HortonError> {
    // The branch order is the one `verification/keys.rs` proves.
    if key.is_empty() {
        Err(HortonError::EmptyKey)
    } else if key.len() > KEY_MAX {
        Err(HortonError::KeyTooLarge {
            len: key.len(),
            max: KEY_MAX,
        })
    } else if key[0] == RESERVED {
        Err(HortonError::ReservedKey)
    } else {
        Ok(())
    }
}

/// Checks a value.
///
/// # Errors
///
/// [`HortonError::ValueTooLarge`].
pub const fn check_value(val: &[u8]) -> Result<(), HortonError> {
    if val.len() > VAL_MAX {
        Err(HortonError::ValueTooLarge {
            len: val.len(),
            max: VAL_MAX,
        })
    } else {
        Ok(())
    }
}

/// The plugin key for a session id. Every id of any length has a key.
///
/// # Errors
///
/// [`HortonError::EmptyKey`] when the id is empty.
pub fn session_key(id: &str) -> Result<Vec<u8>, HortonError> {
    if id.is_empty() {
        return Err(HortonError::EmptyKey);
    }
    Ok(session_key_bytes(&session_hash(id)))
}

/// The prefix of every session key.
pub const SESSION: &[u8] = b"\0session/";

/// The prefix of every session index key.
pub const SESSION_INDEX: &[u8] = b"\0sexp/";

/// The key of the store clock.
pub const CLOCK_KEY: &[u8] = b"\0clock";

/// Bytes of a session id hash.
pub const SESSION_HASH: usize = 32;

/// The SHA-256 of a session id.
#[must_use]
pub fn session_hash(id: &str) -> [u8; SESSION_HASH] {
    use sha2::Digest;
    sha2::Sha256::digest(id.as_bytes()).into()
}

/// The session key for an id hash.
#[must_use]
pub fn session_key_bytes(hash: &[u8]) -> Vec<u8> {
    [SESSION, hash].concat()
}

/// The index key for a session (by id hash) that expires at `expire`.
#[must_use]
pub fn session_index_key(expire: u64, hash: &[u8]) -> Vec<u8> {
    [SESSION_INDEX, &expire.to_be_bytes()[..], hash].concat()
}

/// The exclusive end of the index entries that expire at or before `now`.
#[must_use]
pub fn session_index_end(now: u64) -> Vec<u8> {
    [SESSION_INDEX, &now.saturating_add(1).to_be_bytes()[..]].concat()
}

/// The session id hash in an index key, or `None` for another key.
#[must_use]
pub fn session_id_of_index(key: &[u8]) -> Option<&[u8]> {
    key.strip_prefix(SESSION_INDEX)
        .and_then(|rest| rest.get(8..))
        .filter(|hash| hash.len() == SESSION_HASH)
}

/// The lowest key an app scan can see.
const FLOOR: &[u8] = &[RESERVED + 1];

/// Raises a scan bound to the app floor. App scans never see plugin keys.
/// `verification/keys.rs` models the test as `below_floor`.
#[must_use]
pub const fn app_floor(bound: &[u8]) -> &[u8] {
    if bound.is_empty() || bound[0] == RESERVED {
        FLOOR
    } else {
        bound
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_key_limits() {
        assert_eq!(check_app_key(b"a"), Ok(()));
        assert_eq!(check_app_key(&[b'k'; KEY_MAX]), Ok(()));
        assert_eq!(check_app_key(b""), Err(HortonError::EmptyKey));
        assert_eq!(
            check_app_key(&[b'k'; KEY_MAX + 1]),
            Err(HortonError::KeyTooLarge {
                len: KEY_MAX + 1,
                max: KEY_MAX
            })
        );
        assert_eq!(
            check_app_key(&[RESERVED, b'x']),
            Err(HortonError::ReservedKey)
        );
    }

    #[test]
    fn value_limits() {
        assert_eq!(check_value(b""), Ok(()));
        assert_eq!(check_value(&[0; VAL_MAX]), Ok(()));
        assert_eq!(
            check_value(&[0; VAL_MAX + 1]),
            Err(HortonError::ValueTooLarge {
                len: VAL_MAX + 1,
                max: VAL_MAX
            })
        );
    }

    #[test]
    fn session_keys_are_reserved_and_bounded() {
        let key = session_key("abc").unwrap_or_default();
        assert_eq!(key.first(), Some(&RESERVED));
        assert!(key.ends_with(&session_hash("abc")));
        assert!(!key.windows(3).any(|w| w == b"abc"), "the id is not stored");
        assert_eq!(check_app_key(&key), Err(HortonError::ReservedKey));
        assert_eq!(session_key(""), Err(HortonError::EmptyKey));
        let long = session_key(&"x".repeat(10_000)).unwrap_or_default();
        assert_eq!(long.len(), SESSION.len() + SESSION_HASH);
        assert_ne!(session_key("a"), session_key("b"));
        assert!(session_index_key(u64::MAX, &[0; SESSION_HASH]).len() <= KEY_MAX);
    }

    #[test]
    fn index_keys_round_trip_and_order_by_expiry() {
        let h = session_hash("abc");
        let k = session_index_key(7, &h);
        assert_eq!(session_id_of_index(&k), Some(&h[..]));
        assert!(k < session_index_end(7));
        assert!(k >= session_index_end(6));
        assert_eq!(session_id_of_index(b"\0sexp/short"), None);
        assert_eq!(session_id_of_index(&session_index_key(1, b"")), None);
        assert_eq!(session_id_of_index(b"app"), None);
        for k in [CLOCK_KEY, SESSION, SESSION_INDEX] {
            assert_eq!(check_app_key(k), Err(HortonError::ReservedKey));
        }
    }

    #[test]
    fn app_floor_skips_plugin_keys() {
        assert_eq!(app_floor(b""), &[RESERVED + 1]);
        assert_eq!(app_floor(&[RESERVED, 9]), &[RESERVED + 1]);
        assert_eq!(app_floor(&[RESERVED + 1]), &[RESERVED + 1]);
        assert_eq!(app_floor(b"a"), b"a");
    }

    proptest::proptest! {
        #[test]
        fn plugin_keys_are_never_app_keys(id in "\\PC{1,200}") {
            let key = session_key(&id).unwrap_or_default();
            proptest::prop_assert!(check_app_key(&key).is_err());
            proptest::prop_assert!(key.as_slice() < app_floor(b""));
        }
    }
}
