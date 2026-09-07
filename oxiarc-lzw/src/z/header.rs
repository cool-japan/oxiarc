//! The three-byte UNIX `compress(1)` container header.

use crate::error::{LzwError, Result};

/// The two magic bytes every `.Z` stream starts with.
pub const MAGIC: [u8; 2] = [0x1F, 0x9D];

/// Bit 7 of the third header byte: block mode (code 256 is a table reset).
pub(crate) const BLOCK_MODE_FLAG: u8 = 0x80;

/// Bits 0-4 of the third header byte: the maximum code width.
pub(crate) const BIT_MASK: u8 = 0x1F;

/// Bits 5-6 of the third header byte are reserved and must be ignored.
///
/// GNU `gzip` warns about them and keeps going; `ncompress` masks them off
/// without comment. This crate follows both: they are ignored, never an
/// error, so a file written by some third dialect still decodes.
pub(crate) const RESERVED_FLAGS: u8 = 0x60;

/// The initial code width of every `.Z` stream (9 bits).
pub(crate) const INIT_BITS: u8 = 9;

/// Smallest `max_bits` a `.Z` header may declare.
pub const MIN_MAX_BITS: u8 = 9;

/// Largest `max_bits` a `.Z` header may declare (`compress -b 16`).
pub const MAX_MAX_BITS: u8 = 16;

/// Code 256: the table-reset code in block mode, an ordinary table entry
/// otherwise.
pub(crate) const CLEAR: u16 = 256;

/// First code the *encoder* assigns after a reset in block mode.
///
/// The decoder deliberately restarts one lower (see [`super::decode`]).
pub(crate) const FIRST: u32 = 257;

/// Parsed `.Z` container header.
///
/// ```rust
/// use oxiarc_lzw::z::ZHeader;
///
/// let header = ZHeader::parse(&[0x1F, 0x9D, 0x90]).expect("a compress -b 16 header");
/// assert_eq!(header.max_bits, 16);
/// assert!(header.block_mode);
/// assert_eq!(header.to_bytes(), [0x1F, 0x9D, 0x90]);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ZHeader {
    /// Maximum code width in bits, 9-16 (`compress -b`). The default of the
    /// reference tool is 16.
    pub max_bits: u8,
    /// Block mode: code 256 resets the table mid-stream.
    ///
    /// Every `compress(1)` in circulation writes block-mode streams; the
    /// flag exists because pre-4.3BSD `compress` did not, and those files
    /// are still legal.
    pub block_mode: bool,
}

impl ZHeader {
    /// Length of the header in bytes.
    pub const LEN: usize = 3;

    /// Build a header, validating `max_bits`.
    ///
    /// # Errors
    ///
    /// [`LzwError::ZUnsupportedMaxBits`] unless `9 <= max_bits <= 16`.
    pub fn new(max_bits: u8, block_mode: bool) -> Result<Self> {
        if !(MIN_MAX_BITS..=MAX_MAX_BITS).contains(&max_bits) {
            return Err(LzwError::ZUnsupportedMaxBits(max_bits));
        }
        Ok(Self {
            max_bits,
            block_mode,
        })
    }

