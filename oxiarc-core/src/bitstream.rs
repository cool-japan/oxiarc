//! Bit-level I/O operations for compression algorithms.
//!
//! This module provides `BitReader` and `BitWriter` for reading and writing
//! data at the bit level, which is essential for variable-length codes used
//! in Huffman coding and other compression algorithms.
//!
//! # Bit Ordering
//!
//! Both DEFLATE and LZH use LSB-first (Least Significant Bit first) ordering
//! within bytes. This means bits are packed starting from the least significant
//! bit of each byte.
//!
//! # Example
//!
//! ```
//! use oxiarc_core::bitstream::{BitReader, BitWriter};
//! use std::io::Cursor;
//!
//! // Writing bits
//! let mut output = Vec::new();
//! {
//!     let mut writer = BitWriter::new(&mut output);
//!     writer.write_bits(0b101, 3).unwrap();  // Write 3 bits
//!     writer.write_bits(0b1100, 4).unwrap(); // Write 4 bits
//!     writer.flush().unwrap();
//! }
//!
//! // Reading bits
//! let mut reader = BitReader::new(Cursor::new(&output));
//! assert_eq!(reader.read_bits(3).unwrap(), 0b101);
//! assert_eq!(reader.read_bits(4).unwrap(), 0b1100);
//! ```

use crate::error::{OxiArcError, Result};
use std::io::{Read, Write};

/// A bit-level reader that wraps any `Read` implementation.
///
/// `BitReader` maintains an internal buffer to efficiently read bits across
/// byte boundaries. It uses a 64-bit buffer for speculative reading to
/// minimize I/O operations.
#[derive(Debug)]
pub struct BitReader<R: Read> {
    /// Underlying reader.
    reader: R,
    /// Bit buffer (LSB-first).
    buffer: u64,
    /// Number of valid bits in buffer.
    bits_in_buffer: u8,
    /// Total bits read (for error reporting).
    total_bits_read: u64,
}

impl<R: Read> BitReader<R> {
    /// Create a new `BitReader` wrapping the given reader.
    pub fn new(reader: R) -> Self {
        Self {
            reader,
            buffer: 0,
            bits_in_buffer: 0,
            total_bits_read: 0,
        }
    }

    /// Get a reference to the underlying reader.
    pub fn get_ref(&self) -> &R {
        &self.reader
    }

    /// Get a mutable reference to the underlying reader.
    pub fn get_mut(&mut self) -> &mut R {
        &mut self.reader
    }

    /// Consume this `BitReader` and return the underlying reader.
    pub fn into_inner(self) -> R {
        self.reader
    }

    /// Get the total number of bits read so far.
    pub fn bits_read(&self) -> u64 {
        self.total_bits_read
    }

    /// Get the current bit position (for error reporting).
    pub fn bit_position(&self) -> u64 {
        self.total_bits_read
    }

    /// Ensure at least `count` bits are available in the buffer.
    /// Optimized to read multiple bytes at once when possible.
    #[inline]
    fn fill_buffer(&mut self, count: u8) -> Result<()> {
        debug_assert!(count <= 57, "Cannot fill more than 57 bits at once");

        if self.bits_in_buffer >= count {
            return Ok(());
        }

        // Calculate how many bytes we need
        let bits_needed = count - self.bits_in_buffer;
        let bytes_needed = bits_needed.div_ceil(8).min(7) as usize; // Max 7 to stay under 64 bits

        // Try to read multiple bytes at once for better performance
        let mut temp_buf = [0u8; 8];
        match self.reader.read(&mut temp_buf[..bytes_needed]) {
            Ok(0) => {
                return Err(OxiArcError::unexpected_eof(bytes_needed));
            }
            Ok(n) => {
                // Pack bytes into buffer (LSB-first)
                for byte in temp_buf.iter().take(n) {
                    self.buffer |= (*byte as u64) << self.bits_in_buffer;
                    self.bits_in_buffer += 8;
                }
            }
            Err(e) => return Err(e.into()),
        }

        // Check if we got enough bits
        if self.bits_in_buffer < count {
            return Err(OxiArcError::unexpected_eof(1));
        }

        Ok(())
    }

    /// Read up to 32 bits from the stream.
    ///
    /// # Arguments
    ///
    /// * `count` - Number of bits to read (0-32)
    ///
    /// # Returns
    ///
    /// The bits read as a u32, with the first bit read in the LSB position.
    #[inline]
    pub fn read_bits(&mut self, count: u8) -> Result<u32> {
        debug_assert!(count <= 32, "Cannot read more than 32 bits at once");

        if count == 0 {
            return Ok(0);
        }

        self.fill_buffer(count)?;

        // Extract bits from buffer (optimized with wrapping_sub to avoid overflow check)
        let mask = (1u64 << count).wrapping_sub(1);
        let result = (self.buffer & mask) as u32;

        // Remove read bits from buffer
        self.buffer >>= count;
        self.bits_in_buffer -= count;
        self.total_bits_read += count as u64;

        Ok(result)
    }

