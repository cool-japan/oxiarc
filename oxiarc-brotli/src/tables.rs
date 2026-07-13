//! Shared RFC 7932 constant tables used by both the encoder and the decoder.
//!
//! Everything in this module is pure data transcribed verbatim from the
//! specification, plus tiny helpers to map between values and (code, extra
//! bits) pairs:
//!
//! - insert-length codes (RFC 7932 Section 5),
//! - copy-length codes (Section 5),
//! - the insert-and-copy command cell table (Section 5),
//! - block-count codes (Section 6),
//! - the code-length-code variable-length code (Section 3.5).

/// `(base, extra_bits)` for the 24 insert-length codes (RFC 7932 Section 5).
pub const INSERT_LENGTH_CODES: [(u32, u8); 24] = [
    (0, 0),
    (1, 0),
    (2, 0),
    (3, 0),
    (4, 0),
    (5, 0),
    (6, 1),
    (8, 1),
    (10, 2),
    (14, 2),
    (18, 3),
    (26, 3),
    (34, 4),
    (50, 4),
    (66, 5),
    (98, 5),
    (130, 6),
    (194, 7),
    (322, 8),
    (578, 9),
    (1090, 10),
    (2114, 12),
    (6210, 14),
    (22594, 24),
];

/// `(base, extra_bits)` for the 24 copy-length codes (RFC 7932 Section 5).
pub const COPY_LENGTH_CODES: [(u32, u8); 24] = [
    (2, 0),
    (3, 0),
    (4, 0),
    (5, 0),
    (6, 0),
    (7, 0),
    (8, 0),
    (9, 0),
    (10, 1),
    (12, 1),
    (14, 2),
    (18, 2),
    (22, 3),
    (30, 3),
    (38, 4),
    (54, 4),
    (70, 5),
    (102, 5),
    (134, 6),
    (198, 7),
    (326, 8),
    (582, 9),
    (1094, 10),
    (2118, 24),
];

/// `(base, extra_bits)` for the 26 block-count codes (RFC 7932 Section 6).
pub const BLOCK_COUNT_CODES: [(u32, u8); 26] = [
    (1, 2),
    (5, 2),
    (9, 2),
    (13, 2),
    (17, 3),
    (25, 3),
    (33, 3),
    (41, 3),
    (49, 4),
    (65, 4),
    (81, 4),
    (97, 4),
    (113, 5),
    (145, 5),
    (177, 5),
    (209, 5),
    (241, 6),
    (305, 6),
    (369, 7),
    (497, 8),
    (753, 9),
    (1265, 10),
    (2289, 11),
    (4337, 12),
    (8433, 13),
    (16625, 24),
];

/// Insert-length code range start for each of the 9 explicit-distance cells
/// of the insert-and-copy command table (RFC 7932 Section 5).
const INSERT_RANGE_LUT: [u16; 9] = [0, 0, 8, 8, 0, 16, 8, 16, 16];

/// Copy-length code range start for each of the 9 explicit-distance cells.
const COPY_RANGE_LUT: [u16; 9] = [0, 8, 0, 8, 16, 0, 16, 8, 16];

/// Decompose an insert-and-copy command symbol (0..704) per RFC 7932
/// Section 5 into `(insert_length_code, copy_length_code, implicit_distance_zero)`.
///
/// Symbols 0..127 additionally imply distance code 0 (the last distance is
/// reused and **no** distance symbol follows in the stream).
pub fn decompose_command(symbol: u16) -> (u16, u16, bool) {
    let cell = symbol >> 6; // 0..=10 for symbol < 704
    let (ins_off, copy_off, implicit) = if cell < 2 {
        // Cells 0 and 1: implicit distance code zero.
        (0u16, cell * 8, true)
    } else {
        let idx = (cell - 2) as usize;
        (INSERT_RANGE_LUT[idx], COPY_RANGE_LUT[idx], false)
    };
    let ins_code = ins_off + ((symbol >> 3) & 7);
    let copy_code = copy_off + (symbol & 7);
    (ins_code, copy_code, implicit)
}

