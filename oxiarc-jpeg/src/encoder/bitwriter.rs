//! The entropy-coded bit sink.
//!
//! Two rules decide whether the last byte of every scan matches `cjpeg`'s:
//!
//! * a partial final byte is padded with **1**-bits, not zeros
//!   (`jchuff.c`'s `flush_bits` emits seven one-bits and then discards the
//!   buffer);
//! * every `0xFF` the entropy coder produces — the padding included — is
//!   followed by a stuffed `0x00`.
//!
//! Zero padding decodes fine everywhere and fails byte comparison on most
//! scans, which is exactly the kind of bug that survives a visual check.

/// Accumulates entropy-coded bits and byte-stuffs them into an output buffer.
#[derive(Debug, Default)]
pub(crate) struct BitWriter {
    out: Vec<u8>,
    /// Pending bits, most significant first in the low `count` positions.
    accumulator: u64,
    count: u32,
}

impl BitWriter {
    /// An empty writer.
    #[cfg(test)]
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// A writer with room for `capacity` bytes reserved.
    pub(crate) fn with_capacity(capacity: usize) -> Self {
        Self {
            out: Vec::with_capacity(capacity),
            accumulator: 0,
            count: 0,
        }
    }

    /// Append `size` bits of `code`, most significant first.
    pub(crate) fn emit_bits(&mut self, code: u32, size: u32) {
        if size == 0 {
            return;
        }
        let masked = u64::from(code) & ((1u64 << size) - 1);
        self.accumulator = (self.accumulator << size) | masked;
        self.count += size;
        while self.count >= 8 {
            self.count -= 8;
            let byte = ((self.accumulator >> self.count) & 0xFF) as u8;
            self.out.push(byte);
            if byte == 0xFF {
                self.out.push(0x00);
            }
        }
        self.accumulator &= (1u64 << self.count) - 1;
    }

    /// Append a Huffman code and its magnitude bits in one step.
    ///
    /// Both together are at most 32 bits, and fewer than 8 are pending, so
    /// the accumulator cannot overflow. Merging the two emissions halves the
    /// call count in the entropy coder's inner loop.
    #[inline]
    pub(crate) fn emit_code(&mut self, code: u32, code_size: u32, value: u32, value_size: u32) {
        let total = code_size + value_size;
        if total == 0 {
            return;
        }
        let masked_code = u64::from(code) & ((1u64 << code_size) - 1);
        let merged = if value_size == 0 {
            masked_code
        } else {
            (masked_code << value_size) | (u64::from(value) & ((1u64 << value_size) - 1))
        };
        self.accumulator = (self.accumulator << total) | merged;
        self.count += total;
        while self.count >= 8 {
            self.count -= 8;
            let byte = ((self.accumulator >> self.count) & 0xFF) as u8;
            self.out.push(byte);
            if byte == 0xFF {
                self.out.push(0x00);
            }
        }
        self.accumulator &= (1u64 << self.count) - 1;
    }

    /// Pad the partial byte with one-bits and drop the accumulator.
    pub(crate) fn flush_bits(&mut self) {
        self.emit_bits(0x7F, 7);
        self.accumulator = 0;
        self.count = 0;
    }

    /// Append a two-byte marker after flushing, used for `RSTn`.
    pub(crate) fn emit_restart(&mut self, index: u8) {
        self.flush_bits();
        self.out.push(0xFF);
        self.out.push(0xD0 | (index & 7));
    }

    /// The bytes written, after a final flush.
    pub(crate) fn finish(mut self) -> Vec<u8> {
        self.flush_bits();
        self.out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bits_are_packed_most_significant_first() {
        let mut writer = BitWriter::new();
        writer.emit_bits(0b101, 3);
        writer.emit_bits(0b01010, 5);
        assert_eq!(writer.finish(), vec![0b1010_1010]);
    }

    #[test]
    fn a_partial_byte_is_padded_with_ones() {
        let mut writer = BitWriter::new();
        writer.emit_bits(0, 1);
        // 0 followed by seven one-bits.
        assert_eq!(writer.finish(), vec![0b0111_1111]);
    }

    #[test]
    fn nothing_pending_flushes_to_nothing() {
        let writer = BitWriter::new();
        assert!(writer.finish().is_empty());
        let mut writer = BitWriter::new();
        writer.emit_bits(0xAB, 8);
        assert_eq!(writer.finish(), vec![0xAB]);
    }

    #[test]
    fn ff_bytes_are_stuffed_including_padding() {
        let mut writer = BitWriter::new();
        writer.emit_bits(0xFF, 8);
        assert_eq!(writer.finish(), vec![0xFF, 0x00]);

        // A single one-bit followed by seven bits of padding makes 0xFF,
        // which must itself be stuffed.
        let mut writer = BitWriter::new();
        writer.emit_bits(1, 1);
        assert_eq!(writer.finish(), vec![0xFF, 0x00]);
    }

    #[test]
    fn restart_markers_flush_first_and_cycle() {
        let mut writer = BitWriter::new();
        writer.emit_bits(0, 1);
        writer.emit_restart(0);
        writer.emit_bits(0b11, 2);
        writer.emit_restart(7);
        assert_eq!(
            writer.finish(),
            vec![0b0111_1111, 0xFF, 0xD0, 0b1111_1111, 0x00, 0xFF, 0xD7]
        );
    }

    /// The merged emission must produce exactly the bits two separate calls
    /// would.
    #[test]
    fn merged_emission_matches_two_separate_ones() {
        for &(code, code_size, value, value_size) in &[
            (0b1010u32, 4u32, 0b011u32, 3u32),
            (0xFFFF, 16, 0xFFFF, 16),
            (0, 1, 0, 0),
            (0b110, 3, 0, 0),
            (0x7F, 7, 0x1, 1),
        ] {
            let mut a = BitWriter::new();
            a.emit_bits(code, code_size);
            a.emit_bits(value, value_size);
            let mut b = BitWriter::new();
            b.emit_code(code, code_size, value, value_size);
            assert_eq!(a.finish(), b.finish(), "{code:#x}/{code_size}");
        }
    }

    #[test]
    fn long_codes_cross_byte_boundaries() {
        let mut writer = BitWriter::new();
        writer.emit_bits(0xABCD, 16);
        writer.emit_bits(0x1234, 16);
        assert_eq!(writer.finish(), vec![0xAB, 0xCD, 0x12, 0x34]);
    }

    #[test]
    fn zero_sized_emissions_are_ignored() {
        let mut writer = BitWriter::new();
        writer.emit_bits(0xFF, 0);
        assert!(writer.finish().is_empty());
    }
}
