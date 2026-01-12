//! Range coder for LZMA compression.
//!
//! The range coder is an entropy coding method similar to arithmetic coding.
//! LZMA uses a specific variant with:
//! - 32-bit range tracking
//! - Normalization when range drops below 2^24
//! - 11-bit probability model (2048 = 50%)

use oxiarc_core::error::{OxiArcError, Result};
use std::io::Read;

/// Number of bits in probability model.
pub const PROB_BITS: u32 = 11;

/// Probability representing 50% (1 << 10 = 1024, but we use 2048/2).
pub const PROB_INIT: u16 = 1 << (PROB_BITS - 1);

/// Maximum probability value.
pub const PROB_MAX: u16 = 1 << PROB_BITS;

/// Number of bits to shift for probability update.
pub const MOVE_BITS: u32 = 5;

/// Top value for range normalization.
const TOP_VALUE: u32 = 1 << 24;

/// Range decoder for LZMA decompression.
#[derive(Debug)]
pub struct RangeDecoder<R: Read> {
    reader: R,
    range: u32,
    code: u32,
    corrupted: bool,
}

impl<R: Read> RangeDecoder<R> {
    /// Create a new range decoder.
    pub fn new(mut reader: R) -> Result<Self> {
        // Read first byte (should be 0x00)
        let mut buf = [0u8; 1];
        reader.read_exact(&mut buf)?;

        if buf[0] != 0x00 {
            return Err(OxiArcError::invalid_header(
                "Invalid LZMA stream start byte",
            ));
        }

        // Read initial code value (4 bytes, big-endian)
        let mut code_buf = [0u8; 4];
        reader.read_exact(&mut code_buf)?;
        let code = u32::from_be_bytes(code_buf);

        Ok(Self {
            reader,
            range: 0xFFFF_FFFF,
            code,
            corrupted: false,
        })
    }

    /// Create a range decoder for raw LZMA2 stream (no header byte).
    pub fn new_lzma2(mut reader: R) -> Result<Self> {
        // Read initial code value (5 bytes for LZMA2)
        let mut code_buf = [0u8; 5];
        reader.read_exact(&mut code_buf)?;

        // First byte should be 0
        if code_buf[0] != 0 {
            return Err(OxiArcError::invalid_header("Invalid LZMA2 stream"));
        }

        let code = u32::from_be_bytes([code_buf[1], code_buf[2], code_buf[3], code_buf[4]]);

        Ok(Self {
            reader,
            range: 0xFFFF_FFFF,
            code,
            corrupted: false,
        })
    }

    /// Normalize the range (refill when range gets small).
    fn normalize(&mut self) -> Result<()> {
        if self.range < TOP_VALUE {
            let mut buf = [0u8; 1];
            self.reader.read_exact(&mut buf)?;
            self.range <<= 8;
            self.code = (self.code << 8) | buf[0] as u32;
        }
        Ok(())
    }

    /// Decode a single bit with the given probability.
    pub fn decode_bit(&mut self, prob: &mut u16) -> Result<u32> {
        self.normalize()?;

        let bound = (self.range >> PROB_BITS) * (*prob as u32);

        if self.code < bound {
            // Bit is 0
            self.range = bound;
            *prob += (PROB_MAX - *prob) >> MOVE_BITS;
            Ok(0)
        } else {
            // Bit is 1
            self.range -= bound;
            self.code -= bound;
            *prob -= *prob >> MOVE_BITS;
            Ok(1)
        }
    }

    /// Decode a bit with fixed 50% probability.
    pub fn decode_direct_bit(&mut self) -> Result<u32> {
        self.normalize()?;

        self.range >>= 1;
        self.code = self.code.wrapping_sub(self.range);

        let bit = if (self.code as i32) < 0 {
            self.code = self.code.wrapping_add(self.range);
            0
        } else {
            1
        };

        Ok(bit)
    }

    /// Decode multiple bits with fixed probability.
    pub fn decode_direct_bits(&mut self, count: u32) -> Result<u32> {
        let mut result = 0u32;
        for _ in 0..count {
            result = (result << 1) | self.decode_direct_bit()?;
        }
        Ok(result)
    }

    /// Decode a bit tree (reverse order).
    pub fn decode_bit_tree_reverse(&mut self, probs: &mut [u16], num_bits: u32) -> Result<u32> {
        let mut result = 0u32;
        let mut index = 1usize;

        for i in 0..num_bits {
            let bit = self.decode_bit(&mut probs[index])?;
            index = (index << 1) | bit as usize;
            result |= bit << i;
        }

        Ok(result)
    }

    /// Decode a bit tree (normal order).
    pub fn decode_bit_tree(&mut self, probs: &mut [u16], num_bits: u32) -> Result<u32> {
        let mut index = 1usize;

        for _ in 0..num_bits {
            let bit = self.decode_bit(&mut probs[index])?;
            index = (index << 1) | bit as usize;
        }

        Ok((index as u32) - (1 << num_bits))
    }

    /// Check if the stream is corrupted.
    pub fn is_corrupted(&self) -> bool {
        self.corrupted
    }

    /// Check if decoding finished correctly.
    pub fn is_finished_ok(&self) -> bool {
        self.code == 0
    }
}

