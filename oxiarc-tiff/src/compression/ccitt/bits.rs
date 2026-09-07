//! MSB-first bit reader and writer for the CCITT codes.
//!
//! T.4 and T.6 pack their code words most-significant-bit first inside each
//! byte, independently of the file's byte order. `FillOrder = 2` is applied by
//! the CCITT codecs themselves (libtiff's `Fax3SetupState` sets
//! `TIFF_NOBITREV` so the chunk-level reversal is skipped), which is why the
//! reader takes a `reversed` flag rather than a pre-reversed buffer: reversing
//! on the fly costs one table lookup per byte and no allocation.

/// Bit-reversal of every byte value, used for `FillOrder = 2`.
const REVERSE: [u8; 256] = {
    let mut table = [0u8; 256];
    let mut value = 0usize;
    while value < 256 {
        let mut reversed = 0u8;
        let mut bit = 0;
        while bit < 8 {
            if value & (1 << bit) != 0 {
                reversed |= 1 << (7 - bit);
            }
            bit += 1;
        }
        table[value] = reversed;
        value += 1;
    }
    table
};

/// An MSB-first bit reader over one chunk.
#[derive(Debug)]
pub(super) struct BitReader<'a> {
    data: &'a [u8],
    /// Bits consumed so far.
    pos: usize,
    /// Whether every byte is bit-reversed on the way in (`FillOrder = 2`).
    reversed: bool,
}

impl<'a> BitReader<'a> {
    /// A reader over `data`.
    pub(super) fn new(data: &'a [u8], reversed: bool) -> Self {
        Self {
            data,
            pos: 0,
            reversed,
        }
    }

    /// Total bits in the buffer.
    pub(super) fn len(&self) -> usize {
        self.data.len().saturating_mul(8)
    }

    /// Bits still unread.
    pub(super) fn remaining(&self) -> usize {
        self.len().saturating_sub(self.pos)
    }

    /// Bit offset of the next bit.
    pub(super) fn position(&self) -> usize {
        self.pos
    }

    /// Rewinds or advances to an absolute bit offset.
    pub(super) fn seek(&mut self, position: usize) {
        self.pos = position.min(self.len());
    }

    /// One byte of the source, bit-reversed when `FillOrder` says so.
    fn byte(&self, index: usize) -> u8 {
        match self.data.get(index) {
            Some(byte) if self.reversed => REVERSE[*byte as usize],
            Some(byte) => *byte,
            None => 0,
        }
    }

    /// The next `count` bits (at most 16), zero-padded past the end.
    ///
    /// Padding with zeros is deliberate: a code table lookup on a truncated
    /// stream then lands on the "invalid" entry rather than reading whatever
    /// followed the chunk in memory.
    pub(super) fn peek(&self, count: u8) -> u16 {
        debug_assert!(count <= 16);
        if count == 0 {
            return 0;
        }
        // Three bytes always cover sixteen bits at any bit offset, so one
        // shift-and-mask replaces the byte-at-a-time loop this used to run.
        // The fax decoder calls this once per code word — a few million times
        // for a page — so the loop showed up in profiles.
        let index = self.pos >> 3;
        let offset = (self.pos & 7) as u32;
        let window = (u32::from(self.byte(index)) << 16)
            | (u32::from(self.byte(index + 1)) << 8)
            | u32::from(self.byte(index + 2));
        // `offset <= 7` and `count <= 16`, so the shift is `1..=24`.
        let shift = 24 - offset - u32::from(count);
        let mask = (1u32 << count) - 1;
        ((window >> shift) & mask) as u16
    }

    /// Drops `count` bits.
    pub(super) fn skip(&mut self, count: usize) {
        self.pos = self.pos.saturating_add(count).min(self.len());
    }

    /// Reads one bit; past the end it reads as 0.
    pub(super) fn read_bit(&mut self) -> u8 {
        let bit = self.peek(1) as u8;
        self.skip(1);
        bit
    }

    /// Advances to the next byte boundary.
    pub(super) fn align_to_byte(&mut self) {
        let overshoot = self.pos % 8;
        if overshoot != 0 {
            self.skip(8 - overshoot);
        }
    }

    /// Advances to the next 16-bit boundary (`CCITTRLEW`, compression 32771).
    pub(super) fn align_to_word(&mut self) {
        let overshoot = self.pos % 16;
        if overshoot != 0 {
            self.skip(16 - overshoot);
        }
    }

    /// `true` once every remaining bit is zero (fill bits at the end of a
    /// chunk, which are not an error).
    pub(super) fn rest_is_padding(&self) -> bool {
        let mut probe = self.pos;
        while probe < self.len() {
            let index = probe / 8;
            let offset = probe % 8;
            if self.byte(index) & (0x80 >> offset) != 0 {
                return false;
            }
            probe += 1;
        }
        true
    }
}

/// An MSB-first bit writer.
#[derive(Debug, Default)]
pub(super) struct BitWriter {
    out: Vec<u8>,
    /// Bits already written into the last byte (0..8).
    used: u8,
}

impl BitWriter {
    /// An empty writer.
    pub(super) fn new() -> Self {
        Self::default()
    }