    /// Parse the first three bytes of a `.Z` stream.
    ///
    /// Bits 5-6 of the flags byte are reserved; they are ignored rather
    /// than rejected, matching `gzip` and `ncompress`.
    ///
    /// # Errors
    ///
    /// - [`LzwError::ZTruncatedHeader`] when fewer than three bytes are
    ///   available,
    /// - [`LzwError::ZInvalidMagic`] when the stream does not start with
    ///   `1F 9D`,
    /// - [`LzwError::ZUnsupportedMaxBits`] when the declared width is
    ///   outside 9-16. (The reference decoders on this platform reject
    ///   anything below 12; oxiarc accepts the full 9-16 range that
    ///   `compress -b` can produce.)
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let Some(head) = bytes.get(..Self::LEN) else {
            return Err(LzwError::ZTruncatedHeader { len: bytes.len() });
        };
        if head[0] != MAGIC[0] || head[1] != MAGIC[1] {
            return Err(LzwError::ZInvalidMagic {
                magic: [head[0], head[1]],
            });
        }
        let flags = head[2];
        let _reserved = flags & RESERVED_FLAGS;
        Self::new(flags & BIT_MASK, flags & BLOCK_MODE_FLAG != 0)
    }

    /// Serialize the header.
    #[must_use]
    pub fn to_bytes(&self) -> [u8; Self::LEN] {
        let flags = self.max_bits | if self.block_mode { BLOCK_MODE_FLAG } else { 0 };
        [MAGIC[0], MAGIC[1], flags]
    }

    /// One past the largest code this configuration can assign
    /// (`1 << max_bits`).
    #[must_use]
    pub(crate) fn max_max_code(&self) -> u32 {
        1u32 << self.max_bits
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_width_round_trips_through_the_header() {
        for max_bits in MIN_MAX_BITS..=MAX_MAX_BITS {
            for block_mode in [true, false] {
                let header = ZHeader::new(max_bits, block_mode).expect("valid width");
                let bytes = header.to_bytes();
                assert_eq!(ZHeader::parse(&bytes).expect("re-parse"), header);
            }
        }
    }

    #[test]
    fn the_reference_headers_parse() {
        // `compress -b N -c` writes exactly these on this platform.
        for (bytes, max_bits) in [
            ([0x1F, 0x9D, 0x89], 9),
            ([0x1F, 0x9D, 0x8C], 12),
            ([0x1F, 0x9D, 0x90], 16),
        ] {
            let header = ZHeader::parse(&bytes).expect("reference header");
            assert_eq!(header.max_bits, max_bits);
            assert!(header.block_mode);
        }
    }

    #[test]
    fn reserved_flag_bits_are_ignored_not_rejected() {
        let header = ZHeader::parse(&[0x1F, 0x9D, 0x90 | RESERVED_FLAGS]).expect("reserved bits");
        assert_eq!(header.max_bits, 16);
        assert!(header.block_mode);
    }

    #[test]
    fn bad_headers_are_reported_precisely() {
        assert!(matches!(
            ZHeader::parse(&[]),
            Err(LzwError::ZTruncatedHeader { len: 0 })
        ));
        assert!(matches!(
            ZHeader::parse(&[0x1F, 0x9D]),
            Err(LzwError::ZTruncatedHeader { len: 2 })
        ));
        assert!(matches!(
            ZHeader::parse(&[0x1F, 0x8B, 0x08]),
            Err(LzwError::ZInvalidMagic {
                magic: [0x1F, 0x8B]
            })
        ));
        assert!(matches!(
            ZHeader::parse(&[0x1F, 0x9D, 0x88]),
            Err(LzwError::ZUnsupportedMaxBits(8))
        ));
        assert!(matches!(
            ZHeader::parse(&[0x1F, 0x9D, 0x91]),
            Err(LzwError::ZUnsupportedMaxBits(17))
        ));
        assert!(matches!(
            ZHeader::new(0, true),
            Err(LzwError::ZUnsupportedMaxBits(0))
        ));
    }

    #[test]
    fn block_mode_is_bit_seven() {
        let with_block = ZHeader::new(16, true).expect("16 bits").to_bytes();
        let without = ZHeader::new(16, false).expect("16 bits").to_bytes();
        assert_eq!(with_block[2], 0x90);
        assert_eq!(without[2], 0x10);
        assert!(!ZHeader::parse(&without).expect("parse").block_mode);
    }

    #[test]
    fn max_max_code_matches_the_width() {
        for max_bits in MIN_MAX_BITS..=MAX_MAX_BITS {
            let header = ZHeader::new(max_bits, true).expect("valid");
            assert_eq!(header.max_max_code(), 1u32 << max_bits);
        }
    }
}
