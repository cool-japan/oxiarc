use crate::SzipError;

/// A bit-level reader that supports both MSB-first and LSB-first ordering.
///
/// MSB-first (most significant bit first): the first bit read from byte `b`
/// is `(b >> 7) & 1`.
///
/// LSB-first (least significant bit first): the first bit read from byte `b`
/// is `b & 1`.
pub struct BitReader<'a> {
    data: &'a [u8],
    /// Index of the current byte being read.
    byte_pos: usize,
    /// Bit position within the current byte.
    ///
    /// - MSB mode: `bit_pos` counts up from 0 (MSB) to 7 (LSB), so the bit
    ///   extracted is `(byte >> (7 - bit_pos)) & 1`.
    /// - LSB mode: `bit_pos` counts up from 0 (LSB), so the bit extracted is
    ///   `(byte >> bit_pos) & 1`.
    bit_pos: u8,
    msb: bool,
    /// Total number of bits consumed so far.
    bits_consumed: usize,
}

impl<'a> BitReader<'a> {
    /// Create a new `BitReader` over `data`.
    ///
    /// `msb = true` → bits are read MSB-first (most common for AEC).
    /// `msb = false` → bits are read LSB-first.
    pub fn new(data: &'a [u8], msb: bool) -> Self {
        Self {
            data,
            byte_pos: 0,
            bit_pos: 0,
            msb,
            bits_consumed: 0,
        }
    }

    /// Read a single bit, returning `0` or `1`.
    fn read_bit(&mut self) -> Result<u32, SzipError> {
        if self.byte_pos >= self.data.len() {
            return Err(SzipError::UnexpectedEof {
                offset: self.bits_consumed,
            });
        }
        let byte = self.data[self.byte_pos];
        let bit = if self.msb {
            (byte >> (7 - self.bit_pos)) & 1
        } else {
            (byte >> self.bit_pos) & 1
        };
        self.bit_pos += 1;
        if self.bit_pos == 8 {
            self.bit_pos = 0;
            self.byte_pos += 1;
        }
        self.bits_consumed += 1;
        Ok(u32::from(bit))
    }

    /// Read `n` bits (1..=32) as a `u32` (MSB of the result is the first bit
    /// read).
    pub fn read_bits(&mut self, n: u8) -> Result<u32, SzipError> {
        debug_assert!((1..=32).contains(&n), "n={n} out of range 1..=32");
        let mut value: u32 = 0;
        for _ in 0..n {
            let bit = self.read_bit()?;
            value = (value << 1) | bit;
        }
        Ok(value)
    }

    /// Total number of bits consumed since creation.
    pub fn bits_consumed(&self) -> usize {
        self.bits_consumed
    }

    /// Discard any remaining bits in the current byte so the reader is aligned
    /// to the next byte boundary.
    pub fn align_to_byte(&mut self) {
        if self.bit_pos != 0 {
            let consumed = 8 - self.bit_pos;
            self.bits_consumed += consumed as usize;
            self.bit_pos = 0;
            self.byte_pos += 1;
        }
    }
}

/// A bit-level writer that supports both MSB-first and LSB-first ordering.
pub struct BitWriter {
    data: Vec<u8>,
    current_byte: u8,
    bit_pos: u8,
    msb: bool,
}

impl BitWriter {
    /// Create a new empty `BitWriter`.
    pub fn new(msb: bool) -> Self {
        Self {
            data: Vec::new(),
            current_byte: 0,
            bit_pos: 0,
            msb,
        }
    }

    /// Write a single bit (0 or 1).
    pub fn write_bit(&mut self, bit: u32) {
        let b = (bit & 1) as u8;
        if self.msb {
            self.current_byte |= b << (7 - self.bit_pos);
        } else {
            self.current_byte |= b << self.bit_pos;
        }
        self.bit_pos += 1;
        if self.bit_pos == 8 {
            self.data.push(self.current_byte);
            self.current_byte = 0;
            self.bit_pos = 0;
        }
    }

    /// Write `n` bits (1..=32) from `value` (MSB of value → first bit written).
    pub fn write_bits(&mut self, value: u32, n: u8) {
        debug_assert!((1..=32).contains(&n), "n={n} out of range 1..=32");
        for i in (0..n).rev() {
            let bit = (value >> i) & 1;
            self.write_bit(bit);
        }
    }

    /// Pad the current byte with zeros and flush it so the writer is aligned
    /// to the next byte boundary. Has no effect if already aligned.
    ///
    /// This is used when `SzipParams::rsi_byte_align` is true to insert
    /// inter-RSI padding that matches the alignment the decoder skips over.
    pub fn align_to_byte(&mut self) {
        if self.bit_pos != 0 {
            self.data.push(self.current_byte);
            self.current_byte = 0;
            self.bit_pos = 0;
        }
    }

    /// Flush any partial byte (padding with zeros) and return the byte buffer.
    pub fn finish(mut self) -> Vec<u8> {
        if self.bit_pos != 0 {
            self.data.push(self.current_byte);
        }
        self.data
    }
}