    /// Peek at up to 32 bits without consuming them.
    ///
    /// # Arguments
    ///
    /// * `count` - Number of bits to peek (0-32)
    ///
    /// # Returns
    ///
    /// The bits as a u32, without advancing the read position.
    #[inline]
    pub fn peek_bits(&mut self, count: u8) -> Result<u32> {
        debug_assert!(count <= 32, "Cannot peek more than 32 bits at once");

        if count == 0 {
            return Ok(0);
        }

        self.fill_buffer(count)?;

        let mask = (1u64 << count) - 1;
        Ok((self.buffer & mask) as u32)
    }

    /// Skip a number of bits.
    ///
    /// # Arguments
    ///
    /// * `count` - Number of bits to skip
    pub fn skip_bits(&mut self, count: u8) -> Result<()> {
        if count == 0 {
            return Ok(());
        }

        self.fill_buffer(count)?;

        self.buffer >>= count;
        self.bits_in_buffer -= count;
        self.total_bits_read += count as u64;

        Ok(())
    }

    /// Read a single bit.
    pub fn read_bit(&mut self) -> Result<bool> {
        Ok(self.read_bits(1)? != 0)
    }

    /// Read an aligned byte, discarding any partial bits.
    ///
    /// If there are partial bits in the buffer, they are discarded to
    /// align to the next byte boundary.
    pub fn read_byte_aligned(&mut self) -> Result<u8> {
        // Discard partial bits
        let remainder = self.bits_in_buffer % 8;
        if remainder > 0 {
            self.skip_bits(remainder)?;
        }

        // Read from buffer if we have a full byte
        if self.bits_in_buffer >= 8 {
            let byte = (self.buffer & 0xFF) as u8;
            self.buffer >>= 8;
            self.bits_in_buffer -= 8;
            self.total_bits_read += 8;
            Ok(byte)
        } else {
            // Buffer is empty, read directly
            let mut byte = [0u8; 1];
            self.reader.read_exact(&mut byte)?;
            self.total_bits_read += 8;
            Ok(byte[0])
        }
    }

    /// Align to the next byte boundary by discarding partial bits.
    pub fn align_to_byte(&mut self) {
        let remainder = self.bits_in_buffer % 8;
        if remainder > 0 {
            self.buffer >>= remainder;
            self.bits_in_buffer -= remainder;
            self.total_bits_read += remainder as u64;
        }
    }

    /// Check if the reader is at end of stream.
    ///
    /// Note: This only checks if the buffer is empty and attempts one read.
    pub fn is_eof(&mut self) -> bool {
        if self.bits_in_buffer > 0 {
            return false;
        }

        let mut byte = [0u8; 1];
        match self.reader.read(&mut byte) {
            Ok(0) => true,
            Ok(_) => {
                self.buffer = byte[0] as u64;
                self.bits_in_buffer = 8;
                false
            }
            Err(_) => true,
        }
    }

    /// Read bytes directly, skipping the bit buffer.
    ///
    /// The bit buffer must be byte-aligned before calling this method.
    pub fn read_bytes(&mut self, buf: &mut [u8]) -> Result<()> {
        // First, drain any complete bytes from the bit buffer
        let mut offset = 0;
        while self.bits_in_buffer >= 8 && offset < buf.len() {
            buf[offset] = (self.buffer & 0xFF) as u8;
            self.buffer >>= 8;
            self.bits_in_buffer -= 8;
            self.total_bits_read += 8;
            offset += 1;
        }

        // Read remaining bytes directly
        if offset < buf.len() {
            self.reader.read_exact(&mut buf[offset..])?;
            self.total_bits_read += (buf.len() - offset) as u64 * 8;
        }

        Ok(())
    }
}

/// A bit-level writer that wraps any `Write` implementation.
///
/// `BitWriter` accumulates bits in an internal buffer and flushes complete
/// bytes to the underlying writer. Call `flush()` when done to write any
/// remaining partial byte.
#[derive(Debug)]
pub struct BitWriter<W: Write> {
    /// Underlying writer.
    writer: W,
    /// Bit buffer (LSB-first).
    buffer: u64,
    /// Number of bits in buffer.
    bits_in_buffer: u8,
    /// Total bits written.
    total_bits_written: u64,
}

impl<W: Write> BitWriter<W> {
    /// Create a new `BitWriter` wrapping the given writer.
    pub fn new(writer: W) -> Self {
        Self {
            writer,
            buffer: 0,
            bits_in_buffer: 0,
            total_bits_written: 0,
        }
    }

