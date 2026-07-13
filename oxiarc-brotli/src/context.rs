//! Context modeling for Brotli literal and distance coding.
//!
//! Brotli selects the prefix code for the next literal or distance from a
//! context. For literals the context is derived from the previous two
//! uncompressed bytes (`p1` = most recent, `p2` = second most recent) using
//! one of four context modes; for distances it is derived from the copy
//! length of the same command.
//!
//! ## Context Modes (RFC 7932 Section 7.1)
//!
//! - **LSB6**: Context ID = `p1 & 0x3F`
//! - **MSB6**: Context ID = `p1 >> 2`
//! - **UTF8**: Context ID = `LUT0[p1] | LUT1[p2]`
//! - **Signed**: Context ID = `(LUT2[p1] << 3) | LUT2[p2]`
//!
//! The three lookup tables below are transcribed verbatim from RFC 7932
//! Section 7.1 (CRC-32 values: Lut0 `0x8e91efb7`, Lut1 `0xd01a32f4`, Lut2
//! `0x0dd7a0d6`).

/// Context mode for literal context modeling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ContextMode {
    /// LSB6: context = p1 & 0x3F
    Lsb6 = 0,
    /// MSB6: context = p1 >> 2
    Msb6 = 1,
    /// UTF8: context from lookup tables optimized for UTF-8
    Utf8 = 2,
    /// Signed: context from lookup tables optimized for signed values
    Signed = 3,
}

impl ContextMode {
    /// Create from a 2-bit value.
    pub fn from_bits(bits: u8) -> Option<Self> {
        match bits & 0x03 {
            0 => Some(ContextMode::Lsb6),
            1 => Some(ContextMode::Msb6),
            2 => Some(ContextMode::Utf8),
            3 => Some(ContextMode::Signed),
            _ => None,
        }
    }

    /// Number of context IDs for this mode.
    pub fn num_contexts(self) -> usize {
        64
    }
}

/// RFC 7932 Section 7.1 `Lut0` (UTF8 mode, indexed by `p1`).
#[rustfmt::skip]
const UTF8_LUT0: [u8; 256] = [
     0,  0,  0,  0,  0,  0,  0,  0,  0,  4,  4,  0,  0,  4,  0,  0,
     0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,
     8, 12, 16, 12, 12, 20, 12, 16, 24, 28, 12, 12, 32, 12, 36, 12,
    44, 44, 44, 44, 44, 44, 44, 44, 44, 44, 32, 32, 24, 40, 28, 12,
    12, 48, 52, 52, 52, 48, 52, 52, 52, 48, 52, 52, 52, 52, 52, 48,
    52, 52, 52, 52, 52, 48, 52, 52, 52, 52, 52, 24, 12, 28, 12, 12,
    12, 56, 60, 60, 60, 56, 60, 60, 60, 56, 60, 60, 60, 60, 60, 56,
    60, 60, 60, 60, 60, 56, 60, 60, 60, 60, 60, 24, 12, 28, 12,  0,
     0, 1, 0, 1, 0, 1, 0, 1, 0, 1, 0, 1, 0, 1, 0, 1,
     0, 1, 0, 1, 0, 1, 0, 1, 0, 1, 0, 1, 0, 1, 0, 1,
     0, 1, 0, 1, 0, 1, 0, 1, 0, 1, 0, 1, 0, 1, 0, 1,
     0, 1, 0, 1, 0, 1, 0, 1, 0, 1, 0, 1, 0, 1, 0, 1,
     2, 3, 2, 3, 2, 3, 2, 3, 2, 3, 2, 3, 2, 3, 2, 3,
     2, 3, 2, 3, 2, 3, 2, 3, 2, 3, 2, 3, 2, 3, 2, 3,
     2, 3, 2, 3, 2, 3, 2, 3, 2, 3, 2, 3, 2, 3, 2, 3,
     2, 3, 2, 3, 2, 3, 2, 3, 2, 3, 2, 3, 2, 3, 2, 3,
];

/// RFC 7932 Section 7.1 `Lut1` (UTF8 mode, indexed by `p2`).
#[rustfmt::skip]
const UTF8_LUT1: [u8; 256] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 1, 1, 1, 1, 1, 1,
    1, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2,
    2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 1, 1, 1, 1, 1,
    1, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3,
    3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 1, 1, 1, 1, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2,
    2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2,
];

/// RFC 7932 Section 7.1 `Lut2` (Signed mode, indexed by `p1` and `p2`).
#[rustfmt::skip]
const SIGNED_LUT2: [u8; 256] = [
    0, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2,
    2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2,
    2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2,
    3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3,
    3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3,
    3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3,
    3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3,
    4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4,
    4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4,
    4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4,
    4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4,
    5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5,
    5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5,
    5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5, 5,
    6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 7,
];

/// Compute the context ID for a literal byte given the context mode
/// and the two preceding bytes (p1 = most recent, p2 = second most recent).
///
/// This follows RFC 7932 Section 7.1 exactly; the result is in `0..64`.
pub fn literal_context_id(mode: ContextMode, p1: u8, p2: u8) -> usize {
    match mode {
        ContextMode::Lsb6 => (p1 & 0x3F) as usize,
        ContextMode::Msb6 => (p1 >> 2) as usize,
        ContextMode::Utf8 => (UTF8_LUT0[p1 as usize] | UTF8_LUT1[p2 as usize]) as usize,
        ContextMode::Signed => {
            ((SIGNED_LUT2[p1 as usize] << 3) | SIGNED_LUT2[p2 as usize]) as usize
        }
    }
}

