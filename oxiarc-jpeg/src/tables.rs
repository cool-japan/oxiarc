//! Static tables from ITU-T T.81: the zig-zag order and the Annex K
//! example quantisation and Huffman tables.
//!
//! The Annex K tables are what every reference encoder emits at its default
//! settings, so reproducing them exactly is a prerequisite for byte-identical
//! interoperability with `cjpeg` and `libtiff`'s `JPEGTables` tag.

/// Zig-zag scan order: `ZIGZAG_TO_NATURAL[k]` is the index inside an 8x8 block,
/// in row-major order, of the `k`-th coefficient in the zig-zag sequence.
pub const ZIGZAG_TO_NATURAL: [usize; 64] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27, 20,
    13, 6, 7, 14, 21, 28, 35, 42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51, 58, 59,
    52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
];

/// Inverse of [`ZIGZAG_TO_NATURAL`]: `NATURAL_TO_ZIGZAG[i]` is the zig-zag
/// position of the row-major index `i`.
pub const NATURAL_TO_ZIGZAG: [usize; 64] = [
    0, 1, 5, 6, 14, 15, 27, 28, 2, 4, 7, 13, 16, 26, 29, 42, 3, 8, 12, 17, 25, 30, 41, 43, 9, 11,
    18, 24, 31, 40, 44, 53, 10, 19, 23, 32, 39, 45, 52, 54, 20, 22, 33, 38, 46, 51, 55, 60, 21, 34,
    37, 47, 50, 56, 59, 61, 35, 36, 48, 49, 57, 58, 62, 63,
];

/// Annex K.1: the example luminance quantisation table, in row-major order.
pub const ANNEX_K_LUMA_QUANT: [u16; 64] = [
    16, 11, 10, 16, 24, 40, 51, 61, //
    12, 12, 14, 19, 26, 58, 60, 55, //
    14, 13, 16, 24, 40, 57, 69, 56, //
    14, 17, 22, 29, 51, 87, 80, 62, //
    18, 22, 37, 56, 68, 109, 103, 77, //
    24, 35, 55, 64, 81, 104, 113, 92, //
    49, 64, 78, 87, 103, 121, 120, 101, //
    72, 92, 95, 98, 112, 100, 103, 99,
];

/// Annex K.2: the example chrominance quantisation table, in row-major order.
pub const ANNEX_K_CHROMA_QUANT: [u16; 64] = [
    17, 18, 24, 47, 99, 99, 99, 99, //
    18, 21, 26, 66, 99, 99, 99, 99, //
    24, 26, 56, 99, 99, 99, 99, 99, //
    47, 66, 99, 99, 99, 99, 99, 99, //
    99, 99, 99, 99, 99, 99, 99, 99, //
    99, 99, 99, 99, 99, 99, 99, 99, //
    99, 99, 99, 99, 99, 99, 99, 99, //
    99, 99, 99, 99, 99, 99, 99, 99,
];

/// Annex K.3.1: `BITS` for the example luminance DC table.
pub const ANNEX_K_DC_LUMA_BITS: [u8; 16] = [0, 1, 5, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0];
/// Annex K.3.1: `HUFFVAL` for the example luminance DC table.
pub const ANNEX_K_DC_LUMA_VALUES: [u8; 12] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11];

/// Annex K.3.3: `BITS` for the example chrominance DC table.
pub const ANNEX_K_DC_CHROMA_BITS: [u8; 16] = [0, 3, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0];
/// Annex K.3.3: `HUFFVAL` for the example chrominance DC table.
pub const ANNEX_K_DC_CHROMA_VALUES: [u8; 12] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11];

/// Annex K.3.2: `BITS` for the example luminance AC table.
pub const ANNEX_K_AC_LUMA_BITS: [u8; 16] = [0, 2, 1, 3, 3, 2, 4, 3, 5, 5, 4, 4, 0, 0, 1, 0x7D];

/// Annex K.3.2: `HUFFVAL` for the example luminance AC table (162 symbols).
pub const ANNEX_K_AC_LUMA_VALUES: [u8; 162] = [
    0x01, 0x02, 0x03, 0x00, 0x04, 0x11, 0x05, 0x12, 0x21, 0x31, 0x41, 0x06, 0x13, 0x51, 0x61, 0x07,
    0x22, 0x71, 0x14, 0x32, 0x81, 0x91, 0xA1, 0x08, 0x23, 0x42, 0xB1, 0xC1, 0x15, 0x52, 0xD1, 0xF0,
    0x24, 0x33, 0x62, 0x72, 0x82, 0x09, 0x0A, 0x16, 0x17, 0x18, 0x19, 0x1A, 0x25, 0x26, 0x27, 0x28,
    0x29, 0x2A, 0x34, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3A, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0x49,
    0x4A, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5A, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68, 0x69,
    0x6A, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7A, 0x83, 0x84, 0x85, 0x86, 0x87, 0x88, 0x89,
    0x8A, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99, 0x9A, 0xA2, 0xA3, 0xA4, 0xA5, 0xA6, 0xA7,
    0xA8, 0xA9, 0xAA, 0xB2, 0xB3, 0xB4, 0xB5, 0xB6, 0xB7, 0xB8, 0xB9, 0xBA, 0xC2, 0xC3, 0xC4, 0xC5,
    0xC6, 0xC7, 0xC8, 0xC9, 0xCA, 0xD2, 0xD3, 0xD4, 0xD5, 0xD6, 0xD7, 0xD8, 0xD9, 0xDA, 0xE1, 0xE2,
    0xE3, 0xE4, 0xE5, 0xE6, 0xE7, 0xE8, 0xE9, 0xEA, 0xF1, 0xF2, 0xF3, 0xF4, 0xF5, 0xF6, 0xF7, 0xF8,
    0xF9, 0xFA,
];

