//! LZH compression method definitions.
//!
//! LZH archives support multiple compression methods (lh0-lh7 plus the
//! directory marker `-lhd-`), each with different window sizes and
//! compression characteristics.

/// LZH compression method.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LzhMethod {
    /// lh0: Stored (no compression).
    Lh0,
    /// lh1: 4KB window LZSS + adaptive Huffman (LHarc 1.x legacy format).
    Lh1,
    /// lh4: 4KB window, static Huffman.
    Lh4,
    /// lh5: 8KB window, static Huffman (most common).
    #[default]
    Lh5,
    /// lh6: 32KB window, static Huffman.
    Lh6,
    /// lh7: 64KB window, static Huffman.
    Lh7,
    /// lhd: Directory entry marker (no data).
    Lhd,
    /// Unrecognised method ID (e.g. `-lh2-`, `-lzs-`, `-pm2-`).
    ///
    /// Carrying the raw 5-byte ID lets archive readers list such entries
    /// (and skip them at extraction time) instead of aborting the archive.
    Unknown([u8; 5]),
}

impl LzhMethod {
    /// Parse method from the 5-byte method ID string.
    ///
    /// Returns `None` for unrecognised IDs; use
    /// [`from_id_lossy`](Self::from_id_lossy) to map those to
    /// [`LzhMethod::Unknown`] instead.
    pub fn from_id(id: &[u8]) -> Option<Self> {
        match id {
            b"-lh0-" => Some(Self::Lh0),
            b"-lh1-" => Some(Self::Lh1),
            b"-lh4-" => Some(Self::Lh4),
            b"-lh5-" => Some(Self::Lh5),
            b"-lh6-" => Some(Self::Lh6),
            b"-lh7-" => Some(Self::Lh7),
            b"-lhd-" => Some(Self::Lhd),
            _ => None,
        }
    }

    /// Parse a method ID, mapping unrecognised IDs to [`LzhMethod::Unknown`]
    /// so that archive scanning can continue past unsupported entries.
    pub fn from_id_lossy(id: [u8; 5]) -> Self {
        Self::from_id(&id).unwrap_or(Self::Unknown(id))
    }

    /// Get the 5-byte method ID string.
    pub fn id(&self) -> [u8; 5] {
        match self {
            Self::Lh0 => *b"-lh0-",
            Self::Lh1 => *b"-lh1-",
            Self::Lh4 => *b"-lh4-",
            Self::Lh5 => *b"-lh5-",
            Self::Lh6 => *b"-lh6-",
            Self::Lh7 => *b"-lh7-",
            Self::Lhd => *b"-lhd-",
            Self::Unknown(id) => *id,
        }
    }

    /// Get the sliding window size in bytes.
    pub fn window_size(&self) -> usize {
        match self {
            Self::Lh0 | Self::Lhd | Self::Unknown(_) => 0,
            Self::Lh1 => 4096,  // 4 KB
            Self::Lh4 => 4096,  // 4 KB
            Self::Lh5 => 8192,  // 8 KB
            Self::Lh6 => 32768, // 32 KB
            Self::Lh7 => 65536, // 64 KB
        }
    }

    /// Get the number of bits for position encoding.
    pub fn position_bits(&self) -> u8 {
        match self {
            Self::Lh0 | Self::Lhd | Self::Unknown(_) => 0,
            Self::Lh1 => 12, // log2(4096)
            Self::Lh4 => 12, // log2(4096)
            Self::Lh5 => 13, // log2(8192)
            Self::Lh6 => 15, // log2(32768)
            Self::Lh7 => 16, // log2(65536)
        }
    }

    /// Get the maximum match length.
    pub fn max_match(&self) -> usize {
        match self {
            Self::Lh0 | Self::Lhd | Self::Unknown(_) => 0,
            Self::Lh1 => 60,
            _ => 256,
        }
    }

    /// Get the minimum match length.
    pub fn min_match(&self) -> usize {
        match self {
            Self::Lh0 | Self::Lhd | Self::Unknown(_) => 0,
            _ => 3,
        }
    }

    /// Check if this method is stored (no compression).
    ///
    /// Directory markers (`-lhd-`) are treated as stored: they carry zero
    /// bytes of data, which passes through unchanged.
    pub fn is_stored(&self) -> bool {
        matches!(self, Self::Lh0 | Self::Lhd)
    }