/// Compose an insert-and-copy command symbol from an insert-length code
/// (0..24), copy-length code (0..24), and the implicit-distance-zero flag.
///
/// The exact inverse of [`decompose_command`]. `implicit_zero` is only
/// representable when `ins_code < 8 && copy_code < 16`; if requested outside
/// that range, an explicit-distance cell is used instead.
pub fn compose_command(ins_code: u16, copy_code: u16, implicit_zero: bool) -> u16 {
    let low = ((ins_code & 7) << 3) | (copy_code & 7);
    if implicit_zero && ins_code < 8 && copy_code < 16 {
        // Cells 0 (copy 0..7) and 1 (copy 8..15).
        return ((copy_code >> 3) << 6) | low;
    }
    // Explicit-distance cells 2..=10, keyed by (ins_code / 8, copy_code / 8).
    let cell: u16 = match (ins_code >> 3, copy_code >> 3) {
        (0, 0) => 2,
        (0, 1) => 3,
        (1, 0) => 4,
        (1, 1) => 5,
        (0, 2) => 6,
        (2, 0) => 7,
        (1, 2) => 8,
        (2, 1) => 9,
        _ => 10,
    };
    (cell << 6) | low
}

/// The fixed order in which code lengths of the code-length alphabet are
/// stored in a complex prefix-code descriptor (RFC 7932 Section 3.5).
pub const CODE_LENGTH_CODE_ORDER: [usize; 18] =
    [1, 2, 3, 4, 0, 5, 17, 6, 16, 7, 8, 9, 10, 11, 12, 13, 14, 15];

/// Bit length of the code-length-code VLC entry indexed by the next 4 stream
/// bits (LSB-first), per RFC 7932 Section 3.5.
pub const CODE_LENGTH_PREFIX_LENGTH: [u8; 16] = [2, 2, 2, 3, 2, 2, 2, 4, 2, 2, 2, 3, 2, 2, 2, 4];

/// Decoded value of the code-length-code VLC entry indexed by the next 4
/// stream bits (LSB-first).
pub const CODE_LENGTH_PREFIX_VALUE: [u8; 16] = [0, 4, 3, 2, 0, 4, 3, 1, 0, 4, 3, 2, 0, 4, 3, 5];

/// `(bit_pattern, bit_count)` to WRITE a code-length-code length value 0..=5,
/// LSB-first. The exact inverse of the read tables above.
pub const CODE_LENGTH_VALUE_WRITE: [(u32, u32); 6] = [
    (0b00, 2),   // 0
    (0b0111, 4), // 1
    (0b011, 3),  // 2
    (0b10, 2),   // 3
    (0b01, 2),   // 4
    (0b1111, 4), // 5
];

/// Find the insert-length code for an insert length. Returns
/// `(code, extra_bits, base)`.
pub fn insert_length_to_code(len: u32) -> (u16, u8, u32) {
    // The table is monotonically increasing; scan (24 entries, negligible).
    for (i, &(base, extra)) in INSERT_LENGTH_CODES.iter().enumerate().rev() {
        if len >= base {
            return (i as u16, extra, base);
        }
    }
    (0, 0, 0)
}

/// Find the copy-length code for a copy length (>= 2). Returns
/// `(code, extra_bits, base)`.
pub fn copy_length_to_code(len: u32) -> (u16, u8, u32) {
    for (i, &(base, extra)) in COPY_LENGTH_CODES.iter().enumerate().rev() {
        if len >= base {
            return (i as u16, extra, base);
        }
    }
    (0, 0, 2)
}