/// Annex K.3.4: `BITS` for the example chrominance AC table.
pub const ANNEX_K_AC_CHROMA_BITS: [u8; 16] = [0, 2, 1, 2, 4, 4, 3, 4, 7, 5, 4, 4, 0, 1, 2, 0x77];

/// Annex K.3.4: `HUFFVAL` for the example chrominance AC table (162 symbols).
pub const ANNEX_K_AC_CHROMA_VALUES: [u8; 162] = [
    0x00, 0x01, 0x02, 0x03, 0x11, 0x04, 0x05, 0x21, 0x31, 0x06, 0x12, 0x41, 0x51, 0x07, 0x61, 0x71,
    0x13, 0x22, 0x32, 0x81, 0x08, 0x14, 0x42, 0x91, 0xA1, 0xB1, 0xC1, 0x09, 0x23, 0x33, 0x52, 0xF0,
    0x15, 0x62, 0x72, 0xD1, 0x0A, 0x16, 0x24, 0x34, 0xE1, 0x25, 0xF1, 0x17, 0x18, 0x19, 0x1A, 0x26,
    0x27, 0x28, 0x29, 0x2A, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3A, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48,
    0x49, 0x4A, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5A, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68,
    0x69, 0x6A, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7A, 0x82, 0x83, 0x84, 0x85, 0x86, 0x87,
    0x88, 0x89, 0x8A, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99, 0x9A, 0xA2, 0xA3, 0xA4, 0xA5,
    0xA6, 0xA7, 0xA8, 0xA9, 0xAA, 0xB2, 0xB3, 0xB4, 0xB5, 0xB6, 0xB7, 0xB8, 0xB9, 0xBA, 0xC2, 0xC3,
    0xC4, 0xC5, 0xC6, 0xC7, 0xC8, 0xC9, 0xCA, 0xD2, 0xD3, 0xD4, 0xD5, 0xD6, 0xD7, 0xD8, 0xD9, 0xDA,
    0xE2, 0xE3, 0xE4, 0xE5, 0xE6, 0xE7, 0xE8, 0xE9, 0xEA, 0xF2, 0xF3, 0xF4, 0xF5, 0xF6, 0xF7, 0xF8,
    0xF9, 0xFA,
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zigzag_tables_are_mutual_inverses() {
        for (zig, &nat) in ZIGZAG_TO_NATURAL.iter().enumerate() {
            assert_eq!(NATURAL_TO_ZIGZAG[nat], zig);
        }
        let mut seen = [false; 64];
        for &nat in &ZIGZAG_TO_NATURAL {
            assert!(!seen[nat], "natural index {nat} appears twice");
            seen[nat] = true;
        }
        assert!(seen.iter().all(|&s| s));
    }

    /// The first seven zig-zag luma values are the ones quoted in the TIFF
    /// investigation as libtiff's DQT payload prefix at quality 75 scaling.
    #[test]
    fn luma_quant_in_zigzag_order_matches_the_spec_sequence() {
        let zig: Vec<u16> = ZIGZAG_TO_NATURAL
            .iter()
            .map(|&n| ANNEX_K_LUMA_QUANT[n])
            .collect();
        assert_eq!(&zig[..7], &[16, 11, 12, 14, 12, 10, 16]);
    }

    #[test]
    fn chroma_quant_in_zigzag_order_matches_the_spec_sequence() {
        let zig: Vec<u16> = ZIGZAG_TO_NATURAL
            .iter()
            .map(|&n| ANNEX_K_CHROMA_QUANT[n])
            .collect();
        assert_eq!(
            &zig[..15],
            &[17, 18, 18, 24, 21, 24, 47, 26, 26, 47, 99, 66, 56, 66, 99]
        );
    }

    #[test]
    fn annex_k_huffman_counts_match_their_value_lists() {
        let cases: [(&[u8; 16], usize); 4] = [
            (&ANNEX_K_DC_LUMA_BITS, ANNEX_K_DC_LUMA_VALUES.len()),
            (&ANNEX_K_DC_CHROMA_BITS, ANNEX_K_DC_CHROMA_VALUES.len()),
            (&ANNEX_K_AC_LUMA_BITS, ANNEX_K_AC_LUMA_VALUES.len()),
            (&ANNEX_K_AC_CHROMA_BITS, ANNEX_K_AC_CHROMA_VALUES.len()),
        ];
        for (bits, n) in cases {
            let sum: usize = bits.iter().map(|&b| usize::from(b)).sum();
            assert_eq!(sum, n, "BITS sum {sum} != HUFFVAL length {n}");
        }
    }

    #[test]
    fn ac_tables_contain_the_zrl_and_eob_symbols() {
        assert!(ANNEX_K_AC_LUMA_VALUES.contains(&0xF0));
        assert!(ANNEX_K_AC_CHROMA_VALUES.contains(&0xF0));
        assert!(ANNEX_K_AC_LUMA_VALUES.contains(&0x00));
        assert!(ANNEX_K_AC_CHROMA_VALUES.contains(&0x00));
    }
}