    /// Get a reference to the underlying writer.
    pub fn get_ref(&self) -> &W {
        &self.writer
    }

    /// Get a mutable reference to the underlying writer.
    pub fn get_mut(&mut self) -> &mut W {
        &mut self.writer
    }

    /// Consume this `BitWriter` and return the underlying writer.
    ///
    /// This flushes any remaining bits before returning the writer.
    pub fn into_inner(mut self) -> Result<W> {
        self.flush()?;
        // Use ManuallyDrop to prevent Drop from running (we already flushed)
        let this = std::mem::ManuallyDrop::new(self);
        // SAFETY: We're consuming self and preventing drop, so it's safe to take the writer
        Ok(unsafe { std::ptr::read(&this.writer) })
    }

    /// Get the total number of bits written so far.
    pub fn bits_written(&self) -> u64 {
        self.total_bits_written
    }

    /// Flush complete bytes from the buffer to the writer.
    /// Optimized to write multiple bytes at once.
    #[inline]
    fn flush_bytes(&mut self) -> Result<()> {
        // Optimize: write multiple bytes at once when possible
        if self.bits_in_buffer >= 32 {
            let bytes = [
                (self.buffer & 0xFF) as u8,
                ((self.buffer >> 8) & 0xFF) as u8,
                ((self.buffer >> 16) & 0xFF) as u8,
                ((self.buffer >> 24) & 0xFF) as u8,
            ];
            self.writer.write_all(&bytes)?;
            self.buffer >>= 32;
            self.bits_in_buffer -= 32;
        }

        // Write remaining complete bytes one at a time
        while self.bits_in_buffer >= 8 {
            let byte = (self.buffer & 0xFF) as u8;
            self.writer.write_all(&[byte])?;
            self.buffer >>= 8;
            self.bits_in_buffer -= 8;
        }
        Ok(())
    }

    /// Write up to 32 bits to the stream.
    ///
    /// # Arguments
    ///
    /// * `value` - The bits to write (LSB-first)
    /// * `count` - Number of bits to write (0-32)
    #[inline]
    pub fn write_bits(&mut self, value: u32, count: u8) -> Result<()> {
        debug_assert!(count <= 32, "Cannot write more than 32 bits at once");

        if count == 0 {
            return Ok(());
        }

        // Mask off any extra bits (optimized with wrapping_sub)
        let mask = if count == 32 {
            u32::MAX
        } else {
            (1u32 << count).wrapping_sub(1)
        };
        let value = value & mask;

        // Add bits to buffer
        self.buffer |= (value as u64) << self.bits_in_buffer;
        self.bits_in_buffer += count;
        self.total_bits_written += count as u64;

        // Flush complete bytes
        self.flush_bytes()?;

        Ok(())
    }

    /// Write a single bit.
    #[inline(always)]
    pub fn write_bit(&mut self, bit: bool) -> Result<()> {
        // Inline the critical path for single bit writes
        self.buffer |= (bit as u64) << self.bits_in_buffer;
        self.bits_in_buffer += 1;
        self.total_bits_written += 1;

        if self.bits_in_buffer >= 8 {
            self.flush_bytes()?;
        }

        Ok(())
    }

    /// Write an aligned byte.
    ///
    /// If there are partial bits in the buffer, they are padded with zeros
    /// to complete the byte before writing the new byte.
    pub fn write_byte_aligned(&mut self, byte: u8) -> Result<()> {
        // Pad to byte boundary
        if self.bits_in_buffer % 8 != 0 {
            let padding = 8 - (self.bits_in_buffer % 8);
            self.write_bits(0, padding)?;
        }

        // Write the byte
        self.write_bits(byte as u32, 8)
    }

    /// Pad to byte boundary with zeros and flush.
    pub fn align_to_byte(&mut self) -> Result<()> {
        if self.bits_in_buffer % 8 != 0 {
            let padding = 8 - (self.bits_in_buffer % 8);
            self.write_bits(0, padding)?;
        }
        Ok(())
    }

    /// Flush any remaining bits to the underlying writer.
    ///
    /// If there are partial bits, they are padded with zeros to complete
    /// the final byte.
    pub fn flush(&mut self) -> Result<()> {
        // Pad to byte boundary
        self.align_to_byte()?;

        // Flush any remaining complete bytes
        self.flush_bytes()?;

        // Flush underlying writer
        self.writer.flush()?;

        Ok(())
    }

    /// Write bytes directly to the stream.
    ///
    /// The bit buffer should be byte-aligned before calling this method.
    pub fn write_bytes(&mut self, buf: &[u8]) -> Result<()> {
        // Flush current buffer first
        self.flush_bytes()?;

        // If we have partial bits, we need to merge them
        if self.bits_in_buffer > 0 {
            for &byte in buf {
                self.write_bits(byte as u32, 8)?;
            }
        } else {
            // Direct write
            self.writer.write_all(buf)?;
            self.total_bits_written += buf.len() as u64 * 8;
        }

        Ok(())
    }
}

