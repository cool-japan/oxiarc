//! LZW configuration for different formats (TIFF, GIF).

use crate::error::{LzwError, Result};

/// LZW configuration parameters.
///
/// The [`Default`] impl returns the standard TIFF configuration (9-12 bit
/// codes, clear codes, early code change), matching [`LzwConfig::TIFF`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LzwConfig {
    /// Minimum code size in bits (typically 9).
    pub min_bits: u8,
    /// Maximum code size in bits (typically 12).
    pub max_bits: u8,
    /// Whether to use clear code for table reset.
    /// Both TIFF 6.0 and GIF use clear codes.
    pub use_clear_code: bool,
    /// Whether to use early code change.
    /// TIFF uses early change (increase bit width one code earlier).
    pub early_change: bool,
}

impl LzwConfig {
    /// Standard TIFF 6.0 LZW configuration.
    ///
    /// - MSB-first bit order (handled by bitstream_msb module)
    /// - 9-12 bit codes
    /// - Clear codes: every strip begins with a ClearCode (256), and the
    ///   encoder emits another ClearCode when the code table reaches entry
    ///   4094 (matching libtiff / Pillow / GDAL / Photoshop)
    /// - Early code change
    pub const TIFF: Self = Self {
        min_bits: 9,
        max_bits: 12,
        use_clear_code: true,
        early_change: true,
    };

    /// Old-style TIFF LZW configuration: the standard (late) code-width
    /// change instead of TIFF's early change.
    ///
    /// Identical to [`LzwConfig::TIFF`] — same MSB-first packing, same
    /// ClearCode/EOI handling — except that the code width grows when
    /// `next_code` reaches `2^current_bits + 1` rather than one code
    /// earlier. That is what a writer following TIFF 6.0's own pseudo-code
    /// literally produces, instead of the early change libtiff, Pillow and
    /// GDAL all implement, and it is the reason such strips decode to
    /// garbage (or to `InvalidCode`) under [`LzwConfig::TIFF`].
    ///
    /// This is **not** the same variant as libtiff's `LZWDecodeCompat`
    /// path: that one handles streams whose codes are packed LSB-first,
    /// which this crate's MSB-first bit reader does not decode at all.
    ///
    /// See [`crate::decompress_tiff_into`] for how to fall back to this
    /// configuration safely, and for the case no fallback rule can catch.
    pub const TIFF_OLD_STYLE: Self = Self {
        min_bits: 9,
        max_bits: 12,
        use_clear_code: true,
        early_change: false,
    };

    /// Standard GIF LZW configuration.
    ///
    /// - LSB-first bit order (would need different bitstream)
    /// - 9-12 bit codes
    /// - Uses clear code
    /// - Standard code change
    pub const GIF: Self = Self {
        min_bits: 9,
        max_bits: 12,
        use_clear_code: true,
        early_change: false,
    };

    /// Create a new LZW configuration with TIFF-style clear codes and early
    /// code change.
    ///
    /// # Errors
    ///
    /// Returns [`LzwError::InvalidBitWidth`] unless
    /// `9 <= min_bits <= max_bits <= 12`.
    pub fn new(min_bits: u8, max_bits: u8) -> Result<Self> {
        let config = Self {
            min_bits,
            max_bits,
            use_clear_code: true,
            early_change: true,
        };
        config.validate()?;
        Ok(config)
    }

    /// Validate the bit-width parameters.
    ///
    /// Public struct fields make it possible to build an out-of-range config
    /// via a struct literal; [`crate::LzwEncoder::new`] and
    /// [`crate::LzwDecoder::new`] call this before using the config.
    ///
    /// # Errors
    ///
    /// Returns [`LzwError::InvalidBitWidth`] (carrying the offending value)
    /// unless `9 <= min_bits <= max_bits <= 12`.
    pub fn validate(&self) -> Result<()> {
        if self.min_bits < 9 || self.min_bits > self.max_bits {
            return Err(LzwError::InvalidBitWidth(self.min_bits));
        }
        if self.max_bits > 12 {
            return Err(LzwError::InvalidBitWidth(self.max_bits));
        }
        Ok(())
    }

