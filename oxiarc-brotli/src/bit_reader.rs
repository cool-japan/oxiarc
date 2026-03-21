//! Bit-level reader for Brotli decompression.
//!
//! Brotli uses a little-endian bit ordering: bits are read from LSB to MSB
//! within each byte, and bytes are consumed in order.

use crate::error::{BrotliError, BrotliResult};

/// A bit reader that reads individual bits from a byte slice.
///
/// Brotli uses little-endian bit ordering: bits are consumed from
/// the least-significant bit toward the most-significant bit within
/// each byte.
#[derive(Debug)]
pub struct BitReader<'a> {
    data: &'a [u8],
    byte_pos: usize,
    /// Accumulated bit buffer for fast multi-bit reads.
    bit_buf: u64,
    /// Number of valid bits currently in `bit_buf`.
    bits_in_buf: u32,
}

impl<'a> BitReader<'a> {
    /// Create a new bit reader over a byte slice.
    pub fn new(data: &'a [u8]) -> Self {
        let mut reader = BitReader {
            data,
            byte_pos: 0,
            bit_buf: 0,
            bits_in_buf: 0,
        };
        reader.fill_buffer();
        reader
    }

    /// Fill the bit buffer with as many bytes as possible.
    fn fill_buffer(&mut self) {
        while self.bits_in_buf <= 56 && self.byte_pos < self.data.len() {
            self.bit_buf |= (self.data[self.byte_pos] as u64) << self.bits_in_buf;
            self.byte_pos += 1;
            self.bits_in_buf += 8;
        }
    }

    /// Read `n` bits (up to 32) and return as u32.
    pub fn read_bits(&mut self, n: u32) -> BrotliResult<u32> {
        if n == 0 {
            return Ok(0);
        }
        if n > 32 {
            return Err(BrotliError::InvalidParameter(format!(
                "cannot read {n} bits at once (max 32)"
            )));
        }
        self.ensure_bits(n)?;
        let mask = if n == 32 { u32::MAX } else { (1u32 << n) - 1 };
        let val = (self.bit_buf as u32) & mask;
        self.bit_buf >>= n;
        self.bits_in_buf -= n;
        self.fill_buffer();
        Ok(val)
    }

    /// Peek at the next `n` bits without consuming them.
    ///
    /// Near the end of the stream, fewer than `n` bits may be available.
    /// In that case, the missing bits are treated as 0 (Brotli padding).
    /// This is safe for Huffman LUT lookup since `drop_bits` only drops
    /// the actual code length, not the full `n` bits.
    pub fn peek_bits(&mut self, n: u32) -> BrotliResult<u32> {
        if n == 0 {
            return Ok(0);
        }
        self.ensure_bits_for_peek(n)?;
        let mask = if n == 32 { u32::MAX } else { (1u32 << n) - 1 };
        Ok((self.bit_buf as u32) & mask)
    }

    /// Drop `n` bits that were previously peeked.
    pub fn drop_bits(&mut self, n: u32) {
        self.bit_buf >>= n;
        self.bits_in_buf = self.bits_in_buf.saturating_sub(n);
        self.fill_buffer();
    }

    /// Read a single bit.
    pub fn read_bit(&mut self) -> BrotliResult<bool> {
        Ok(self.read_bits(1)? != 0)
    }

    /// Ensure at least `n` bits are available in the buffer.
    fn ensure_bits(&mut self, n: u32) -> BrotliResult<()> {
        if self.bits_in_buf >= n {
            return Ok(());
        }
        // Try to fill more
        self.fill_buffer();
        if self.bits_in_buf >= n {
            Ok(())
        } else {
            Err(BrotliError::UnexpectedEof)
        }
    }

