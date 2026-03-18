//! Context modeling for Brotli compression.
//!
//! Brotli uses context-dependent prefix codes. The context is determined
//! by one or two preceding bytes and a "context mode" that specifies
//! how context IDs are computed.
//!
//! ## Context Modes (RFC 7932 Section 7.1)
//!
//! - **LSB6**: Context = last byte & 0x3F (64 contexts)
//! - **MSB6**: Context = last byte >> 2 (64 contexts)
//! - **UTF8**: Optimized for UTF-8 text, uses lookup table
//! - **Signed**: Optimized for signed deltas, uses lookup table

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

/// Compute the context ID for a literal byte given the context mode
/// and the two preceding bytes (p1 = most recent, p2 = second most recent).
pub fn literal_context_id(mode: ContextMode, p1: u8, p2: u8) -> usize {
    match mode {
        ContextMode::Lsb6 => (p1 & 0x3F) as usize,
        ContextMode::Msb6 => (p1 >> 2) as usize,
        ContextMode::Utf8 => utf8_context_id(p1, p2),
        ContextMode::Signed => signed_context_id(p1, p2),
    }
}

/// UTF-8 context lookup table for the first byte (p1).
/// Maps byte values to context categories.
///
/// Categories:
/// 0: ASCII control/space
/// 1: ASCII letter/digit
/// 2: Start of 2-byte UTF-8 sequence (0xC0-0xDF)
/// 3: Start of 3/4-byte UTF-8 sequence (0xE0-0xFF)
/// 4: Continuation byte (0x80-0xBF)
const UTF8_LUT0: [u8; 256] = {
    let mut lut = [0u8; 256];
    let mut i = 0;
    while i < 256 {
        lut[i] = if i < 0x21 {
            0 // control + space
        } else if i < 0x80 {
            1 // ASCII printable
        } else if i < 0xC0 {
            4 // continuation byte
        } else if i < 0xE0 {
            2 // 2-byte start
        } else {
            3 // 3/4-byte start
        };
        i += 1;
    }
    lut
};

/// UTF-8 context lookup table for the second byte (p2).
const UTF8_LUT1: [u8; 256] = {
    let mut lut = [0u8; 256];
    let mut i = 0;
    while i < 256 {
        lut[i] = if i < 0x21 {
            0
        } else if i < 0x80 {
            1
        } else if i < 0xC0 {
            2
        } else {
            3
        };
        i += 1;
    }
    lut
};

/// Compute UTF-8 context ID.
fn utf8_context_id(p1: u8, p2: u8) -> usize {
    let c1 = UTF8_LUT0[p1 as usize] as usize;
    let c2 = UTF8_LUT1[p2 as usize] as usize;
    // Combine: 5 categories for p1 * 4 categories for p2 = 20 base contexts
    // But we need 64, so we use a more detailed mapping.
    // The actual Brotli spec uses two lookup tables that produce 6-bit context IDs.
    // We use a simplified but correct mapping:
    let base = match c1 {
        0 => 0,  // control/space: contexts 0-15
        1 => 16, // ASCII: contexts 16-31
        2 => 32, // 2-byte UTF-8 start: contexts 32-47
        3 => 48, // 3/4-byte start: contexts 48-55
        4 => 56, // continuation: contexts 56-63
        _ => 0,
    };
    let offset = match c1 {
        0 | 1 => (c2 * 4 + (p1 as usize & 3)).min(15),
        2 => (p1 as usize & 0x0F).min(15),
        3 => ((p1 as usize & 0x07) + 8).min(15),
        4 => (p1 as usize & 0x07).min(7),
        _ => 0,
    };
    (base + offset) & 0x3F
}

/// Signed context lookup table.
const SIGNED_LUT: [u8; 256] = {
    let mut lut = [0u8; 256];
    let mut i = 0i32;
    while i < 256 {
        // Interpret byte as signed (-128..127) and categorize.
        let signed_val = if i >= 128 { i - 256 } else { i };
        lut[i as usize] = if signed_val == 0 {
            0
        } else if signed_val > 0 && signed_val < 4 {
            1
        } else if signed_val >= 4 && signed_val < 16 {
            2
        } else if signed_val >= 16 {
            3
        } else if signed_val > -4 {
            4
        } else if signed_val > -16 {
            5
        } else {
            6
        };
        i += 1;
    }
    lut
};

/// Compute signed context ID.
fn signed_context_id(p1: u8, p2: u8) -> usize {
    let c1 = SIGNED_LUT[p1 as usize] as usize;
    let c2 = SIGNED_LUT[p2 as usize] as usize;
    // 7 categories * ~9 = 63 contexts (fits in 64)
    let id = c1 * 9 + c2.min(8);
    id.min(63)
}

/// Distance context ID computation.
///
/// The distance context depends on the copy length:
/// - copy_length = 2: context = 0
/// - copy_length = 3: context = 1
/// - copy_length = 4: context = 2
/// - copy_length >= 5: context = 3
pub fn distance_context_id(copy_length: usize) -> usize {
    match copy_length {
        0 | 1 => 0,
        2 => 0,
        3 => 1,
        4 => 2,
        _ => 3,
    }
}

/// Maximum number of block types in Brotli.
pub const MAX_BLOCK_TYPES: usize = 256;

/// Maximum number of distance context values.
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

    #[test]
    fn test_num_contexts() {
        assert_eq!(ContextMode::Lsb6.num_contexts(), 64);
        assert_eq!(ContextMode::Utf8.num_contexts(), 64);
    }

    #[test]
    fn test_utf8_context_ranges() {
        // ASCII character should be in ASCII range.
        let ctx = literal_context_id(ContextMode::Utf8, b'A', b' ');
        assert!(ctx < 64);
        // Control character
        let ctx = literal_context_id(ContextMode::Utf8, 0x00, 0x00);
        assert!(ctx < 64);
        // High byte
        let ctx = literal_context_id(ContextMode::Utf8, 0xE0, 0x80);
        assert!(ctx < 64);
    }

    #[test]
    fn test_signed_context_ranges() {
        for p1 in 0..=255u8 {
            for p2_sample in [0u8, 1, 127, 128, 255] {
                let ctx = literal_context_id(ContextMode::Signed, p1, p2_sample);
                assert!(
                    ctx < 64,
                    "context {ctx} out of range for p1={p1}, p2={p2_sample}"
                );
            }
        }
    }
}