/// Distance context ID computation (RFC 7932 Section 7.2).
///
/// The context IDs are 0, 1, 2, and 3 for copy lengths 2, 3, 4, and more
/// than 4, respectively.
pub fn distance_context_id(copy_length: usize) -> usize {
    match copy_length {
        0..=2 => 0,
        3 => 1,
        4 => 2,
        _ => 3,
    }
}

/// Maximum number of block types in Brotli.
pub const MAX_BLOCK_TYPES: usize = 256;

/// Number of literal context IDs per block type.
pub const NUM_LITERAL_CONTEXTS: usize = 64;

/// Number of distance context IDs per block type.
pub const NUM_DISTANCE_CONTEXTS: usize = 4;

/// A context map that maps (block_type, context_id) to a prefix tree index.
#[derive(Debug, Clone)]
pub struct ContextMap {
    /// The mapping values. Index = block_type * num_contexts + context_id.
    pub map: Vec<u8>,
    /// Number of contexts per block type.
    pub num_contexts: usize,
    /// Number of prefix trees.
    pub num_trees: usize,
}

impl ContextMap {
    /// Create a trivial context map (all contexts map to tree 0).
    pub fn trivial(num_block_types: usize, num_contexts: usize) -> Self {
        ContextMap {
            map: vec![0u8; num_block_types * num_contexts],
            num_contexts,
            num_trees: 1,
        }
    }

    /// Look up the tree index for a given block type and context ID.
    pub fn tree_index(&self, block_type: usize, context_id: usize) -> usize {
        let idx = block_type * self.num_contexts + context_id;
        if idx < self.map.len() {
            self.map[idx] as usize
        } else {
            0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// CRC-32 (as defined in RFC 7932 Appendix C) for table verification.
    fn crc32(data: &[u8]) -> u32 {
        let mut crc = 0xFFFF_FFFFu32;
        for &byte in data {
            let mut c = (crc ^ byte as u32) & 0xFF;
            for _ in 0..8 {
                c = if c & 1 != 0 {
                    0xEDB8_8320 ^ (c >> 1)
                } else {
                    c >> 1
                };
            }
            crc = c ^ (crc >> 8);
        }
        crc ^ 0xFFFF_FFFF
    }

    /// The three context tables must match the CRC-32 check values printed
    /// in RFC 7932 Section 7.1.
    #[test]
    fn test_luts_match_rfc_crc32() {
        assert_eq!(crc32(&UTF8_LUT0), 0x8e91efb7, "Lut0 CRC mismatch");
        assert_eq!(crc32(&UTF8_LUT1), 0xd01a32f4, "Lut1 CRC mismatch");
        assert_eq!(crc32(&SIGNED_LUT2), 0x0dd7a0d6, "Lut2 CRC mismatch");
    }

    #[test]
    fn test_context_mode_from_bits() {
        assert_eq!(ContextMode::from_bits(0), Some(ContextMode::Lsb6));
        assert_eq!(ContextMode::from_bits(1), Some(ContextMode::Msb6));
        assert_eq!(ContextMode::from_bits(2), Some(ContextMode::Utf8));
        assert_eq!(ContextMode::from_bits(3), Some(ContextMode::Signed));
    }

    #[test]
    fn test_lsb6_context() {
        assert_eq!(literal_context_id(ContextMode::Lsb6, 0x41, 0), 0x01);
        assert_eq!(literal_context_id(ContextMode::Lsb6, 0xFF, 0), 0x3F);
        assert_eq!(literal_context_id(ContextMode::Lsb6, 0x00, 0), 0x00);
    }

    #[test]
    fn test_msb6_context() {
        assert_eq!(literal_context_id(ContextMode::Msb6, 0xFF, 0), 0x3F);
        assert_eq!(literal_context_id(ContextMode::Msb6, 0x00, 0), 0x00);
        assert_eq!(literal_context_id(ContextMode::Msb6, 0x80, 0), 0x20);
    }

    #[test]
    fn test_all_modes_stay_in_range() {
        for mode in [
            ContextMode::Lsb6,
            ContextMode::Msb6,
            ContextMode::Utf8,
            ContextMode::Signed,
        ] {
            for p1 in 0..=255u8 {
                for p2 in [0u8, 1, 0x20, b'a', 127, 128, 0xC3, 255] {
                    let ctx = literal_context_id(mode, p1, p2);
                    assert!(ctx < 64, "mode {mode:?} p1={p1} p2={p2} ctx={ctx}");
                }
            }
        }
    }

    #[test]
    fn test_distance_context() {
        assert_eq!(distance_context_id(2), 0);
        assert_eq!(distance_context_id(3), 1);
        assert_eq!(distance_context_id(4), 2);
        assert_eq!(distance_context_id(5), 3);
        assert_eq!(distance_context_id(100), 3);
    }

    #[test]
    fn test_context_map_trivial() {
        let cm = ContextMap::trivial(2, 64);
        assert_eq!(cm.tree_index(0, 0), 0);
        assert_eq!(cm.tree_index(1, 63), 0);
    }
}
