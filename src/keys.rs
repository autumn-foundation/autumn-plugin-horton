//! Key namespaces.
//!
//! App keys and plugin keys share one horton keyspace. Byte `0x00` starts
//! every plugin key, and the app API refuses such keys. Thus the two sets
//! are disjoint. `verification/keys.rs` proves this.

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

/// The plugin key for a session id.
///
/// # Errors
///
/// [`HortonError::KeyTooLarge`] when the id is too long, or
/// [`HortonError::EmptyKey`] when it is empty.
pub fn session_key(id: &str) -> Result<Vec<u8>, HortonError> {
    if id.is_empty() {
        return Err(HortonError::EmptyKey);
    }
    let len = 1 + SESSION_PREFIX.len() + id.len();
    if len > KEY_MAX {
        return Err(HortonError::KeyTooLarge { len, max: KEY_MAX });
    }
    let mut key = Vec::with_capacity(len);
    key.push(RESERVED);
    key.extend_from_slice(SESSION_PREFIX);
    key.extend_from_slice(id.as_bytes());
    Ok(key)
}

/// The text after [`RESERVED`] in a session key.
const SESSION_PREFIX: &[u8] = b"session/";

/// The lowest key an app scan can see.
const FLOOR: &[u8] = &[RESERVED + 1];

/// Raises a scan bound to the app floor. App scans never see plugin keys.
#[must_use]
pub fn app_floor(bound: &[u8]) -> &[u8] {
    if bound < FLOOR { FLOOR } else { bound }
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
        assert!(key.ends_with(b"abc"));
        assert_eq!(check_app_key(&key), Err(HortonError::ReservedKey));
        assert_eq!(session_key(""), Err(HortonError::EmptyKey));
        let long = "x".repeat(KEY_MAX);
        assert!(matches!(
            session_key(&long),
            Err(HortonError::KeyTooLarge { .. })
        ));
    }

    #[test]
    fn app_floor_skips_plugin_keys() {
        assert_eq!(app_floor(b""), &[RESERVED + 1]);
        assert_eq!(app_floor(&[RESERVED, 9]), &[RESERVED + 1]);
        assert_eq!(app_floor(b"a"), b"a");
    }

    proptest::proptest! {
        #[test]
        fn plugin_keys_are_never_app_keys(id in "[a-zA-Z0-9]{1,100}") {
            let key = session_key(&id).unwrap_or_default();
            proptest::prop_assert!(check_app_key(&key).is_err());
            proptest::prop_assert!(key.as_slice() < app_floor(b""));
        }
    }
}
