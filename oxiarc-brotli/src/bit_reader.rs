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
    /// Near the end of the stream, fewer than `n` bits may be available. In
    /// that case, the missing bits are treated as 0. This is safe for prefix
    /// code table lookup because [`BitReader::drop_bits`] refuses to consume
    /// bits that do not exist: a phantom (zero-padded) code longer than the
    /// remaining real bits produces an `UnexpectedEof` at drop time instead
    /// of silently decoding garbage.
    pub fn peek_bits(&mut self, n: u32) -> BrotliResult<u32> {
        if n == 0 {
            return Ok(0);
        }
        if n > 32 {
            return Err(BrotliError::InvalidParameter(format!(
                "cannot peek {n} bits at once (max 32)"
            )));
        }
        if self.bits_in_buf < n {
            self.fill_buffer();
        }
        let mask = if n == 32 { u32::MAX } else { (1u32 << n) - 1 };
        Ok((self.bit_buf as u32) & mask)
    }

    /// Drop `n` bits that were previously peeked.
    ///
    /// Returns `UnexpectedEof` when fewer than `n` real bits remain, so a
    /// zero-padded peek can never silently over-consume the stream.
    pub fn drop_bits(&mut self, n: u32) -> BrotliResult<()> {
        if n == 0 {
            return Ok(());
        }
        if self.bits_in_buf < n {
            self.fill_buffer();
            if self.bits_in_buf < n {
                return Err(BrotliError::UnexpectedEof);
            }
        }
        self.bit_buf >>= n;
        self.bits_in_buf -= n;
        self.fill_buffer();
        Ok(())
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
        self.fill_buffer();
        if self.bits_in_buf >= n {
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

    /// Discard bits up to the next byte boundary and return their value.
    ///
    /// Returns `Ok(0)` when already aligned. Brotli requires these fill bits
    /// to be zero in several places; the caller checks the returned value.
    pub fn align_to_byte(&mut self) -> BrotliResult<u32> {
        let misalign = (self.bits_consumed() % 8) as u32;
        if misalign == 0 {
            return Ok(0);
        }
        self.read_bits(8 - misalign)
    }

    /// Append exactly `n` bytes to `out`. The reader must be byte-aligned.
    ///
    /// Used for uncompressed meta-blocks and metadata skipping; takes the
    /// bulk path over the underlying slice where possible.
    pub fn read_bytes_aligned(&mut self, out: &mut Vec<u8>, n: usize) -> BrotliResult<()> {
        if self.bits_consumed() % 8 != 0 {
            return Err(BrotliError::CorruptedData(
                "internal error: unaligned byte read".to_string(),
            ));
        }
        let mut remaining = n;
        // Drain whole bytes buffered in `bit_buf` first.
        while remaining > 0 && self.bits_in_buf >= 8 {
            out.push((self.bit_buf & 0xFF) as u8);
            self.bit_buf >>= 8;
            self.bits_in_buf -= 8;
            remaining -= 1;
        }
        // Bulk-copy the rest straight from the input slice.
        if remaining > 0 {
            let available = self.data.len() - self.byte_pos;
            if available < remaining {
                return Err(BrotliError::UnexpectedEof);
            }
            out.extend_from_slice(&self.data[self.byte_pos..self.byte_pos + remaining]);
            self.byte_pos += remaining;
        }
        self.fill_buffer();
        Ok(())
    }

    /// Skip exactly `n` bytes. The reader must be byte-aligned.
    pub fn skip_bytes_aligned(&mut self, n: usize) -> BrotliResult<()> {
        if self.bits_consumed() % 8 != 0 {
            return Err(BrotliError::CorruptedData(
                "internal error: unaligned byte skip".to_string(),
            ));
        }
        let mut remaining = n;
        while remaining > 0 && self.bits_in_buf >= 8 {
            self.bit_buf >>= 8;
            self.bits_in_buf -= 8;
            remaining -= 1;
        }
        if remaining > 0 {
            let available = self.data.len() - self.byte_pos;
            if available < remaining {
                return Err(BrotliError::UnexpectedEof);
            }
            self.byte_pos += remaining;
        }
        self.fill_buffer();
        Ok(())
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
        assert!(reader.drop_bits(4).is_ok());
        assert_eq!(reader.read_bits(4).ok(), Some(0x0A));
    }

    #[test]
    fn test_peek_zero_pads_but_drop_checks() {
        let data = [0x01];
        let mut reader = BitReader::new(&data);
        // Peeking 16 bits with only 8 available zero-pads.
        assert_eq!(reader.peek_bits(16).ok(), Some(0x0001));
        // Dropping more than available must fail.
        assert!(reader.drop_bits(9).is_err());
        // Dropping what exists is fine.
        assert!(reader.drop_bits(8).is_ok());
        assert!(reader.drop_bits(1).is_err());
    }

    #[test]
    fn test_unexpected_eof() {
        let data = [0xFF];
        let mut reader = BitReader::new(&data);
        let _ = reader.read_bits(8);
        assert!(reader.read_bits(1).is_err());
    }

    #[test]
    fn test_align_to_byte() {
        let data = [0b0000_0101, 0xAB];
        let mut reader = BitReader::new(&data);
        assert_eq!(reader.read_bits(3).ok(), Some(0b101));
        // Remaining 5 bits of byte 0 are zero.
        assert_eq!(reader.align_to_byte().ok(), Some(0));
        assert_eq!(reader.read_bits(8).ok(), Some(0xAB));
        // Already aligned: no-op.
        assert_eq!(reader.align_to_byte().ok(), Some(0));
    }

    #[test]
    fn test_read_bytes_aligned() {
        let data = [0x11, 0x22, 0x33, 0x44];
        let mut reader = BitReader::new(&data);
        assert_eq!(reader.read_bits(8).ok(), Some(0x11));
        let mut out = Vec::new();
        assert!(reader.read_bytes_aligned(&mut out, 3).is_ok());
        assert_eq!(out, vec![0x22, 0x33, 0x44]);
        assert!(!reader.has_more());
        // Reading past the end errors.
        let mut reader = BitReader::new(&data);
        let mut out = Vec::new();
        assert!(reader.read_bytes_aligned(&mut out, 5).is_err());
    }

    #[test]
    fn test_skip_bytes_aligned() {
        let data = [0x11, 0x22, 0x33];
        let mut reader = BitReader::new(&data);
        assert!(reader.skip_bytes_aligned(2).is_ok());
        assert_eq!(reader.read_bits(8).ok(), Some(0x33));
        assert!(reader.skip_bytes_aligned(1).is_err());
    }

    #[test]
    fn test_bits_consumed() {
        let data = [0xFF, 0xFF, 0xFF];
        let mut reader = BitReader::new(&data);
        assert_eq!(reader.bits_consumed(), 0);
        let _ = reader.read_bits(5);
        assert_eq!(reader.bits_consumed(), 5);
        let _ = reader.peek_bits(10);
        assert_eq!(reader.bits_consumed(), 5);
        assert!(reader.drop_bits(10).is_ok());
        assert_eq!(reader.bits_consumed(), 15);
    }

    #[test]
    fn test_variable_length() {
        // First bit 0 => return 0
        let data = [0x00];
        let mut reader = BitReader::new(&data);
        assert_eq!(reader.read_variable_length(4).ok(), Some(0));

        let data = [0b00010101]; // bits: 1(flag), 0101(=5 in 4-bit LE), 000
        let mut reader = BitReader::new(&data);
        assert_eq!(reader.read_variable_length(4).ok(), Some(0b1010));
    }
}