    /// Bits written so far.
    pub(super) fn bit_len(&self) -> usize {
        self.out
            .len()
            .saturating_mul(8)
            .saturating_sub(usize::from(if self.used == 0 { 0 } else { 8 - self.used }))
    }

    /// Writes the low `count` bits of `code`, most significant first.
    pub(super) fn write(&mut self, code: u16, count: u8) {
        for index in (0..count).rev() {
            let bit = (code >> index) & 1;
            if self.used == 0 {
                self.out.push(0);
            }
            if bit == 1 {
                if let Some(last) = self.out.last_mut() {
                    *last |= 0x80 >> self.used;
                }
            }
            self.used = (self.used + 1) % 8;
        }
    }

    /// Writes `count` zero bits.
    pub(super) fn write_zeros(&mut self, count: usize) {
        for _ in 0..count {
            self.write(0, 1);
        }
    }

    /// Pads with zero bits up to the next byte boundary.
    pub(super) fn align_to_byte(&mut self) {
        if self.used != 0 {
            let pad = 8 - self.used;
            self.write_zeros(usize::from(pad));
        }
    }

    /// Pads with zero bits up to the next 16-bit boundary.
    pub(super) fn align_to_word(&mut self) {
        self.align_to_byte();
        if self.out.len() % 2 != 0 {
            self.out.push(0);
        }
    }

    /// The finished buffer, byte-aligned, with every byte bit-reversed when
    /// `FillOrder` is 2.
    pub(super) fn finish(mut self, reversed: bool) -> Vec<u8> {
        self.align_to_byte();
        if reversed {
            for byte in &mut self.out {
                *byte = REVERSE[*byte as usize];
            }
        }
        self.out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_reverse_table_is_an_involution() {
        for value in 0..=255u8 {
            assert_eq!(REVERSE[REVERSE[value as usize] as usize], value);
        }
        assert_eq!(REVERSE[0b1000_0000], 0b0000_0001);
        assert_eq!(REVERSE[0b1010_0000], 0b0000_0101);
    }

    #[test]
    fn peek_reads_across_byte_boundaries() {
        let data = [0b1011_0010u8, 0b0100_1111];
        let reader = BitReader::new(&data, false);
        assert_eq!(reader.peek(4), 0b1011);
        assert_eq!(reader.peek(12), 0b1011_0010_0100);
        assert_eq!(reader.peek(16), 0b1011_0010_0100_1111);
    }

    #[test]
    fn peek_zero_pads_past_the_end() {
        let data = [0b1111_0000u8];
        let mut reader = BitReader::new(&data, false);
        reader.skip(4);
        assert_eq!(reader.peek(12), 0);
        assert_eq!(reader.remaining(), 4);
        reader.skip(100);
        assert_eq!(reader.remaining(), 0);
        assert_eq!(reader.peek(1), 0);
    }

    #[test]
    fn fill_order_two_is_applied_on_the_fly() {
        let plain = [0b1011_0010u8];
        let reversed = [0b0100_1101u8];
        let a = BitReader::new(&plain, false);
        let b = BitReader::new(&reversed, true);
        assert_eq!(a.peek(8), b.peek(8));
    }

    #[test]
    fn alignment_moves_to_the_next_boundary() {
        let data = [0u8; 4];
        let mut reader = BitReader::new(&data, false);
        reader.skip(3);
        reader.align_to_byte();
        assert_eq!(reader.position(), 8);
        reader.skip(1);
        reader.align_to_word();
        assert_eq!(reader.position(), 16);
        reader.align_to_word();
        assert_eq!(reader.position(), 16);
    }

    #[test]
    fn the_writer_round_trips_through_the_reader() {
        let mut writer = BitWriter::new();
        writer.write(0b1, 1);
        writer.write(0b0110, 4);
        writer.write(0b0000_0000_0001, 12);
        assert_eq!(writer.bit_len(), 17);
        let bytes = writer.finish(false);
        let mut reader = BitReader::new(&bytes, false);
        assert_eq!(reader.peek(1), 1);
        reader.skip(1);
        assert_eq!(reader.peek(4), 0b0110);
        reader.skip(4);
        assert_eq!(reader.peek(12), 1);
    }

    #[test]
    fn a_reversed_writer_matches_a_reversed_reader() {
        let mut writer = BitWriter::new();
        writer.write(0b1010_0110, 8);
        writer.write(0b111, 3);
        let bytes = writer.finish(true);
        let mut reader = BitReader::new(&bytes, true);
        assert_eq!(reader.peek(8), 0b1010_0110);
        reader.skip(8);
        assert_eq!(reader.peek(3), 0b111);
    }

    #[test]
    fn word_alignment_pads_the_buffer() {
        let mut writer = BitWriter::new();
        writer.write(0xff, 8);
        writer.align_to_word();
        assert_eq!(writer.finish(false).len(), 2);
    }

    #[test]
    fn trailing_zero_bits_count_as_padding() {
        let data = [0b1000_0000u8, 0, 0];
        let mut reader = BitReader::new(&data, false);
        assert!(!reader.rest_is_padding());
        reader.skip(1);
        assert!(reader.rest_is_padding());
    }
}
