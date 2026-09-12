//! LZW configuration for different formats (TIFF, GIF, UNIX `compress`).

use crate::error::{LzwError, Result};

/// Order in which the bits of a variable-width LZW code are packed into
/// bytes.
///
/// The two orders are **not** interchangeable: the same bytes decode to
/// different codes under each, so a stream must be read with the order its
/// writer used.
///
/// | Format | Order |
/// |---|---|
/// | TIFF 6.0 §13 (`Compression = 5`) | [`LzwBitOrder::Msb`] |
/// | GIF 89a image data | [`LzwBitOrder::Lsb`] |
/// | UNIX `compress(1)` / `.Z` (see [`crate::z`]) | [`LzwBitOrder::Lsb`] |
/// | libtiff's pre-1993 `LZWDecodeCompat` strips | [`LzwBitOrder::Lsb`] |
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum LzwBitOrder {
    /// Most-significant-bit first: the first code occupies the high bits of
    /// the first byte. Used by TIFF.
    Msb,
    /// Least-significant-bit first: the first code occupies the low bits of
    /// the first byte. Used by GIF and by UNIX `compress(1)`.
    Lsb,
}

/// LZW configuration parameters.
///
/// The [`Default`] impl returns the standard TIFF configuration (9-12 bit
/// codes, clear codes, early code change, MSB-first), matching
/// [`LzwConfig::TIFF`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LzwConfig {
    /// Minimum code size in bits (typically 9).
    pub min_bits: u8,
    /// Maximum code size in bits (9-16; 12 for TIFF and GIF, up to 16 for
    /// UNIX `compress`).
    pub max_bits: u8,
    /// Whether to use clear code for table reset.
    /// Both TIFF 6.0 and GIF use clear codes.
    pub use_clear_code: bool,
    /// Whether to use early code change.
    /// TIFF uses early change (increase bit width one code earlier).
    pub early_change: bool,
    /// Bit order used to pack codes into bytes.
    pub bit_order: LzwBitOrder,
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
        bit_order: LzwBitOrder::Msb,
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
    /// path: that one additionally packs codes LSB-first. Set
    /// `bit_order: LzwBitOrder::Lsb` on a copy of this constant (or use
    /// [`LzwConfig::TIFF_COMPAT_LSB`]) to decode those.
    ///
    /// See [`crate::decompress_tiff_into`] for how to fall back to this
    /// configuration safely, and for the case no fallback rule can catch.
    pub const TIFF_OLD_STYLE: Self = Self {
        min_bits: 9,
        max_bits: 12,
        use_clear_code: true,
        early_change: false,
        bit_order: LzwBitOrder::Msb,
    };

    /// libtiff's `LZWDecodeCompat` configuration: pre-1993 TIFF writers that
    /// packed codes **LSB-first** and used the standard (late) code-width
    /// change.
    ///
    /// This is [`LzwConfig::TIFF_OLD_STYLE`] with
    /// `bit_order: LzwBitOrder::Lsb`. libtiff selects its `LZWDecodeCompat`
    /// path for exactly these strips; before 0.4.2 this crate could not
    /// decode them at all, because it had no LSB-first code reader.
    pub const TIFF_COMPAT_LSB: Self = Self {
        min_bits: 9,
        max_bits: 12,
        use_clear_code: true,
        early_change: false,
        bit_order: LzwBitOrder::Lsb,
    };

    /// GIF-flavoured LZW configuration for the generic entry points:
    /// **LSB-first**, 9-12 bit codes, clear codes, standard (late) code
    /// change.
    ///
    /// # Not the GIF codec
    ///
    /// This constant configures the *generic* [`crate::compress`] /
    /// [`crate::decompress`] / [`crate::decompress_into`] engine with GIF's
    /// bit order and width rule. It is **not** the GIF 89a image-data codec:
    /// GIF derives its clear code, EOI code and initial width from the
    /// image descriptor's `minimum_code_size` (2-11), which this constant
    /// hard-codes to the 8-bit case, and GIF data is framed in sub-blocks.
    /// Real GIF image data must go through [`crate::gif_compress`] /
    /// [`crate::gif_decompress`], which own those rules.
    ///
    /// At `minimum_code_size == 8` the two encoders happen to agree byte for
    /// byte for every input that never fills the 4096-entry table (measured
    /// over 3 004 shapes), and diverge once it does: `gif_compress` emits its
    /// ClearCode at the next entry it cannot store, while the generic engine
    /// emits it as soon as the last slot is taken. Neither is "the" GIF
    /// encoder for this crate's purposes — use `gif_compress` for GIF.
    ///
    /// Before 0.4.2 this constant had exactly the same field values as
    /// [`LzwConfig::TIFF_OLD_STYLE`] and therefore silently decoded
    /// MSB-first. It is now genuinely LSB-first, so `LzwConfig::GIF` and
    /// `LzwConfig::TIFF_OLD_STYLE` are different configurations that decode
    /// the same bytes differently.
    pub const GIF: Self = Self {
        min_bits: 9,
        max_bits: 12,
        use_clear_code: true,
        early_change: false,
        bit_order: LzwBitOrder::Lsb,
    };

    /// Create a new LZW configuration with TIFF-style clear codes, early
    /// code change and MSB-first packing.
    ///
    /// Use [`LzwConfig::with_bit_order`] to switch the packing.
    ///
    /// # Errors
    ///
    /// Returns [`LzwError::InvalidBitWidth`] unless
    /// `9 <= min_bits <= max_bits <= 16`.
    ///
    /// # Example
    ///
    /// ```rust
    /// use oxiarc_lzw::{LzwBitOrder, LzwConfig};
    ///
    /// let config = LzwConfig::new(9, 16)
    ///     .expect("9-16 is a valid width range")
    ///     .with_bit_order(LzwBitOrder::Lsb);
    /// assert_eq!(config.max_code(), 65535);
    /// ```
    pub fn new(min_bits: u8, max_bits: u8) -> Result<Self> {
        let config = Self {
            min_bits,
            max_bits,
            use_clear_code: true,
            early_change: true,
            bit_order: LzwBitOrder::Msb,
        };
        config.validate()?;
        Ok(config)
    }

    /// Return a copy of this configuration with a different bit order.
    #[must_use]
    pub const fn with_bit_order(mut self, bit_order: LzwBitOrder) -> Self {
        self.bit_order = bit_order;
        self
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
    /// unless `9 <= min_bits <= max_bits <= 16`.
    ///
    /// 12 is the TIFF/GIF ceiling; widths up to 16 exist so that the UNIX
    /// `compress(1)` code widths (`.Z` files, see [`crate::z`]) are
    /// expressible. A 16-bit table costs about 640 KiB.
    pub fn validate(&self) -> Result<()> {
        if self.min_bits < 9 || self.min_bits > self.max_bits {
            return Err(LzwError::InvalidBitWidth(self.min_bits));
        }
        if self.max_bits > Self::MAX_SUPPORTED_BITS {
            return Err(LzwError::InvalidBitWidth(self.max_bits));
        }
        Ok(())
    }

    /// Largest `max_bits` this crate supports (16, the UNIX `compress`
    /// ceiling).
    pub const MAX_SUPPORTED_BITS: u8 = 16;

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

    /// Get the maximum code value for the maximum bit width (4095 for 12
    /// bits, 65535 for 16).
    ///
    /// Like [`Self::clear_code`], this saturates instead of panicking on an
    /// out-of-range `max_bits` from a struct-literal config.
    pub fn max_code(&self) -> u16 {
        let bits = u32::from(self.max_bits.min(Self::MAX_SUPPORTED_BITS));
        ((1u32 << bits) - 1).min(u32::from(u16::MAX)) as u16
    }
}

