//! Encoding/decoding parameters for the AEC/SZIP codec.

use crate::SzipError;

/// Parameters governing AEC/SZIP encoding and decoding.
///
/// These correspond closely to the parameters exposed by libaec and the
/// CCSDS-121.0-B-2 standard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SzipParams {
    /// Bits encoded per sample (1–32). Common values: 8, 16, 32.
    pub bits_per_pixel: u8,

    /// Number of samples per coding block (J). Must be 8, 16, 32, or 64.
    pub pixels_per_block: u32,

    /// Total number of samples to decode.
    pub samples: usize,

    /// Reference sample interval in samples (= rsi_blocks × pixels_per_block).
    /// Setting this to 0 means the entire stream is treated as a single RSI.
    pub reference_sample_interval: u32,

    /// `true` = read/write compressed bits MSB-first — the CCSDS-121 /
    /// libaec bit ordering, required for interoperability. `false` =
    /// LSB-first, a crate-local extension that only round-trips with this
    /// crate.
    pub msb: bool,

    /// `true` = the AEC samples have been preprocessed with the unit-delay NN
    /// predictor. The decoder must apply the inverse preprocessing step.
    pub nn_preprocess: bool,

    /// `true` = each RSI boundary is byte-aligned in the bit stream
    /// (libaec's `AEC_PAD_RSI`). The encoder pads each RSI to the next byte
    /// boundary, and the decoder skips that padding.
    ///
    /// This matches some hardware AEC implementations and certain CCSDS
    /// sample data sets. Defaults to `false` for pure software streams.
    pub rsi_byte_align: bool,
}

impl Default for SzipParams {
    /// Common 8-bit, pure-software AEC/SZIP stream settings.
    ///
    /// - `bits_per_pixel`: 8
    /// - `pixels_per_block`: 8
    /// - `samples`: 0 (caller must set this to the actual sample count)
    /// - `reference_sample_interval`: 8 (one reference sample per block)
    /// - `msb`: `true` (MSB-first bit ordering)
    /// - `nn_preprocess`: `false`
    /// - `rsi_byte_align`: `false`
    fn default() -> Self {
        SzipParams {
            bits_per_pixel: 8,
            pixels_per_block: 8,
            samples: 0,
            reference_sample_interval: 8,
            msb: true,
            nn_preprocess: false,
            rsi_byte_align: false,
        }
    }
}

impl SzipParams {
    /// Length of the option-ID field in bits.
    ///
    /// Matches the CCSDS-121.0-B-2 *basic* (unrestricted) coder and libaec's
    /// default mode:
    ///
    /// - bpp  1– 8 → 3 bits
    /// - bpp  9–16 → 4 bits
    /// - bpp 17–32 → 5 bits
    ///
    /// (The 1/2-bit ID fields of the CCSDS *restricted* option set —
    /// libaec's `AEC_RESTRICTED` flag — are not supported.)
    pub fn id_len(&self) -> u8 {
        match self.bits_per_pixel {
            1..=8 => 3,
            9..=16 => 4,
            _ => 5,
        }
    }

    /// Maximum Golomb-Rice `k` (split-sample) parameter.
    ///
    /// The option-ID field can express `k = 0 ..= 2^id_len - 3` (IDs `0` and
    /// all-ones are reserved for the low-entropy and no-compression options),
    /// and a split position of `bpp - 1` or more can never beat the
    /// no-compression option, so conforming encoders (libaec included) only
    /// ever emit `k <= bpp - 2`. The effective bound is the smaller of the
    /// two.
    pub fn k_max(&self) -> u8 {
        let id_space = (1u8 << self.id_len()) - 3;
        (self.bits_per_pixel.saturating_sub(2)).min(id_space)
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
        if !matches!(self.pixels_per_block, 8 | 16 | 32 | 64) {
            return Err(SzipError::InvalidParam(
                "pixels_per_block must be 8, 16, 32, or 64",
            ));
        }
        // Reference sample intervals are defined in whole blocks (libaec's
        // `rsi` parameter counts blocks); a partial-block interval has no
        // representation in the bit stream.
        if self.reference_sample_interval != 0
            && self.reference_sample_interval % self.pixels_per_block != 0
        {
            return Err(SzipError::InvalidParam(
                "reference_sample_interval must be 0 or a multiple of pixels_per_block",
            ));
        }
        Ok(())
    }
}