/// Find the block-count code for a block count (1..=16793840). Returns
/// `(code, extra_bits, base)`.
pub fn block_count_to_code(count: u32) -> (u16, u8, u32) {
    for (i, &(base, extra)) in BLOCK_COUNT_CODES.iter().enumerate().rev() {
        if count >= base {
            return (i as u16, extra, base);
        }
    }
    (0, 2, 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_command_decompose_compose_roundtrip() {
        for symbol in 0u16..704 {
            let (ins, copy, implicit) = decompose_command(symbol);
            assert!(ins < 24, "symbol {symbol}: insert code {ins} out of range");
            assert!(copy < 24, "symbol {symbol}: copy code {copy} out of range");
            // Implicit-zero cells only exist for insert 0..7 / copy 0..15.
            if implicit {
                assert!(ins < 8 && copy < 16, "symbol {symbol}: bad implicit cell");
            }
            let recomposed = compose_command(ins, copy, implicit);
            assert_eq!(recomposed, symbol, "symbol {symbol} did not round-trip");
        }
    }

    #[test]
    fn test_command_cells_match_rfc_table() {
        // Spot checks straight from the RFC Section 5 cell table.
        assert_eq!(decompose_command(0), (0, 0, true));
        assert_eq!(decompose_command(63), (7, 7, true));
        assert_eq!(decompose_command(64), (0, 8, true));
        assert_eq!(decompose_command(127), (7, 15, true));
        assert_eq!(decompose_command(128), (0, 0, false));
        assert_eq!(decompose_command(255), (7, 15, false));
        assert_eq!(decompose_command(256), (8, 0, false));
        assert_eq!(decompose_command(384), (0, 16, false));
        assert_eq!(decompose_command(448), (16, 0, false));
        assert_eq!(decompose_command(512), (8, 16, false));
        assert_eq!(decompose_command(576), (16, 8, false));
        assert_eq!(decompose_command(640), (16, 16, false));
        assert_eq!(decompose_command(703), (23, 23, false));
    }

    #[test]
    fn test_length_code_tables_are_contiguous() {
        // Each code's range must end exactly where the next one begins.
        for w in INSERT_LENGTH_CODES.windows(2) {
            let (base, extra) = w[0];
            let (next_base, _) = w[1];
            assert_eq!(base + (1 << extra), next_base, "insert table gap");
        }
        for w in COPY_LENGTH_CODES.windows(2) {
            let (base, extra) = w[0];
            let (next_base, _) = w[1];
            assert_eq!(base + (1 << extra), next_base, "copy table gap");
        }
        for w in BLOCK_COUNT_CODES.windows(2) {
            let (base, extra) = w[0];
            let (next_base, _) = w[1];
            assert_eq!(base + (1 << extra), next_base, "block count table gap");
        }
        // RFC-stated endpoints.
        assert_eq!(INSERT_LENGTH_CODES[23].0 + (1 << 24) - 1, 16_799_809);
        assert_eq!(COPY_LENGTH_CODES[23].0 + (1 << 24) - 1, 16_779_333);
        assert_eq!(BLOCK_COUNT_CODES[25].0 + (1 << 24) - 1, 16_793_840);
    }

    #[test]
    fn test_value_to_code_inverse() {
        for len in [0u32, 1, 5, 6, 7, 9, 129, 130, 22594, 100_000] {
            let (code, extra, base) = insert_length_to_code(len);
            assert!(len >= base && len - base < (1 << extra), "insert {len}");
            assert_eq!(INSERT_LENGTH_CODES[code as usize].0, base);
        }
        for len in [2u32, 3, 9, 10, 69, 70, 2117, 2118, 1_000_000] {
            let (code, extra, base) = copy_length_to_code(len);
            assert!(len >= base && len - base < (1 << extra), "copy {len}");
            assert_eq!(COPY_LENGTH_CODES[code as usize].0, base);
        }
        for count in [1u32, 4, 5, 16624, 16625, 40_000] {
            let (code, extra, base) = block_count_to_code(count);
            assert!(
                count >= base && count - base < (1 << extra),
                "count {count}"
            );
            assert_eq!(BLOCK_COUNT_CODES[code as usize].0, base);
        }
    }

    #[test]
    fn test_code_length_vlc_tables_are_inverse() {
        // Writing any value 0..=5 and re-reading through the 4-bit peek tables
        // must return the same value and consume the same number of bits.
        for value in 0u8..=5 {
            let (pattern, nbits) = CODE_LENGTH_VALUE_WRITE[value as usize];
            // Extend the pattern to 4 bits with arbitrary garbage above nbits.
            for garbage in 0u32..(1 << (4 - nbits)) {
                let peeked = (pattern | (garbage << nbits)) & 0xF;
                assert_eq!(
                    CODE_LENGTH_PREFIX_VALUE[peeked as usize], value,
                    "value {value} pattern {pattern:04b}"
                );
                assert_eq!(
                    CODE_LENGTH_PREFIX_LENGTH[peeked as usize] as u32, nbits,
                    "value {value} length"
                );
            }
        }
    }
}
