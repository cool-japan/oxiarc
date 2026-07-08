//! Property-based round-trip and no-panic tests for the DEFLATE codec.
//!
//! Complements the fixed-seed PRNG tests elsewhere in the crate with
//! proptest-driven exploration of the input space.

use oxiarc_deflate::{deflate, inflate};
use proptest::prelude::*;

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    /// `inflate(deflate(data)) == data` for arbitrary bytes at every valid
    /// compression level.
    #[test]
    fn roundtrip(
        data in prop::collection::vec(any::<u8>(), 0..4096),
        level in 0u8..=9,
    ) {
        let compressed = deflate(&data, level).expect("deflate must not fail on valid input");
        let decompressed = inflate(&compressed).expect("inflate must not fail on data we just produced");
        prop_assert_eq!(decompressed, data);
    }

    /// Feeding arbitrary (very likely invalid) bytes into `inflate` must
    /// only ever return `Ok` or `Err`, never panic.
    #[test]
    fn inflate_never_panics(data in prop::collection::vec(any::<u8>(), 0..4096)) {
        let result = std::panic::catch_unwind(|| inflate(&data));
        prop_assert!(result.is_ok(), "inflate() must not panic on arbitrary input");
    }
}