    /// Ensure at least `n` bits are available, allowing zero-padding.
    /// Used for peek operations where the caller will only consume as many
    /// bits as are actually in a valid code. Brotli padding bits are 0.
    fn ensure_bits_for_peek(&mut self, n: u32) -> BrotliResult<()> {
        if self.bits_in_buf >= n {
            return Ok(());
        }
        self.fill_buffer();
        if self.bits_in_buf >= n {
            return Ok(());
        }
        // If we have at least 1 bit, allow zero-padding.
        // Zero padding is safe for Huffman LUT lookup since:
        // 1. The actual code fits in the bits we have.
        // 2. Padding zeros won't produce a valid longer code.
        if self.bits_in_buf > 0 {
            Ok(())
        } else {
            Err(BrotliError::UnexpectedEof)
        }
    }

    /// Read bits and return as u8.
    pub fn read_u8(&mut self, n: u32) -> BrotliResult<u8> {
        Ok(self.read_bits(n)? as u8)
    }

    /// Check if there are any remaining bits/bytes.
    pub fn has_more(&self) -> bool {
        self.bits_in_buf > 0 || self.byte_pos < self.data.len()
    }

    /// Return total number of bits consumed so far.
    pub fn bits_consumed(&self) -> usize {
        self.byte_pos * 8 - self.bits_in_buf as usize
    }

    /// Read a variable-length integer used in Brotli for various lengths.
    /// Reads 1 bit: if 0, return 0. If 1, read `n` more bits.
    pub fn read_variable_length(&mut self, n: u32) -> BrotliResult<u32> {
        if self.read_bit()? {
            self.read_bits(n)
        } else {
            Ok(0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_read_bits_basic() {
        // 0b10110100 = 0xB4
        let data = [0xB4];
        let mut reader = BitReader::new(&data);
        // Read 4 bits: should get LSB nibble = 0b0100 = 4
        assert_eq!(reader.read_bits(4).ok(), Some(4));
        // Read 4 bits: should get MSB nibble = 0b1011 = 11
        assert_eq!(reader.read_bits(4).ok(), Some(11));
    }

    #[test]
    fn test_read_single_bits() {
        let data = [0b10110001];
        let mut reader = BitReader::new(&data);
        // LSB first: 1, 0, 0, 0, 1, 1, 0, 1
        assert_eq!(reader.read_bit().ok(), Some(true));
        assert_eq!(reader.read_bit().ok(), Some(false));
        assert_eq!(reader.read_bit().ok(), Some(false));
        assert_eq!(reader.read_bit().ok(), Some(false));
        assert_eq!(reader.read_bit().ok(), Some(true));
        assert_eq!(reader.read_bit().ok(), Some(true));
        assert_eq!(reader.read_bit().ok(), Some(false));
        assert_eq!(reader.read_bit().ok(), Some(true));
    }

    #[test]
    fn test_read_cross_byte_boundary() {
        let data = [0xFF, 0x00];
        let mut reader = BitReader::new(&data);
        assert_eq!(reader.read_bits(12).ok(), Some(0x0FF));
        assert_eq!(reader.read_bits(4).ok(), Some(0x00));
    }

    #[test]
    fn test_peek_and_drop() {
        let data = [0xAB];
        let mut reader = BitReader::new(&data);
        assert_eq!(reader.peek_bits(4).ok(), Some(0x0B));
        reader.drop_bits(4);
        assert_eq!(reader.read_bits(4).ok(), Some(0x0A));
    }

    #[test]
    fn test_unexpected_eof() {
        let data = [0xFF];
        let mut reader = BitReader::new(&data);
        let _ = reader.read_bits(8);
        assert!(reader.read_bits(1).is_err());
    }

    #[test]
    fn test_variable_length() {
        // First bit 0 => return 0
        let data = [0x00];
        let mut reader = BitReader::new(&data);
        assert_eq!(reader.read_variable_length(4).ok(), Some(0));

        // First bit 1, then 4 bits = 0b1010 = 10, but encoded little-endian
        // 0b10101_1 in byte = 0b0010_1011 but we need to think in bit order
        let data = [0b00010101]; // bits: 1(flag), 0101(=5 in 4-bit LE), 000
        let mut reader = BitReader::new(&data);
        assert_eq!(reader.read_variable_length(4).ok(), Some(0b1010));
    }
}
