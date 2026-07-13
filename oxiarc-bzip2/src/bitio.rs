//! MSB-first bit I/O for the bzip2 stream format.
//!
//! The bzip2 format is a big-endian *bit* stream: within every byte the most
//! significant bit comes first. This differs from the LSB-first (DEFLATE
//! style) `BitReader`/`BitWriter` in `oxiarc-core`, so the bzip2 codec uses
//! these dedicated MSB-first implementations. Using an LSB-first bit order
//! here would produce streams that are bit-reversed within each byte relative
//! to the specification (readable only by the same implementation).

use oxiarc_core::error::{OxiArcError, Result};
use std::io::{Read, Write};

/// Internal chunk size for buffered reads/writes.
const CHUNK_SIZE: usize = 4096;

/// An MSB-first bit reader over any [`Read`] implementation.
pub(crate) struct MsbBitReader<R: Read> {
    reader: R,
    /// Byte chunk buffer.
    chunk: [u8; CHUNK_SIZE],
    /// Valid bytes in `chunk`.
    chunk_len: usize,
    /// Next unread byte in `chunk`.
    chunk_pos: usize,
    /// Bit accumulator; the next bit to deliver is bit `bits - 1`.
    buffer: u64,
    /// Number of valid bits in `buffer`.
    bits: u32,
}

impl<R: Read> MsbBitReader<R> {
    /// Create a new MSB-first bit reader.
    pub(crate) fn new(reader: R) -> Self {
        Self {
            reader,
            chunk: [0u8; CHUNK_SIZE],
            chunk_len: 0,
            chunk_pos: 0,
            buffer: 0,
            bits: 0,
        }
    }

    /// Pull the next byte from the chunk buffer, refilling it if needed.
    /// Returns `Ok(None)` on a clean end of input.
    fn try_next_byte(&mut self) -> Result<Option<u8>> {
        if self.chunk_pos >= self.chunk_len {
            let n = self.reader.read(&mut self.chunk)?;
            if n == 0 {
                return Ok(None);
            }
            self.chunk_len = n;
            self.chunk_pos = 0;
        }
        let byte = self.chunk[self.chunk_pos];
        self.chunk_pos += 1;
        Ok(Some(byte))
    }

    /// Pull the next byte from the chunk buffer, refilling it if needed.
    fn next_byte(&mut self) -> Result<u8> {
        self.try_next_byte()?
            .ok_or_else(|| OxiArcError::unexpected_eof(1))
    }

    /// Ensure at least `count` bits are buffered.
    fn fill(&mut self, count: u32) -> Result<()> {
        debug_assert!(count <= 32);
        while self.bits < count {
            let byte = self.next_byte()?;
            self.buffer = (self.buffer << 8) | u64::from(byte);
            self.bits += 8;
        }
        Ok(())
    }

    /// Read `count` bits (0-32), MSB first.
    pub(crate) fn read_bits(&mut self, count: u32) -> Result<u32> {
        if count == 0 {
            return Ok(0);
        }
        self.fill(count)?;
        self.bits -= count;
        let value = (self.buffer >> self.bits) as u32;
        let mask = if count == 32 {
            u32::MAX
        } else {
            (1u32 << count) - 1
        };
        Ok(value & mask)
    }

    /// Read a single bit.
    pub(crate) fn read_bit(&mut self) -> Result<u32> {
        self.read_bits(1)
    }

    /// Read `count` bits (0-64) as a `u64`, MSB first.
    pub(crate) fn read_bits_u64(&mut self, count: u32) -> Result<u64> {
        debug_assert!(count <= 64);
        if count <= 32 {
            return Ok(u64::from(self.read_bits(count)?));
        }
        let high = u64::from(self.read_bits(count - 32)?);
        let low = u64::from(self.read_bits(32)?);
        Ok((high << 32) | low)
    }

    /// Discard buffered bits until the read position is byte aligned with
    /// the underlying stream.
    ///
    /// Bytes are pulled into the accumulator whole, so the sub-byte bit
    /// remainder (`bits % 8`) is exactly the padding left in the byte
    /// currently being consumed. Used at bzip2 stream boundaries: every
    /// stream is zero-padded to a whole byte, and a concatenated follow-up
    /// stream starts on the next byte boundary.
    pub(crate) fn align_to_byte(&mut self) {
        let pad = self.bits % 8;
        self.bits -= pad;
    }

    /// Read one whole byte, or `Ok(None)` on a clean end of input.
    ///
    /// The reader must be byte aligned (see [`MsbBitReader::align_to_byte`]).
    /// Unlike [`MsbBitReader::read_bits`], end of input here is not an
    /// error: it is how the decoder distinguishes "no more concatenated
    /// streams" from a truncated stream.
    pub(crate) fn try_read_aligned_byte(&mut self) -> Result<Option<u8>> {
        debug_assert_eq!(self.bits % 8, 0, "reader must be byte aligned");
        if self.bits >= 8 {
            self.bits -= 8;
            return Ok(Some((self.buffer >> self.bits) as u8));
        }
        self.try_next_byte()
    }
}

/// An MSB-first bit writer over any [`Write`] implementation.
pub(crate) struct MsbBitWriter<W: Write> {
    writer: W,
    /// Pending output bytes (flushed in chunks).
    pending: Vec<u8>,
    /// Bit accumulator; bits are appended at the low end.
    buffer: u64,
    /// Number of valid bits in `buffer` (always < 8 between calls).
    bits: u32,
}

