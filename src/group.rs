//! Group-commit sizing.
//!
//! The store thread adds queued writes to one group while the group fits
//! one WAL block. `verification/group.rs` proves this check is exact and
//! does not overflow.

/// True when a group of `bytes` and `ops` can take `add_bytes` and
/// `add_ops` more and stay within `max_bytes` and `max_ops`.
#[must_use]
pub const fn fits(
    bytes: usize,
    ops: usize,
    add_bytes: usize,
    add_ops: usize,
    max_bytes: usize,
    max_ops: usize,
) -> bool {
    bytes <= max_bytes
        && add_bytes <= max_bytes - bytes
        && ops <= max_ops
        && add_ops <= max_ops - ops
}

#[cfg(test)]
mod tests {
    use super::fits;

    #[test]
    fn edges() {
        assert!(fits(0, 0, 10, 1, 10, 1));
        assert!(!fits(0, 0, 11, 1, 10, 1));
        assert!(!fits(0, 1, 1, 1, 10, 1));
        assert!(!fits(11, 0, 0, 0, 10, 1));
        assert!(!fits(1, 0, usize::MAX, 0, 10, 1));
    }

    proptest::proptest! {
        #[test]
        fn matches_wide_arithmetic(
            b in 0usize..2000, o in 0usize..200,
            ab: usize, ao in 0usize..200,
            mb in 0usize..2000, mo in 0usize..200,
        ) {
            let want = b as u128 + ab as u128 <= mb as u128
                && o as u128 + ao as u128 <= mo as u128;
            proptest::prop_assert_eq!(fits(b, o, ab, ao, mb, mo), want);
        }
    }
}