impl<W: Write> Drop for BitWriter<W> {
    fn drop(&mut self) {
        // Best-effort flush on drop
        let _ = self.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn test_bitreader_basic() {
        // 0b10110101 = 0xB5
        let data = vec![0xB5];
        let mut reader = BitReader::new(Cursor::new(data));

        assert_eq!(reader.read_bits(1).unwrap(), 1); // LSB first
        assert_eq!(reader.read_bits(1).unwrap(), 0);
        assert_eq!(reader.read_bits(1).unwrap(), 1);
        assert_eq!(reader.read_bits(1).unwrap(), 0);
        assert_eq!(reader.read_bits(1).unwrap(), 1);
        assert_eq!(reader.read_bits(1).unwrap(), 1);
        assert_eq!(reader.read_bits(1).unwrap(), 0);
        assert_eq!(reader.read_bits(1).unwrap(), 1);
    }

    #[test]
    fn test_bitreader_multi_byte() {
        let data = vec![0xFF, 0x00];
        let mut reader = BitReader::new(Cursor::new(data));

        assert_eq!(reader.read_bits(4).unwrap(), 0xF);
        assert_eq!(reader.read_bits(8).unwrap(), 0x0F); // Crosses byte boundary
        assert_eq!(reader.read_bits(4).unwrap(), 0x0);
    }

    #[test]
    fn test_bitreader_peek() {
        let data = vec![0xAB];
        let mut reader = BitReader::new(Cursor::new(data));

        assert_eq!(reader.peek_bits(4).unwrap(), 0xB);
        assert_eq!(reader.peek_bits(4).unwrap(), 0xB); // Same value
        assert_eq!(reader.read_bits(4).unwrap(), 0xB); // Now consume
        assert_eq!(reader.peek_bits(4).unwrap(), 0xA);
    }

    #[test]
    fn test_bitwriter_basic() {
        let mut output = Vec::new();
        {
            let mut writer = BitWriter::new(&mut output);
            // Write 0b10110101 bit by bit
            writer.write_bit(true).unwrap(); // 1
            writer.write_bit(false).unwrap(); // 0
            writer.write_bit(true).unwrap(); // 1
            writer.write_bit(false).unwrap(); // 0
            writer.write_bit(true).unwrap(); // 1
            writer.write_bit(true).unwrap(); // 1
            writer.write_bit(false).unwrap(); // 0
            writer.write_bit(true).unwrap(); // 1
            writer.flush().unwrap();
        }
        assert_eq!(output, vec![0xB5]);
    }

    #[test]
    fn test_bitwriter_multi_bits() {
        let mut output = Vec::new();
        {
            let mut writer = BitWriter::new(&mut output);
            writer.write_bits(0b101, 3).unwrap();
            writer.write_bits(0b11001, 5).unwrap();
            writer.flush().unwrap();
        }
        // 3 bits: 101, 5 bits: 11001 -> 11001_101 = 0xCD
        assert_eq!(output, vec![0xCD]);
    }

    #[test]
    fn test_roundtrip() {
        let mut output = Vec::new();
        {
            let mut writer = BitWriter::new(&mut output);
            writer.write_bits(0b101, 3).unwrap();
            writer.write_bits(0b1111, 4).unwrap();
            writer.write_bits(0b10, 2).unwrap();
            writer.write_bits(0b110011, 6).unwrap();
            writer.flush().unwrap();
        }

        let mut reader = BitReader::new(Cursor::new(&output));
        assert_eq!(reader.read_bits(3).unwrap(), 0b101);
        assert_eq!(reader.read_bits(4).unwrap(), 0b1111);
        assert_eq!(reader.read_bits(2).unwrap(), 0b10);
        assert_eq!(reader.read_bits(6).unwrap(), 0b110011);
    }

    #[test]
    fn test_align_to_byte() {
        let data = vec![0xFF, 0xAA];
        let mut reader = BitReader::new(Cursor::new(data));

        reader.read_bits(3).unwrap(); // Read 3 bits
        reader.align_to_byte(); // Skip remaining 5 bits
        assert_eq!(reader.read_bits(8).unwrap(), 0xAA);
    }

    #[test]
    fn test_read_bytes() {
        let data = vec![0x12, 0x34, 0x56, 0x78];
        let mut reader = BitReader::new(Cursor::new(data));

        let mut buf = [0u8; 2];
        reader.read_bytes(&mut buf).unwrap();
        assert_eq!(buf, [0x12, 0x34]);

        reader.read_bytes(&mut buf).unwrap();
        assert_eq!(buf, [0x56, 0x78]);
    }
}
