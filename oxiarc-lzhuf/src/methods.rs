//! LZH compression method definitions.
//!
//! LZH containers carry more than the LHarc/LHA `-lh0-`..`-lh7-` line: LArc's
//! `-lzs-`/`-lz4-`/`-lz5-`, LHarc 2.x's `-lh2-`/`-lh3-`, PMarc's
//! `-pm0-`/`-pm2-` and the directory marker `-lhd-` all appear in real
//! archives. Each has its own window size, match limits and entropy coding;
//! the accessors here are the single source of truth for those parameters and
//! deliberately name every variant explicitly rather than falling through a
//! catch-all, so that adding a method can never silently inherit a zero window.

/// LZH compression method.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum LzhMethod {
    /// lh0: Stored (no compression).
    Lh0,
    /// lh1: 4KB window LZSS + adaptive Huffman (LHarc 1.x legacy format).
    Lh1,
    /// lh2: 8KB window LZSS + adaptive Huffman over a growing position tree
    /// (LHarc 2.x).
    Lh2,
    /// lh3: 8KB window LZSS + block-static Huffman with LHarc 2.x's table
    /// format.
    Lh3,
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
    /// lzs: LArc LZSS — 2KB history, 11-bit absolute index, 4-bit length.
    Lzs,
    /// lz4: LArc stored (no compression).
    Lz4,
    /// lz5: LArc LZSS — 4KB pre-seeded history, 12-bit absolute index.
    Lz5,
    /// pm0: PMarc stored (no compression).
    Pm0,
    /// Unrecognised method ID (e.g. `-pm2-`, `-lhx-`).
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
            b"-lh2-" => Some(Self::Lh2),
            b"-lh3-" => Some(Self::Lh3),
            b"-lh4-" => Some(Self::Lh4),
            b"-lh5-" => Some(Self::Lh5),
            b"-lh6-" => Some(Self::Lh6),
            b"-lh7-" => Some(Self::Lh7),
            b"-lhd-" => Some(Self::Lhd),
            b"-lzs-" => Some(Self::Lzs),
            b"-lz4-" => Some(Self::Lz4),
            b"-lz5-" => Some(Self::Lz5),
            b"-pm0-" => Some(Self::Pm0),
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
            Self::Lh2 => *b"-lh2-",
            Self::Lh3 => *b"-lh3-",
            Self::Lh4 => *b"-lh4-",
            Self::Lh5 => *b"-lh5-",
            Self::Lh6 => *b"-lh6-",
            Self::Lh7 => *b"-lh7-",
            Self::Lhd => *b"-lhd-",
            Self::Lzs => *b"-lzs-",
            Self::Lz4 => *b"-lz4-",
            Self::Lz5 => *b"-lz5-",
            Self::Pm0 => *b"-pm0-",
            Self::Unknown(id) => *id,
        }
    }

    /// Get the sliding window size in bytes.
    pub fn window_size(&self) -> usize {
        match self {
            Self::Lh0 | Self::Lhd | Self::Lz4 | Self::Pm0 | Self::Unknown(_) => 0,
            Self::Lzs => 2048,  // 2 KB
            Self::Lh1 => 4096,  // 4 KB
            Self::Lh4 => 4096,  // 4 KB
            Self::Lz5 => 4096,  // 4 KB
            Self::Lh2 => 8192,  // 8 KB
            Self::Lh3 => 8192,  // 8 KB
            Self::Lh5 => 8192,  // 8 KB
            Self::Lh6 => 32768, // 32 KB
            Self::Lh7 => 65536, // 64 KB
        }
    }

    /// Get the number of bits for position encoding.
    pub fn position_bits(&self) -> u8 {
        match self {
            Self::Lh0 | Self::Lhd | Self::Lz4 | Self::Pm0 | Self::Unknown(_) => 0,
            Self::Lzs => 11, // log2(2048)
            Self::Lh1 => 12, // log2(4096)
            Self::Lh4 => 12, // log2(4096)
            Self::Lz5 => 12, // log2(4096)
            Self::Lh2 => 13, // log2(8192)
            Self::Lh3 => 13, // log2(8192)
            Self::Lh5 => 13, // log2(8192)
            Self::Lh6 => 15, // log2(32768)
            Self::Lh7 => 16, // log2(65536)
        }
    }

    /// Width, in bits, of the offset-tree code-count field (canonical LHA
    /// `pbit` / lhasa `OFFSET_BITS`).
    ///
    /// This field prefixes the position/offset Huffman table and also sizes
    /// the single-code value in the `n == 0` degenerate case. Verified against
    /// the lhasa `lh{5,6,7}_decoder.c` wrappers: `-lh4-`/`-lh5-` use 4 bits,
    /// `-lh6-`/`-lh7-` use 5 bits.
    pub(crate) fn offset_bits(&self) -> u8 {
        match self {
            Self::Lh4 | Self::Lh5 => 4,
            Self::Lh6 | Self::Lh7 => 5,
            _ => 0,
        }
    }

    /// Number of history-buffer address bits used by the canonical decoder
    /// (lhasa `HISTORY_BITS`); the ring buffer holds `1 << history_bits` bytes.
    ///
    /// Verified against lhasa: `-lh4-`/`-lh5-` = 14 (16 KiB), `-lh6-` = 16
    /// (64 KiB), `-lh7-` = 17 (128 KiB). The decoder allocates the full ring so
    /// that any conformant archive — whichever dictionary size its encoder
    /// actually used — is decodable; a larger-than-needed ring is harmless.
    pub(crate) fn history_bits(&self) -> u8 {
        match self {
            Self::Lh4 | Self::Lh5 => 14,
            Self::Lh6 => 16,
            Self::Lh7 => 17,
            _ => 0,
        }
    }

    /// Maximum number of offset codes the decoder will read for this method,
    /// i.e. lhasa `MAX_OFFSET_CODES = (1 << OFFSET_BITS) - 1`.
    pub(crate) fn max_offset_codes(&self) -> usize {
        (1usize << self.offset_bits()).saturating_sub(1)
    }

    /// Get the maximum match length.
    pub fn max_match(&self) -> usize {
        match self {
            Self::Lh0 | Self::Lhd | Self::Lz4 | Self::Pm0 | Self::Unknown(_) => 0,
            Self::Lh1 => 60,
            Self::Lzs => 17,
            Self::Lz5 => 18,
            Self::Lh2 | Self::Lh3 | Self::Lh4 | Self::Lh5 | Self::Lh6 | Self::Lh7 => 256,
        }
    }

    /// Get the minimum match length.
    pub fn min_match(&self) -> usize {
        match self {
            Self::Lh0 | Self::Lhd | Self::Lz4 | Self::Pm0 | Self::Unknown(_) => 0,
            Self::Lzs => 2,
            Self::Lh1
            | Self::Lh2
            | Self::Lh3
            | Self::Lh4
            | Self::Lh5
            | Self::Lh6
            | Self::Lh7
            | Self::Lz5 => 3,
        }
    }

    /// Check if this method is stored (no compression).
    ///
    /// Directory markers (`-lhd-`) are treated as stored: they carry zero
    /// bytes of data, which passes through unchanged. LArc's `-lz4-` and
    /// PMarc's `-pm0-` are genuine stored formats — the reference decoders map
    /// both to a null (passthrough) decoder.
    pub fn is_stored(&self) -> bool {
        matches!(self, Self::Lh0 | Self::Lhd | Self::Lz4 | Self::Pm0)
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
            Self::Lh2 => "lh2",
            Self::Lh3 => "lh3",
            Self::Lh4 => "lh4",
            Self::Lh5 => "lh5",
            Self::Lh6 => "lh6",
            Self::Lh7 => "lh7",
            Self::Lhd => "lhd",
            Self::Lzs => "lzs",
            Self::Lz4 => "lz4",
            Self::Lz5 => "lz5",
            Self::Pm0 => "pm0",
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
/// This is used only by the legacy `src/streaming/*` decoder (out of scope
/// for the canonical-format rewrite; retained so that module keeps building).
/// lh4/lh5 use `np = 14` (4 bits); lh6/lh7 use `np = 16`/`17`, which does not
/// fit in 4 bits, so 5 bits are used.
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
        // `-lz5-` used to be unrecognised; it is now an implemented LArc
        // method, so `from_id` must resolve it rather than return `None`.
        assert_eq!(LzhMethod::from_id(b"-lz5-"), Some(LzhMethod::Lz5));
        assert_eq!(LzhMethod::from_id(b"-lzs-"), Some(LzhMethod::Lzs));
        assert_eq!(LzhMethod::from_id(b"-lz4-"), Some(LzhMethod::Lz4));
        assert_eq!(LzhMethod::from_id(b"-lh2-"), Some(LzhMethod::Lh2));
        assert_eq!(LzhMethod::from_id(b"-lh3-"), Some(LzhMethod::Lh3));
        assert_eq!(LzhMethod::from_id(b"-pm0-"), Some(LzhMethod::Pm0));
        // Still unimplemented, so still unrecognised.
        assert_eq!(LzhMethod::from_id(b"-pm2-"), None);
        assert_eq!(LzhMethod::from_id(b"-lhx-"), None);
    }

    #[test]
    fn test_method_from_id_lossy() {
        assert_eq!(LzhMethod::from_id_lossy(*b"-lh5-"), LzhMethod::Lh5);
        assert_eq!(LzhMethod::from_id_lossy(*b"-lz5-"), LzhMethod::Lz5);
        assert_eq!(
            LzhMethod::from_id_lossy(*b"-pm2-"),
            LzhMethod::Unknown(*b"-pm2-")
        );
    }

    #[test]
    fn test_window_sizes() {
        assert_eq!(LzhMethod::Lzs.window_size(), 2048);
        assert_eq!(LzhMethod::Lh1.window_size(), 4096);
        assert_eq!(LzhMethod::Lh4.window_size(), 4096);
        assert_eq!(LzhMethod::Lz5.window_size(), 4096);
        assert_eq!(LzhMethod::Lh2.window_size(), 8192);
        assert_eq!(LzhMethod::Lh3.window_size(), 8192);
        assert_eq!(LzhMethod::Lh5.window_size(), 8192);
        assert_eq!(LzhMethod::Lh6.window_size(), 32768);
        assert_eq!(LzhMethod::Lh7.window_size(), 65536);
        assert_eq!(LzhMethod::Lhd.window_size(), 0);
        assert_eq!(LzhMethod::Lz4.window_size(), 0);
        assert_eq!(LzhMethod::Pm0.window_size(), 0);
    }

    #[test]
    fn test_legacy_match_limits() {
        assert_eq!(LzhMethod::Lzs.min_match(), 2);
        assert_eq!(LzhMethod::Lzs.max_match(), 17);
        assert_eq!(LzhMethod::Lz5.min_match(), 3);
        assert_eq!(LzhMethod::Lz5.max_match(), 18);
        assert_eq!(LzhMethod::Lh2.min_match(), 3);
        assert_eq!(LzhMethod::Lh2.max_match(), 256);
        assert_eq!(LzhMethod::Lh3.min_match(), 3);
        assert_eq!(LzhMethod::Lh3.max_match(), 256);
    }

    #[test]
    fn test_stored_methods() {
        // LArc `-lz4-` and PMarc `-pm0-` are genuine stored formats (the
        // reference decoders route both to a null decoder), so they must not
        // reach a codec.
        assert!(LzhMethod::Lz4.is_stored());
        assert!(LzhMethod::Pm0.is_stored());
        assert!(LzhMethod::Lh0.is_stored());
        assert!(LzhMethod::Lhd.is_stored());
        assert!(!LzhMethod::Lzs.is_stored());
        assert!(!LzhMethod::Lz5.is_stored());
        assert!(!LzhMethod::Lh2.is_stored());
        assert!(!LzhMethod::Lh3.is_stored());
    }

    #[test]
    fn test_position_bits() {
        assert_eq!(LzhMethod::Lh4.position_bits(), 12);
        assert_eq!(LzhMethod::Lh5.position_bits(), 13);
        assert_eq!(LzhMethod::Lh6.position_bits(), 15);
        assert_eq!(LzhMethod::Lh7.position_bits(), 16);
    }

    #[test]
    fn test_canonical_offset_and_history_bits() {
        // Verified against lhasa lh{5,6,7}_decoder.c.
        assert_eq!(LzhMethod::Lh4.offset_bits(), 4);
        assert_eq!(LzhMethod::Lh5.offset_bits(), 4);
        assert_eq!(LzhMethod::Lh6.offset_bits(), 5);
        assert_eq!(LzhMethod::Lh7.offset_bits(), 5);

        assert_eq!(LzhMethod::Lh4.history_bits(), 14);
        assert_eq!(LzhMethod::Lh5.history_bits(), 14);
        assert_eq!(LzhMethod::Lh6.history_bits(), 16);
        assert_eq!(LzhMethod::Lh7.history_bits(), 17);

        assert_eq!(LzhMethod::Lh5.max_offset_codes(), 15);
        assert_eq!(LzhMethod::Lh6.max_offset_codes(), 31);
        assert_eq!(LzhMethod::Lh7.max_offset_codes(), 31);
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
            LzhMethod::Lh2,
            LzhMethod::Lh3,
            LzhMethod::Lh4,
            LzhMethod::Lh5,
            LzhMethod::Lh6,
            LzhMethod::Lh7,
            LzhMethod::Lhd,
            LzhMethod::Lzs,
            LzhMethod::Lz4,
            LzhMethod::Lz5,
            LzhMethod::Pm0,
        ] {
            assert_eq!(LzhMethod::from_id(&m.id()), Some(m));
            assert_ne!(m.name(), "unknown");
        }
    }
}