impl Default for LzwConfig {
    /// Defaults to the standard TIFF configuration (9-12 bit codes, clear
    /// codes, early code change, MSB-first). A derived all-zero `Default` would be an
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
        assert_eq!(config.bit_order, LzwBitOrder::Lsb);
    }

    #[test]
    fn gif_and_tiff_old_style_are_no_longer_the_same_configuration() {
        // Before 0.4.2 these two constants were literally equal, so
        // `decompress_into(_, _, LzwConfig::GIF)` silently decoded
        // MSB-first. The bit-order field is what separates them.
        assert_ne!(LzwConfig::GIF, LzwConfig::TIFF_OLD_STYLE);
        assert_eq!(LzwConfig::GIF.bit_order, LzwBitOrder::Lsb);
        assert_eq!(LzwConfig::TIFF_OLD_STYLE.bit_order, LzwBitOrder::Msb);
        assert_eq!(
            LzwConfig::TIFF_COMPAT_LSB,
            LzwConfig::TIFF_OLD_STYLE.with_bit_order(LzwBitOrder::Lsb)
        );
    }

    #[test]
    fn sixteen_bit_configurations_size_their_tables_correctly() {
        for bits in 9u8..=16 {
            let config = LzwConfig::new(9, bits).expect("valid width range");
            assert!(config.validate().is_ok());
            let expected = u16::try_from((1u32 << u32::from(bits)) - 1).unwrap_or(u16::MAX);
            assert_eq!(config.max_code(), expected, "max_bits {bits}");
        }
        assert_eq!(LzwConfig::new(9, 16).expect("9-16").max_code(), 65535);
    }

    #[test]
    fn test_new_validates() {
        assert!(LzwConfig::new(9, 12).is_ok());
        assert!(LzwConfig::new(9, 9).is_ok());
        assert!(LzwConfig::new(0, 12).is_err());
        assert!(LzwConfig::new(8, 12).is_err());
        assert!(LzwConfig::new(9, 13).is_ok());
        assert!(LzwConfig::new(9, 16).is_ok());
        assert!(LzwConfig::new(16, 16).is_ok());
        assert!(LzwConfig::new(9, 17).is_err());
        assert!(LzwConfig::new(12, 9).is_err());
        assert!(LzwConfig::new(255, 255).is_err());
    }

    #[test]
    fn test_struct_literal_config_never_panics() {
        // Regression for LZW-03: an invalid struct-literal config must not
        // panic (previously `clear_code()` did `1 << (0 - 1)`), only return
        // clamped values; `validate()` is what rejects it.
        for (min_bits, max_bits) in [(0u8, 0u8), (0, 12), (1, 200), (255, 255), (8, 16)] {
            let config = LzwConfig {
                min_bits,
                max_bits,
                use_clear_code: false,
                early_change: true,
                bit_order: LzwBitOrder::Msb,
            };
            let _ = config.clear_code();
            let _ = config.eoi_code();
            let _ = config.first_code();
            let _ = config.max_code();
            assert!(config.validate().is_err());
        }
    }
}
