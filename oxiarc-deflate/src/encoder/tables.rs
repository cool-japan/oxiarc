//! Constant tables shared by the DEFLATE encoder.
//!
//! Every table is derived at compile time by the same construction zlib's
//! `tr_static_init()` performs at run time, so the values cannot drift from a
//! hand transcription.

/// Number of literal symbols (0..=255).
pub(crate) const LITERALS: usize = 256;
/// Number of length codes (257..=285 → 29 codes).
pub(crate) const LENGTH_CODES: usize = 29;
/// Literal/length alphabet size.
pub(crate) const L_CODES: usize = LITERALS + 1 + LENGTH_CODES;
/// Distance alphabet size.
pub(crate) const D_CODES: usize = 30;
/// Bit-length alphabet size.
pub(crate) const BL_CODES: usize = 19;
/// Size of the shared Huffman heap (`2 * L_CODES + 1`).
pub(crate) const HEAP_SIZE: usize = 2 * L_CODES + 1;
/// Maximum Huffman code length for the literal/length and distance trees.
pub(crate) const MAX_BITS: usize = 15;
/// Maximum Huffman code length for the bit-length tree.
pub(crate) const MAX_BL_BITS: usize = 7;
/// The end-of-block symbol.
pub(crate) const END_BLOCK: usize = 256;
/// Bit-length code: repeat the previous length 3..=6 times.
pub(crate) const REP_3_6: usize = 16;
/// Bit-length code: repeat a zero length 3..=10 times.
pub(crate) const REPZ_3_10: usize = 17;
/// Bit-length code: repeat a zero length 11..=138 times.
pub(crate) const REPZ_11_138: usize = 18;

/// Minimum LZ77 match length.
pub(crate) const MIN_MATCH: usize = 3;
/// Maximum LZ77 match length.
pub(crate) const MAX_MATCH: usize = 258;
/// Maximum size of a stored (BTYPE=00) block payload.
pub(crate) const MAX_STORED: usize = 65535;

/// Extra bits for each length code.
pub(crate) const EXTRA_LBITS: [u8; LENGTH_CODES] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];

/// Extra bits for each distance code.
pub(crate) const EXTRA_DBITS: [u8; D_CODES] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13,
    13,
];

/// Extra bits for each bit-length code.
pub(crate) const EXTRA_BLBITS: [u8; BL_CODES] =
    [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2, 3, 7];

/// Order in which the bit-length code lengths are transmitted (RFC 1951 §3.2.7).
pub(crate) const BL_ORDER: [usize; BL_CODES] = [
    16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
];

const fn build_length_tables() -> ([u8; 256], [u16; LENGTH_CODES]) {
    let mut length_code = [0u8; 256];
    let mut base_length = [0u16; LENGTH_CODES];
    let mut length = 0usize;
    let mut code = 0usize;
    while code < LENGTH_CODES - 1 {
        base_length[code] = length as u16;
        let n_max = 1usize << EXTRA_LBITS[code];
        let mut n = 0;
        while n < n_max {
            length_code[length] = code as u8;
            length += 1;
            n += 1;
        }
        code += 1;
    }
    // Length 258 (index 255) is representable as code 284 + 5 extra bits or as
    // code 285; the single-symbol encoding is always cheaper.
    length_code[255] = code as u8;
    base_length[LENGTH_CODES - 1] = 255;
    (length_code, base_length)
}

const fn build_dist_tables() -> ([u8; 512], [u16; D_CODES]) {
    let mut dist_code = [0u8; 512];
    let mut base_dist = [0u16; D_CODES];
    let mut dist = 0usize;
    let mut code = 0usize;
    while code < 16 {
        base_dist[code] = dist as u16;
        let n_max = 1usize << EXTRA_DBITS[code];
        let mut n = 0;
        while n < n_max {
            dist_code[dist] = code as u8;
            dist += 1;
            n += 1;
        }
        code += 1;
    }
    dist >>= 7;
    while code < D_CODES {
        base_dist[code] = (dist << 7) as u16;
        let n_max = 1usize << (EXTRA_DBITS[code] - 7);
        let mut n = 0;
        while n < n_max {
            dist_code[256 + dist] = code as u8;
            dist += 1;
            n += 1;
        }
        code += 1;
    }
    (dist_code, base_dist)
}

const LENGTH_TABLES: ([u8; 256], [u16; LENGTH_CODES]) = build_length_tables();
const DIST_TABLES: ([u8; 512], [u16; D_CODES]) = build_dist_tables();

/// `LENGTH_CODE[len - MIN_MATCH]` is the length code index (0..=28).
pub(crate) const LENGTH_CODE: [u8; 256] = LENGTH_TABLES.0;
/// First length (minus `MIN_MATCH`) covered by each length code.
pub(crate) const BASE_LENGTH: [u16; LENGTH_CODES] = LENGTH_TABLES.1;
/// Distance-code lookup; see [`d_code`].
pub(crate) const DIST_CODE: [u8; 512] = DIST_TABLES.0;
/// First distance (minus one) covered by each distance code.
pub(crate) const BASE_DIST: [u16; D_CODES] = DIST_TABLES.1;