    /// Check if this method marks a directory entry (`-lhd-`).
    pub fn is_directory(&self) -> bool {
        matches!(self, Self::Lhd)
    }

    /// Check if this crate can decode data compressed with this method.
    pub fn supports_decode(&self) -> bool {
        !matches!(self, Self::Unknown(_))
    }

    /// Get the method name.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Lh0 => "lh0",
            Self::Lh1 => "lh1",
            Self::Lh4 => "lh4",
            Self::Lh5 => "lh5",
            Self::Lh6 => "lh6",
            Self::Lh7 => "lh7",
            Self::Lhd => "lhd",
            Self::Unknown(_) => "unknown",
        }
    }
}

impl std::fmt::Display for LzhMethod {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unknown(id) => write!(f, "{}", String::from_utf8_lossy(id)),
            _ => write!(f, "{}", self.name()),
        }
    }
}

/// LZH constants for encoding/decoding.
pub mod constants {
    /// Number of character codes (0-255 literals + 256+ lengths).
    pub const NC: usize = 510;
    /// Number of position (distance) codes.
    pub const NP_MAX: usize = 17; // For lh7 (16-bit positions + 1)
    /// Number of code length codes.
    pub const NT: usize = 19;
    /// Special code for tree encoding.
    pub const TBIT: u8 = 5;
    /// Character/length code bits.
    pub const CBIT: u8 = 9;
    /// Position code bits (varies by method).
    pub const PBIT_MAX: u8 = 5;
}

/// Number of bits used to encode the P-tree code count for a given `np`.
///
/// lh4/lh5 use `np = 14` (4 bits); lh6/lh7 use `np = 16`/`17`, which does
/// not fit in 4 bits, so 5 bits are used.
pub(crate) fn p_tree_count_bits(np: usize) -> u8 {
    if np <= 14 { 4 } else { 5 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_method_from_id() {
        assert_eq!(LzhMethod::from_id(b"-lh0-"), Some(LzhMethod::Lh0));
        assert_eq!(LzhMethod::from_id(b"-lh1-"), Some(LzhMethod::Lh1));
        assert_eq!(LzhMethod::from_id(b"-lh5-"), Some(LzhMethod::Lh5));
        assert_eq!(LzhMethod::from_id(b"-lh7-"), Some(LzhMethod::Lh7));
        assert_eq!(LzhMethod::from_id(b"-lhd-"), Some(LzhMethod::Lhd));
        assert_eq!(LzhMethod::from_id(b"-lz5-"), None);
    }

    #[test]
    fn test_method_from_id_lossy() {
        assert_eq!(LzhMethod::from_id_lossy(*b"-lh5-"), LzhMethod::Lh5);
        assert_eq!(
            LzhMethod::from_id_lossy(*b"-pm2-"),
            LzhMethod::Unknown(*b"-pm2-")
        );
    }

    #[test]
    fn test_window_sizes() {
        assert_eq!(LzhMethod::Lh1.window_size(), 4096);
        assert_eq!(LzhMethod::Lh4.window_size(), 4096);
        assert_eq!(LzhMethod::Lh5.window_size(), 8192);
        assert_eq!(LzhMethod::Lh6.window_size(), 32768);
        assert_eq!(LzhMethod::Lh7.window_size(), 65536);
        assert_eq!(LzhMethod::Lhd.window_size(), 0);
    }

    #[test]
    fn test_position_bits() {
        assert_eq!(LzhMethod::Lh4.position_bits(), 12);
        assert_eq!(LzhMethod::Lh5.position_bits(), 13);
        assert_eq!(LzhMethod::Lh6.position_bits(), 15);
        assert_eq!(LzhMethod::Lh7.position_bits(), 16);
    }

    #[test]
    fn test_directory_marker() {
        assert!(LzhMethod::Lhd.is_directory());
        assert!(LzhMethod::Lhd.is_stored());
        assert!(!LzhMethod::Lh5.is_directory());
    }

    #[test]
    fn test_id_roundtrip() {
        for m in [
            LzhMethod::Lh0,
            LzhMethod::Lh1,
            LzhMethod::Lh4,
            LzhMethod::Lh5,
            LzhMethod::Lh6,
            LzhMethod::Lh7,
            LzhMethod::Lhd,
        ] {
            assert_eq!(LzhMethod::from_id(&m.id()), Some(m));
        }
    }
}
