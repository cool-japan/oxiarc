//! Fixed Huffman code tables for DEFLATE (RFC 1951).
//!
//! DEFLATE specifies fixed Huffman codes that can be used instead of
//! transmitting custom codes. These provide a balance between compression
//! ratio and encoding overhead.

use crate::huffman::HuffmanTree;
use oxiarc_core::error::Result;
use std::sync::OnceLock;

/// Fixed literal/length code lengths (RFC 1951 Section 3.2.6).
///
/// - Symbols 0-143: 8 bits
/// - Symbols 144-255: 9 bits
/// - Symbols 256-279: 7 bits
/// - Symbols 280-287: 8 bits
pub fn fixed_litlen_lengths() -> [u8; 288] {
    let mut lengths = [0u8; 288];

    // 0-143: 8 bits
    for len in lengths.iter_mut().take(144) {
        *len = 8;
    }

    // 144-255: 9 bits
    for len in lengths.iter_mut().take(256).skip(144) {
        *len = 9;
    }

    // 256-279: 7 bits
    for len in lengths.iter_mut().take(280).skip(256) {
        *len = 7;
    }

    // 280-287: 8 bits
    for len in lengths.iter_mut().take(288).skip(280) {
        *len = 8;
    }

    lengths
}

/// Fixed distance code lengths (RFC 1951 Section 3.2.6).
///
/// All 30 distance codes use 5 bits.
pub fn fixed_distance_lengths() -> [u8; 30] {
    [5u8; 30]
}

/// Get the fixed literal/length Huffman tree.
///
/// This tree is cached after first construction.
pub fn fixed_litlen_tree() -> Result<&'static HuffmanTree> {
    static TREE: OnceLock<HuffmanTree> = OnceLock::new();

    Ok(TREE.get_or_init(|| {
        HuffmanTree::from_code_lengths(&fixed_litlen_lengths())
            .expect("Fixed litlen tree construction should never fail")
    }))
}

/// Get the fixed distance Huffman tree.
///
/// This tree is cached after first construction.
pub fn fixed_distance_tree() -> Result<&'static HuffmanTree> {
    static TREE: OnceLock<HuffmanTree> = OnceLock::new();

    Ok(TREE.get_or_init(|| {
        HuffmanTree::from_code_lengths(&fixed_distance_lengths())
            .expect("Fixed distance tree construction should never fail")
    }))
}

/// Length code base values (RFC 1951 Section 3.2.5).
///
/// For length codes 257-285, this gives the base length value.
/// Extra bits are added to get the final length.
pub const LENGTH_BASE: [u16; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, // 257-264: 0 extra bits
    11, 13, 15, 17, // 265-268: 1 extra bit
    19, 23, 27, 31, // 269-272: 2 extra bits
    35, 43, 51, 59, // 273-276: 3 extra bits
    67, 83, 99, 115, // 277-280: 4 extra bits
    131, 163, 195, 227, // 281-284: 5 extra bits
    258, // 285: 0 extra bits (special case)
];

/// Number of extra bits for length codes 257-285.
pub const LENGTH_EXTRA_BITS: [u8; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, // 257-264
    1, 1, 1, 1, // 265-268
    2, 2, 2, 2, // 269-272
    3, 3, 3, 3, // 273-276
    4, 4, 4, 4, // 277-280
    5, 5, 5, 5, // 281-284
    0, // 285
];

/// Distance code base values (RFC 1951 Section 3.2.5).
///
/// For distance codes 0-29, this gives the base distance value.
pub const DISTANCE_BASE: [u16; 30] = [
    1, 2, 3, 4, // 0-3: 0 extra bits
    5, 7, // 4-5: 1 extra bit
    9, 13, // 6-7: 2 extra bits
    17, 25, // 8-9: 3 extra bits
    33, 49, // 10-11: 4 extra bits
    65, 97, // 12-13: 5 extra bits
    129, 193, // 14-15: 6 extra bits
    257, 385, // 16-17: 7 extra bits
    513, 769, // 18-19: 8 extra bits
    1025, 1537, // 20-21: 9 extra bits
    2049, 3073, // 22-23: 10 extra bits
    4097, 6145, // 24-25: 11 extra bits
    8193, 12289, // 26-27: 12 extra bits
    16385, 24577, // 28-29: 13 extra bits
];

/// Number of extra bits for distance codes 0-29.
pub const DISTANCE_EXTRA_BITS: [u8; 30] = [
    0, 0, 0, 0, // 0-3
    1, 1, // 4-5
    2, 2, // 6-7
    3, 3, // 8-9
    4, 4, // 10-11
    5, 5, // 12-13
    6, 6, // 14-15
    7, 7, // 16-17
    8, 8, // 18-19
    9, 9, // 20-21
    10, 10, // 22-23
    11, 11, // 24-25
    12, 12, // 26-27
    13, 13, // 28-29
];

/// Order of code length codes in dynamic block header.
///
/// Code length codes are transmitted in this order (RFC 1951 Section 3.2.7).
pub const CODE_LENGTH_ORDER: [usize; 19] = [
    16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
];

