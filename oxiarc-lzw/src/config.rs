//! LZW configuration for different formats (TIFF, GIF).

/// LZW configuration parameters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LzwConfig {
    /// Minimum code size in bits (typically 9).
    pub min_bits: u8,
    /// Maximum code size in bits (typically 12).
    pub max_bits: u8,
    /// Whether to use clear code for table reset.
    /// TIFF typically doesn't use clear codes, GIF does.
    pub use_clear_code: bool,
    /// Whether to use early code change.
    /// TIFF uses early change (increase bit width one code earlier).
    pub early_change: bool,
}

impl LzwConfig {
    /// Standard TIFF LZW configuration.
    ///
    /// - MSB-first bit order (handled by bitstream_msb module)
    /// - 9-12 bit codes
    /// - No clear code
    /// - Early code change
    pub const TIFF: Self = Self {
        min_bits: 9,
        max_bits: 12,
        use_clear_code: false,
        early_change: true,
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

    /// Create a new LZW configuration.
    pub fn new(min_bits: u8, max_bits: u8) -> Self {
        Self {
            min_bits,
            max_bits,
            use_clear_code: false,
            early_change: true,
        }
    }

    /// Get the clear code value (256 for 8-bit initial code size).
    pub fn clear_code(&self) -> u16 {
        1 << (self.min_bits - 1)
    }

    /// Get the end-of-information code value (clear_code + 1).
    pub fn eoi_code(&self) -> u16 {
        self.clear_code() + 1
    }

    /// Get the first available code for dictionary entries.
    pub fn first_code(&self) -> u16 {
        self.eoi_code() + 1
    }

    /// Get the maximum number of codes for the maximum bit width.
    pub fn max_code(&self) -> u16 {
        (1 << self.max_bits) - 1
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
        assert!(!config.use_clear_code);
        assert!(config.early_change);
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
}
