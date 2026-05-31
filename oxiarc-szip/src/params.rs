use crate::SzipError;

/// Parameters governing AEC/SZIP encoding and decoding.
///
/// These correspond closely to the parameters exposed by libaec and the
/// CCSDS-121.0-B-2 standard.
#[derive(Debug, Clone)]
pub struct SzipParams {
    /// Bits encoded per sample (1–32). Common values: 8, 16, 32.
    pub bits_per_pixel: u8,

    /// Number of samples per coding block (J). Must be a power of 2: 8, 16, or 32.
    pub pixels_per_block: u32,

    /// Total number of samples to decode.
    pub samples: usize,

    /// Reference sample interval in samples (= rsi_blocks × pixels_per_block).
    /// Setting this to 0 means the entire stream is treated as a single RSI.
    pub reference_sample_interval: u32,

    /// `true` = read/write compressed bits MSB-first (most common for
    /// hardware/CHIPS mode).  `false` = LSB-first.
    pub msb: bool,

    /// `true` = the AEC samples have been preprocessed with the unit-delay NN
    /// predictor. The decoder must apply the inverse preprocessing step.
    pub nn_preprocess: bool,

    /// `true` = each RSI boundary is byte-aligned in the bit stream (CHIP
    /// hardware mode). The encoder pads each RSI to the next byte boundary,
    /// and the decoder skips that padding.
    ///
    /// This is required by some hardware AEC implementations and by the HDF5
    /// szip filter when the `AEC_DATA_SIGNED` / `AEC_CHIP_OPTION` flags are
    /// set. Defaults to `false` for pure software streams.
    pub rsi_byte_align: bool,
}

impl SzipParams {
    /// Length of the option-ID field in bits.
    ///
    /// - bpp  1– 2 → 1 bit
    /// - bpp  3– 4 → 2 bits
    /// - bpp  5– 8 → 3 bits
    /// - bpp  9–16 → 4 bits
    /// - bpp 17–32 → 5 bits
    pub fn id_len(&self) -> u8 {
        match self.bits_per_pixel {
            1..=2 => 1,
            3..=4 => 2,
            5..=8 => 3,
            9..=16 => 4,
            _ => 5,
        }
    }

    /// Maximum Golomb-Rice `k` parameter: `bpp - 2`, capped at 14.
    pub fn k_max(&self) -> u8 {
        (self.bits_per_pixel.saturating_sub(2)).min(14)
    }

    /// Option ID that signals an uncompressed (no-compression) block.
    /// This is the all-ones pattern of `id_len()` bits.
    pub fn id_no_compress(&self) -> u32 {
        (1u32 << self.id_len()) - 1
    }

    /// Maximum representable sample value: `(1 << bpp) - 1`.
    pub fn xmax(&self) -> u64 {
        if self.bits_per_pixel >= 64 {
            u64::MAX
        } else {
            (1u64 << self.bits_per_pixel) - 1
        }
    }

    /// Number of bytes needed to store one sample in the output byte array.
    pub fn bytes_per_sample(&self) -> usize {
        match self.bits_per_pixel {
            1..=8 => 1,
            9..=16 => 2,
            17..=32 => 4,
            _ => 8,
        }
    }

    /// Validate that the parameters are within their legal ranges.
    pub fn validate(&self) -> Result<(), SzipError> {
        if self.bits_per_pixel == 0 || self.bits_per_pixel > 32 {
            return Err(SzipError::InvalidParam("bits_per_pixel must be 1..=32"));
        }
        if !matches!(self.pixels_per_block, 8 | 16 | 32) {
            return Err(SzipError::InvalidParam(
                "pixels_per_block must be 8, 16, or 32",
            ));
        }
        Ok(())
    }
}