/// Convert a length value (3-258) to a length code (257-285).
pub fn length_to_code(length: u16) -> (u16, u8, u16) {
    debug_assert!(
        (3..=258).contains(&length),
        "Length out of range: {}",
        length
    );

    let length = length as usize;

    // Binary search for the appropriate code
    let code = match length {
        3..=10 => length - 3 + 257,
        11..=18 => (length - 11) / 2 + 265,
        19..=34 => (length - 19) / 4 + 269,
        35..=66 => (length - 35) / 8 + 273,
        67..=130 => (length - 67) / 16 + 277,
        131..=257 => (length - 131) / 32 + 281,
        258 => 285,
        _ => unreachable!(),
    };

    let base = LENGTH_BASE[code - 257] as usize;
    let extra_bits = LENGTH_EXTRA_BITS[code - 257];
    let extra_value = (length - base) as u16;

    (code as u16, extra_bits, extra_value)
}

/// Convert a distance value (1-32768) to a distance code (0-29).
pub fn distance_to_code(distance: u16) -> (u16, u8, u16) {
    debug_assert!(
        (1..=32768).contains(&distance),
        "Distance out of range: {}",
        distance
    );

    // Find the appropriate code by searching the DISTANCE_BASE table
    let code = match distance {
        1 => 0,
        2 => 1,
        3 => 2,
        4 => 3,
        5..=6 => 4,
        7..=8 => 5,
        9..=12 => 6,
        13..=16 => 7,
        17..=24 => 8,
        25..=32 => 9,
        33..=48 => 10,
        49..=64 => 11,
        65..=96 => 12,
        97..=128 => 13,
        129..=192 => 14,
        193..=256 => 15,
        257..=384 => 16,
        385..=512 => 17,
        513..=768 => 18,
        769..=1024 => 19,
        1025..=1536 => 20,
        1537..=2048 => 21,
        2049..=3072 => 22,
        3073..=4096 => 23,
        4097..=6144 => 24,
        6145..=8192 => 25,
        8193..=12288 => 26,
        12289..=16384 => 27,
        16385..=24576 => 28,
        _ => 29, // 24577..=32768
    };

    let base = DISTANCE_BASE[code];
    let extra_bits = DISTANCE_EXTRA_BITS[code];
    let extra_value = distance - base;

    (code as u16, extra_bits, extra_value)
}

/// Decode a length from a length code and extra bits.
pub fn decode_length(code: u16, extra: u16) -> u16 {
    debug_assert!((257..=285).contains(&code), "Invalid length code: {}", code);
    LENGTH_BASE[(code - 257) as usize] + extra
}

/// Decode a distance from a distance code and extra bits.
pub fn decode_distance(code: u16, extra: u16) -> u16 {
    debug_assert!(code < 30, "Invalid distance code: {}", code);
    DISTANCE_BASE[code as usize] + extra
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fixed_litlen_lengths() {
        let lengths = fixed_litlen_lengths();

        // Check some known values
        assert_eq!(lengths[0], 8);
        assert_eq!(lengths[143], 8);
        assert_eq!(lengths[144], 9);
        assert_eq!(lengths[255], 9);
        assert_eq!(lengths[256], 7); // End of block
        assert_eq!(lengths[279], 7);
        assert_eq!(lengths[280], 8);
        assert_eq!(lengths[287], 8);
    }

    #[test]
    fn test_fixed_distance_lengths() {
        let lengths = fixed_distance_lengths();
        assert!(lengths.iter().all(|&l| l == 5));
    }

    #[test]
    fn test_fixed_trees() {
        // These should not fail
        let _ = fixed_litlen_tree().unwrap();
        let _ = fixed_distance_tree().unwrap();
    }

    #[test]
    fn test_length_to_code_roundtrip() {
        for length in 3..=258 {
            let (code, extra_bits, extra_value) = length_to_code(length);
            let decoded = decode_length(code, extra_value);
            assert_eq!(
                decoded, length,
                "Roundtrip failed for length {}: code={}, extra_bits={}, extra_value={}",
                length, code, extra_bits, extra_value
            );
        }
    }

    #[test]
    fn test_distance_to_code_roundtrip() {
        for distance in 1..=32768u16 {
            let (code, extra_bits, extra_value) = distance_to_code(distance);
            let decoded = decode_distance(code, extra_value);
            assert_eq!(
                decoded, distance,
                "Roundtrip failed for distance {}: code={}, extra_bits={}, extra_value={}",
                distance, code, extra_bits, extra_value
            );
        }
    }

    #[test]
    fn test_specific_lengths() {
        // Test some specific length encodings
        assert_eq!(length_to_code(3), (257, 0, 0));
        assert_eq!(length_to_code(10), (264, 0, 0));
        assert_eq!(length_to_code(11), (265, 1, 0));
        assert_eq!(length_to_code(12), (265, 1, 1));
        assert_eq!(length_to_code(258), (285, 0, 0));
    }

    #[test]
    fn test_specific_distances() {
        // Test some specific distance encodings
        assert_eq!(distance_to_code(1), (0, 0, 0));
        assert_eq!(distance_to_code(4), (3, 0, 0));
        assert_eq!(distance_to_code(5), (4, 1, 0));
        assert_eq!(distance_to_code(6), (4, 1, 1));
        assert_eq!(distance_to_code(32768), (29, 13, 8191));
    }
}