/// Range encoder for LZMA compression.
#[derive(Debug)]
pub struct RangeEncoder {
    /// Output buffer.
    buffer: Vec<u8>,
    /// Current range.
    range: u32,
    /// Low value.
    low: u64,
    /// Cache byte.
    cache: u8,
    /// Cache size.
    cache_size: u64,
}

impl RangeEncoder {
    /// Create a new range encoder.
    pub fn new() -> Self {
        Self {
            buffer: Vec::new(),
            range: 0xFFFF_FFFF,
            low: 0,
            cache: 0,
            cache_size: 1,
        }
    }

    /// Shift low and write bytes.
    ///
    /// This uses the carry-handling cache mechanism from the LZMA SDK.
    /// The low value is a 64-bit accumulator where bits 32-39 represent overflow (carry).
    fn shift_low(&mut self) {
        // Check if we can output bytes:
        // - low < 0xFF000000: no pending carry propagation needed
        // - low > 0xFFFFFFFF: there's a carry to propagate
        if self.low < 0xFF00_0000 || self.low > 0xFFFF_FFFF {
            // Output pending bytes with carry propagation
            let mut tmp = self.cache;
            let carry = (self.low >> 32) as u8;

            loop {
                let byte = tmp.wrapping_add(carry);
                self.buffer.push(byte);
                tmp = 0xFF; // Subsequent bytes are 0xFF (will become 0x00 if carry)
                self.cache_size -= 1;
                if self.cache_size == 0 {
                    break;
                }
            }

            // New cache is the top byte of the 32-bit low value
            self.cache = (self.low >> 24) as u8;
        }

        // Always increment cache_size (tracks pending bytes)
        self.cache_size += 1;

        // Shift low left by 8 bits, keeping only 32 bits
        self.low = (self.low << 8) & 0xFFFF_FFFF;
    }

    /// Normalize the range.
    fn normalize(&mut self) {
        if self.range < TOP_VALUE {
            self.range <<= 8;
            self.shift_low();
        }
    }

    /// Encode a single bit with the given probability.
    pub fn encode_bit(&mut self, prob: &mut u16, bit: u32) {
        let bound = (self.range >> PROB_BITS) * (*prob as u32);

        if bit == 0 {
            self.range = bound;
            *prob += (PROB_MAX - *prob) >> MOVE_BITS;
        } else {
            self.low += bound as u64;
            self.range -= bound;
            *prob -= *prob >> MOVE_BITS;
        }

        self.normalize();
    }

    /// Encode a bit with fixed 50% probability.
    pub fn encode_direct_bit(&mut self, bit: u32) {
        self.range >>= 1;
        if bit != 0 {
            self.low += self.range as u64;
        }
        self.normalize();
    }

    /// Encode multiple bits with fixed probability.
    pub fn encode_direct_bits(&mut self, value: u32, count: u32) {
        for i in (0..count).rev() {
            self.encode_direct_bit((value >> i) & 1);
        }
    }

    /// Encode a bit tree (reverse order).
    pub fn encode_bit_tree_reverse(&mut self, probs: &mut [u16], num_bits: u32, value: u32) {
        let mut index = 1usize;

        for i in 0..num_bits {
            let bit = (value >> i) & 1;
            self.encode_bit(&mut probs[index], bit);
            index = (index << 1) | bit as usize;
        }
    }

    /// Encode a bit tree (normal order).
    pub fn encode_bit_tree(&mut self, probs: &mut [u16], num_bits: u32, value: u32) {
        let mut index = 1usize;

        for i in (0..num_bits).rev() {
            let bit = (value >> i) & 1;
            self.encode_bit(&mut probs[index], bit);
            index = (index << 1) | bit as usize;
        }
    }

    /// Flush the encoder.
    pub fn flush(&mut self) {
        for _ in 0..5 {
            self.shift_low();
        }
    }

    /// Get the encoded data.
    pub fn finish(mut self) -> Vec<u8> {
        self.flush();
        self.buffer
    }
}

impl Default for RangeEncoder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn test_prob_constants() {
        assert_eq!(PROB_INIT, 1024);
        assert_eq!(PROB_MAX, 2048);
    }

    #[test]
    fn test_range_encoder_basic() {
        let encoder = RangeEncoder::new();
        assert_eq!(encoder.range, 0xFFFF_FFFF);
    }

    #[test]
    fn test_encode_decode_bits() {
        // Encode some bits
        let mut encoder = RangeEncoder::new();
        let mut prob = PROB_INIT;

        encoder.encode_bit(&mut prob, 0);
        encoder.encode_bit(&mut prob, 1);
        encoder.encode_bit(&mut prob, 0);
        encoder.encode_bit(&mut prob, 1);

        let encoded = encoder.finish();

        // The encoder output already includes the leading 0x00 byte
        // through its cache mechanism, so we use it directly
        let cursor = Cursor::new(encoded);
        let mut decoder = RangeDecoder::new(cursor).unwrap();
        let mut prob = PROB_INIT;

        assert_eq!(decoder.decode_bit(&mut prob).unwrap(), 0);
        assert_eq!(decoder.decode_bit(&mut prob).unwrap(), 1);
        assert_eq!(decoder.decode_bit(&mut prob).unwrap(), 0);
        assert_eq!(decoder.decode_bit(&mut prob).unwrap(), 1);
    }
}