    /// Get the clear code value (256 for the standard 9-bit initial size).
    ///
    /// Uses saturating arithmetic so that an invalid struct-literal config
    /// (e.g. `min_bits: 0`) yields a bogus-but-harmless value instead of
    /// panicking; [`Self::validate`] is the authoritative gate.
    pub fn clear_code(&self) -> u16 {
        1u16 << self.min_bits.saturating_sub(1).min(15)
    }

    /// Get the end-of-information code value (clear_code + 1).
    pub fn eoi_code(&self) -> u16 {
        self.clear_code().saturating_add(1)
    }

    /// Get the first available code for dictionary entries.
    pub fn first_code(&self) -> u16 {
        self.eoi_code().saturating_add(1)
    }

    /// Get the maximum code value for the maximum bit width (4095 for 12 bits).
    ///
    /// Like [`Self::clear_code`], this saturates instead of panicking on an
    /// out-of-range `max_bits` from a struct-literal config.
    pub fn max_code(&self) -> u16 {
        ((1u32 << u32::from(self.max_bits.min(15))) - 1) as u16
    }
}

impl Default for LzwConfig {
    /// Defaults to the standard TIFF configuration (9-12 bit codes, clear
    /// codes, early code change). A derived all-zero `Default` would be an
    /// invalid configuration (`min_bits: 0` fails [`LzwConfig::validate`]),
    /// so this is a hand-written impl that returns a sensible, usable
    /// default rather than a bitwise-zero one.
    fn default() -> Self {
        Self::TIFF
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tiff_config() {
        let config = LzwConfig::TIFF;
        assert_eq!(config.min_bits, 9);
        assert_eq!(config.max_bits, 12);
        assert_eq!(config.clear_code(), 256);
        assert_eq!(config.eoi_code(), 257);
        assert_eq!(config.first_code(), 258);
        assert_eq!(config.max_code(), 4095);
        assert!(config.use_clear_code, "TIFF 6.0 mandates clear codes");
        assert!(config.early_change);
    }

    #[test]
    fn test_default_config_is_tiff() {
        assert_eq!(LzwConfig::default(), LzwConfig::TIFF);
    }

    #[test]
    fn test_tiff_old_style_config() {
        let config = LzwConfig::TIFF_OLD_STYLE;
        assert_eq!(config.min_bits, 9);
        assert_eq!(config.max_bits, 12);
        assert!(config.use_clear_code);
        assert!(!config.early_change, "old-style TIFF has no early change");
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_gif_config() {
        let config = LzwConfig::GIF;
        assert_eq!(config.min_bits, 9);
        assert_eq!(config.max_bits, 12);
        assert_eq!(config.clear_code(), 256);
        assert_eq!(config.eoi_code(), 257);
        assert!(config.use_clear_code);
        assert!(!config.early_change);
    }

    #[test]
    fn test_new_validates() {
        assert!(LzwConfig::new(9, 12).is_ok());
        assert!(LzwConfig::new(9, 9).is_ok());
        assert!(LzwConfig::new(0, 12).is_err());
        assert!(LzwConfig::new(8, 12).is_err());
        assert!(LzwConfig::new(9, 13).is_err());
        assert!(LzwConfig::new(12, 9).is_err());
        assert!(LzwConfig::new(255, 255).is_err());
    }

    #[test]
    fn test_struct_literal_config_never_panics() {
        // Regression for LZW-03: an invalid struct-literal config must not
        // panic (previously `clear_code()` did `1 << (0 - 1)`), only return
        // clamped values; `validate()` is what rejects it.
        for (min_bits, max_bits) in [(0u8, 0u8), (0, 12), (1, 200), (255, 255), (16, 16)] {
            let config = LzwConfig {
                min_bits,
                max_bits,
                use_clear_code: false,
                early_change: true,
            };
            let _ = config.clear_code();
            let _ = config.eoi_code();
            let _ = config.first_code();
            let _ = config.max_code();
            assert!(config.validate().is_err());
        }
    }
}