/// Map `dist` (the match distance **minus one**) to its distance code.
#[inline(always)]
pub(crate) fn d_code(dist: usize) -> usize {
    if dist < 256 {
        DIST_CODE[dist] as usize
    } else {
        DIST_CODE[256 + (dist >> 7)] as usize
    }
}

/// Reverse the low `len` bits of `code` (DEFLATE transmits Huffman codes
/// most-significant bit first, while the bit accumulator is LSB-first).
pub(crate) const fn bi_reverse(code: u32, len: u32) -> u32 {
    let mut res = 0u32;
    let mut value = code;
    let mut i = 0;
    while i < len {
        res = (res << 1) | (value & 1);
        value >>= 1;
        i += 1;
    }
    res
}

const fn build_static_ltree() -> ([u16; L_CODES + 2], [u8; L_CODES + 2]) {
    let mut len = [0u8; L_CODES + 2];
    let mut n = 0;
    while n <= 143 {
        len[n] = 8;
        n += 1;
    }
    while n <= 255 {
        len[n] = 9;
        n += 1;
    }
    while n <= 279 {
        len[n] = 7;
        n += 1;
    }
    while n <= 287 {
        len[n] = 8;
        n += 1;
    }
    let mut bl_count = [0u16; MAX_BITS + 1];
    let mut i = 0;
    while i < L_CODES + 2 {
        bl_count[len[i] as usize] += 1;
        i += 1;
    }
    let mut next_code = [0u16; MAX_BITS + 1];
    let mut code = 0u32;
    let mut bits = 1;
    while bits <= MAX_BITS {
        code = (code + bl_count[bits - 1] as u32) << 1;
        next_code[bits] = code as u16;
        bits += 1;
    }
    let mut codes = [0u16; L_CODES + 2];
    let mut n = 0;
    while n < L_CODES + 2 {
        let l = len[n] as usize;
        if l != 0 {
            codes[n] = bi_reverse(next_code[l] as u32, l as u32) as u16;
            next_code[l] += 1;
        }
        n += 1;
    }
    (codes, len)
}

const fn build_static_dtree() -> ([u16; D_CODES], [u8; D_CODES]) {
    let mut codes = [0u16; D_CODES];
    let len = [5u8; D_CODES];
    let mut n = 0;
    while n < D_CODES {
        codes[n] = bi_reverse(n as u32, 5) as u16;
        n += 1;
    }
    (codes, len)
}

const STATIC_L: ([u16; L_CODES + 2], [u8; L_CODES + 2]) = build_static_ltree();
const STATIC_D: ([u16; D_CODES], [u8; D_CODES]) = build_static_dtree();

/// Bit-reversed codes of the fixed literal/length tree (RFC 1951 §3.2.6).
pub(crate) const STATIC_LTREE_CODE: [u16; L_CODES + 2] = STATIC_L.0;
/// Code lengths of the fixed literal/length tree.
pub(crate) const STATIC_LTREE_LEN: [u8; L_CODES + 2] = STATIC_L.1;
/// Bit-reversed codes of the fixed distance tree.
pub(crate) const STATIC_DTREE_CODE: [u16; D_CODES] = STATIC_D.0;
/// Code lengths of the fixed distance tree (all 5).
pub(crate) const STATIC_DTREE_LEN: [u8; D_CODES] = STATIC_D.1;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn length_code_matches_rfc_1951_table() {
        // Length 3 -> code 257, length 258 -> code 285.
        assert_eq!(LENGTH_CODE[0], 0);
        assert_eq!(LENGTH_CODE[255], 28);
        // Length 11..=12 share code 265 (index 8) with one extra bit.
        assert_eq!(LENGTH_CODE[11 - MIN_MATCH], 8);
        assert_eq!(LENGTH_CODE[12 - MIN_MATCH], 8);
        assert_eq!(LENGTH_CODE[13 - MIN_MATCH], 9);
        assert_eq!(BASE_LENGTH[8], (11 - MIN_MATCH) as u16);
    }

    #[test]
    fn dist_code_matches_rfc_1951_table() {
        assert_eq!(d_code(0), 0); // distance 1
        assert_eq!(d_code(3), 3); // distance 4
        assert_eq!(d_code(4), 4); // distance 5..=6
        assert_eq!(d_code(5), 4);
        assert_eq!(d_code(32767), 29); // distance 32768
        assert_eq!(BASE_DIST[4], 4);
        assert_eq!(BASE_DIST[29], 24576);
    }

    #[test]
    fn static_trees_are_canonical() {
        assert_eq!(STATIC_LTREE_LEN[0], 8);
        assert_eq!(STATIC_LTREE_LEN[143], 8);
        assert_eq!(STATIC_LTREE_LEN[144], 9);
        assert_eq!(STATIC_LTREE_LEN[255], 9);
        assert_eq!(STATIC_LTREE_LEN[256], 7);
        assert_eq!(STATIC_LTREE_LEN[279], 7);
        assert_eq!(STATIC_LTREE_LEN[280], 8);
        // Literal 0 has code 00110000 (0x30) MSB-first; stored reversed.
        assert_eq!(STATIC_LTREE_CODE[0], bi_reverse(0b0011_0000, 8) as u16);
        // Symbol 256 has code 0000000.
        assert_eq!(STATIC_LTREE_CODE[256], 0);
        assert_eq!(STATIC_DTREE_CODE[1], bi_reverse(1, 5) as u16);
    }
}
