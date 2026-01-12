//! LZH compression method definitions.
//!
//! LZH archives support multiple compression methods (lh0-lh7), each with
//! different window sizes and compression characteristics.

/// LZH compression method.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LzhMethod {
    /// lh0: Stored (no compression).
    Lh0,
    /// lh4: 4KB window, static Huffman.
    Lh4,
    /// lh5: 8KB window, static Huffman (most common).
    #[default]
    Lh5,
    /// lh6: 32KB window, static Huffman.
    Lh6,
    /// lh7: 64KB window, static Huffman.
    Lh7,
}

impl LzhMethod {
    /// Parse method from the 5-byte method ID string.
    pub fn from_id(id: &[u8]) -> Option<Self> {
        match id {
            b"-lh0-" => Some(Self::Lh0),
            b"-lh4-" => Some(Self::Lh4),
            b"-lh5-" => Some(Self::Lh5),
            b"-lh6-" => Some(Self::Lh6),
            b"-lh7-" => Some(Self::Lh7),
            _ => None,
        }
    }

    /// Get the method ID string.
    pub fn id(&self) -> &'static [u8; 5] {
        match self {
            Self::Lh0 => b"-lh0-",
            Self::Lh4 => b"-lh4-",
            Self::Lh5 => b"-lh5-",
            Self::Lh6 => b"-lh6-",
            Self::Lh7 => b"-lh7-",
        }
    }

    /// Get the sliding window size in bytes.
    pub fn window_size(&self) -> usize {
        match self {
            Self::Lh0 => 0,
            Self::Lh4 => 4096,  // 4 KB
            Self::Lh5 => 8192,  // 8 KB
            Self::Lh6 => 32768, // 32 KB
            Self::Lh7 => 65536, // 64 KB
        }
    }

    /// Get the number of bits for position encoding.
    pub fn position_bits(&self) -> u8 {
        match self {
            Self::Lh0 => 0,
            Self::Lh4 => 12, // log2(4096)
            Self::Lh5 => 13, // log2(8192)
            Self::Lh6 => 15, // log2(32768)
            Self::Lh7 => 16, // log2(65536)
        }
    }

    /// Get the maximum match length.
    pub fn max_match(&self) -> usize {
        match self {
            Self::Lh0 => 0,
            _ => 256,
        }
    }

    /// Get the minimum match length.
    pub fn min_match(&self) -> usize {
        match self {
            Self::Lh0 => 0,
            _ => 3,
        }
    }

    /// Check if this method is stored (no compression).
    pub fn is_stored(&self) -> bool {
        matches!(self, Self::Lh0)
    }

    /// Get the method name.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Lh0 => "lh0",
            Self::Lh4 => "lh4",
            Self::Lh5 => "lh5",
            Self::Lh6 => "lh6",
            Self::Lh7 => "lh7",
        }
    }
}

impl std::fmt::Display for LzhMethod {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.name())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_method_from_id() {
        assert_eq!(LzhMethod::from_id(b"-lh0-"), Some(LzhMethod::Lh0));
        assert_eq!(LzhMethod::from_id(b"-lh5-"), Some(LzhMethod::Lh5));
        assert_eq!(LzhMethod::from_id(b"-lh7-"), Some(LzhMethod::Lh7));
        assert_eq!(LzhMethod::from_id(b"-lz5-"), None);
    }

    #[test]
    fn test_window_sizes() {
        assert_eq!(LzhMethod::Lh4.window_size(), 4096);
        assert_eq!(LzhMethod::Lh5.window_size(), 8192);
        assert_eq!(LzhMethod::Lh6.window_size(), 32768);
        assert_eq!(LzhMethod::Lh7.window_size(), 65536);
    }

    #[test]
    fn test_position_bits() {
        assert_eq!(LzhMethod::Lh4.position_bits(), 12);
        assert_eq!(LzhMethod::Lh5.position_bits(), 13);
        assert_eq!(LzhMethod::Lh6.position_bits(), 15);
        assert_eq!(LzhMethod::Lh7.position_bits(), 16);
    }
}