impl<W: Write> MsbBitWriter<W> {
    /// Create a new MSB-first bit writer.
    pub(crate) fn new(writer: W) -> Self {
        Self {
            writer,
            pending: Vec::with_capacity(CHUNK_SIZE),
            buffer: 0,
            bits: 0,
        }
    }

    /// Write the low `count` bits (0-32) of `value`, MSB first.
    pub(crate) fn write_bits(&mut self, value: u32, count: u32) -> Result<()> {
        debug_assert!(count <= 32);
        if count == 0 {
            return Ok(());
        }
        let mask = if count == 32 {
            u32::MAX
        } else {
            (1u32 << count) - 1
        };
        self.buffer = (self.buffer << count) | u64::from(value & mask);
        self.bits += count;
        while self.bits >= 8 {
            self.bits -= 8;
            let byte = (self.buffer >> self.bits) as u8;
            self.pending.push(byte);
            if self.pending.len() >= CHUNK_SIZE {
                self.writer.write_all(&self.pending)?;
                self.pending.clear();
            }
        }
        Ok(())
    }

    /// Write a single bit.
    pub(crate) fn write_bit(&mut self, bit: u32) -> Result<()> {
        self.write_bits(bit & 1, 1)
    }

    /// Write the low `count` bits (0-64) of `value`, MSB first.
    pub(crate) fn write_bits_u64(&mut self, value: u64, count: u32) -> Result<()> {
        debug_assert!(count <= 64);
        if count > 32 {
            self.write_bits((value >> 32) as u32, count - 32)?;
            self.write_bits(value as u32, 32)
        } else {
            self.write_bits(value as u32, count)
        }
    }

    /// Write raw bytes. Only valid while the stream is still byte aligned
    /// (used for the fixed 4-byte stream header).
    pub(crate) fn write_bytes_aligned(&mut self, bytes: &[u8]) -> Result<()> {
        debug_assert_eq!(self.bits, 0, "stream must be byte aligned");
        self.pending.extend_from_slice(bytes);
        if self.pending.len() >= CHUNK_SIZE {
            self.writer.write_all(&self.pending)?;
            self.pending.clear();
        }
        Ok(())
    }

    /// Pad the final partial byte with zero bits and flush everything.
    pub(crate) fn finish(&mut self) -> Result<()> {
        if self.bits > 0 {
            let pad = 8 - self.bits;
            self.write_bits(0, pad)?;
        }
        if !self.pending.is_empty() {
            self.writer.write_all(&self.pending)?;
            self.pending.clear();
        }
        self.writer.flush()?;
        Ok(())
    }

    /// Consume the writer and return the wrapped [`Write`] value.
    ///
    /// The caller must invoke [`MsbBitWriter::finish`] first.
    pub(crate) fn into_inner(self) -> W {
        debug_assert_eq!(self.bits, 0, "finish() must be called before into_inner()");
        self.writer
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn msb_roundtrip_mixed_widths() {
        let mut writer = MsbBitWriter::new(Vec::new());
        writer.write_bits(0b101, 3).expect("write 3 bits");
        writer.write_bits(0xBEEF, 16).expect("write 16 bits");
        writer.write_bit(1).expect("write 1 bit");
        writer
            .write_bits_u64(0x3141_5926_5359, 48)
            .expect("write 48 bits");
        writer.finish().expect("finish");
        let bytes = writer.into_inner();

        let mut reader = MsbBitReader::new(Cursor::new(bytes));
        assert_eq!(reader.read_bits(3).expect("read 3 bits"), 0b101);
        assert_eq!(reader.read_bits(16).expect("read 16 bits"), 0xBEEF);
        assert_eq!(reader.read_bit().expect("read 1 bit"), 1);
        assert_eq!(
            reader.read_bits_u64(48).expect("read 48 bits"),
            0x3141_5926_5359
        );
    }

    #[test]
    fn msb_bit_order_is_big_endian_within_bytes() {
        // 0b1000_0000 followed by 0b0000_0001: the first bit read must be 1.
        let mut reader = MsbBitReader::new(Cursor::new(vec![0x80, 0x01]));
        assert_eq!(reader.read_bit().expect("first bit"), 1);
        assert_eq!(reader.read_bits(14).expect("middle bits"), 0);
        assert_eq!(reader.read_bit().expect("last bit"), 1);
    }

    #[test]
    fn msb_reader_eof_is_error() {
        let mut reader = MsbBitReader::new(Cursor::new(vec![0xFF]));
        assert_eq!(reader.read_bits(8).expect("read byte"), 0xFF);
        assert!(reader.read_bit().is_err());
    }

    #[test]
    fn align_then_aligned_byte_reads_and_probes_eof() {
        // 3 bits consumed, align discards the 5 pad bits, then the next two
        // whole bytes are readable and the end of input probes as None.
        let mut reader = MsbBitReader::new(Cursor::new(vec![0b1010_0000, 0xAB, 0xCD]));
        assert_eq!(reader.read_bits(3).expect("read 3 bits"), 0b101);
        reader.align_to_byte();
        assert_eq!(
            reader.try_read_aligned_byte().expect("read aligned byte"),
            Some(0xAB)
        );
        assert_eq!(
            reader.try_read_aligned_byte().expect("read aligned byte"),
            Some(0xCD)
        );
        assert_eq!(reader.try_read_aligned_byte().expect("probe EOF"), None);
    }

    #[test]
    fn align_on_already_aligned_reader_is_noop() {
        let mut reader = MsbBitReader::new(Cursor::new(vec![0x12, 0x34]));
        assert_eq!(reader.read_bits(8).expect("read byte"), 0x12);
        reader.align_to_byte();
        assert_eq!(reader.read_bits(8).expect("read byte"), 0x34);
    }
}
