//! Move-to-Front Transform for BZip2.
//!
//! MTF transforms a stream by replacing each byte with its position
//! in a dynamic list. After each byte, that byte is moved to the front
//! of the list. This converts local byte clusters into many zeros.
//!
//! The bzip2 pipeline always works over the block's *used-symbol* list
//! (recovered from the symbol map), so both helpers here take an explicit
//! alphabet and are fallible: a byte outside the alphabet, or an index
//! outside the list, is a caller bug or corrupted input and yields an
//! error instead of a silent skip or a panic.

use oxiarc_core::Result;
use oxiarc_core::error::OxiArcError;

/// MTF over a limited alphabet (the bzip2 used-symbol list).
///
/// `alphabet` must contain every byte value that occurs in `data`, in
/// ascending order, exactly as recovered from the block's symbol map.
/// A byte absent from the alphabet is an error (it would otherwise be
/// silently dropped, corrupting the round-trip).
pub fn transform_with_alphabet(data: &[u8], alphabet: &[u8]) -> Result<Vec<u8>> {
    if data.is_empty() {
        return Ok(Vec::new());
    }

    let mut list = alphabet.to_vec();
    let mut result = Vec::with_capacity(data.len());

    for &byte in data {
        let pos = list.iter().position(|&b| b == byte).ok_or_else(|| {
            OxiArcError::encoding_error("BZip2 MTF input byte missing from alphabet")
        })?;
        result.push(pos as u8);

        // Move to front
        if pos > 0 {
            list.copy_within(0..pos, 1);
            list[0] = byte;
        }
    }

    Ok(result)
}

/// Inverse MTF with limited alphabet (utility counterpart of
/// [`transform_with_alphabet`]).
///
/// An index at or beyond the alphabet length is corrupted input and
/// yields an error instead of a panic.
#[allow(dead_code)]
pub fn inverse_transform_with_alphabet(data: &[u8], alphabet: &[u8]) -> Result<Vec<u8>> {
    if data.is_empty() {
        return Ok(Vec::new());
    }

    let mut list = alphabet.to_vec();
    let mut result = Vec::with_capacity(data.len());

    for &pos in data {
        let pos = usize::from(pos);
        if pos >= list.len() {
            return Err(OxiArcError::corrupted(
                result.len() as u64,
                "BZip2 MTF index out of alphabet range",
            ));
        }
        let byte = list[pos];
        result.push(byte);

        // Move to front
        if pos > 0 {
            list.copy_within(0..pos, 1);
            list[0] = byte;
        }
    }

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Full ascending byte alphabet for tests that want 256-symbol MTF.
    fn full_alphabet() -> Vec<u8> {
        (0..=255).collect()
    }

    #[test]
    fn test_mtf_empty() {
        assert!(
            transform_with_alphabet(b"", &full_alphabet())
                .expect("empty transform")
                .is_empty()
        );
        assert!(
            inverse_transform_with_alphabet(b"", &full_alphabet())
                .expect("empty inverse")
                .is_empty()
        );
    }

    #[test]
    fn test_mtf_single() {
        let result = transform_with_alphabet(b"a", &full_alphabet()).expect("transform");
        assert_eq!(result, vec![b'a']); // 'a' is at position 97
    }

    #[test]
    fn test_mtf_repeated() {
        // Repeated bytes should produce zeros after the first
        let result = transform_with_alphabet(b"aaaa", &full_alphabet()).expect("transform");
        assert_eq!(result, vec![b'a', 0, 0, 0]); // First 'a' at pos 97, then 0s
    }

    #[test]
    fn test_mtf_roundtrip() {
        let test_cases = [
            b"hello".as_slice(),
            b"banana",
            b"abracadabra",
            b"the quick brown fox",
        ];

        for data in test_cases {
            let alphabet = full_alphabet();
            let transformed = transform_with_alphabet(data, &alphabet).expect("transform");
            let recovered =
                inverse_transform_with_alphabet(&transformed, &alphabet).expect("inverse");
            assert_eq!(recovered, data, "Failed for: {:?}", data);
        }
    }

    #[test]
    fn test_mtf_produces_low_values() {
        // After BWT, similar bytes are grouped, so MTF should produce many low values
        let data = b"bbbbbaaaacccc";
        let transformed = transform_with_alphabet(data, &full_alphabet()).expect("transform");

        // Count zeros
        let zeros = transformed.iter().filter(|&&b| b == 0).count();
        // Should have many zeros due to runs
        assert!(
            zeros > data.len() / 2,
            "MTF should produce many zeros for runs"
        );
    }

    #[test]
    fn test_mtf_with_alphabet() {
        let data = b"abab";
        let alphabet = *b"ab";
        let transformed = transform_with_alphabet(data, &alphabet).expect("transform");

        // 'a' at pos 0, 'b' at pos 1, 'a' at pos 1 (after 'b' moved front), 'b' at pos 1
        assert_eq!(transformed, vec![0, 1, 1, 1]);

        let recovered = inverse_transform_with_alphabet(&transformed, &alphabet).expect("inverse");
        assert_eq!(recovered, data.as_slice());
    }

    #[test]
    fn test_mtf_byte_outside_alphabet_is_error() {
        // 'c' is not in the alphabet: must be an error, not a silent skip.
        assert!(transform_with_alphabet(b"abcab", b"ab").is_err());
    }

    #[test]
    fn test_inverse_mtf_index_out_of_range_is_error() {
        // Index 2 exceeds the 2-symbol alphabet: must be an error, not a panic.
        assert!(inverse_transform_with_alphabet(&[0, 2], b"ab").is_err());
    }
}
