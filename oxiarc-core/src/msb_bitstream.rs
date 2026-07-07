//! MSB-first bit-level I/O for canonical LZH / LHA compression.
//!
//! This module provides [`MsbBitReader`] and [`MsbBitWriter`], the
//! **most-significant-bit-first** counterparts of the LSB-first
//! [`BitReader`](crate::bitstream::BitReader) /
//! [`BitWriter`](crate::bitstream::BitWriter) in [`crate::bitstream`].
//!
//! # Why a separate module?
//!
//! DEFLATE packs bits **LSB-first** (least-significant bit of each byte first).
//! Canonical LZH/LHA — the classic `lha` / `LHarc` family, including the
//! `-lh5-` method this crate targets for spec compatibility — instead packs
//! bits **MSB-first**: the first bit of the stream is bit 7 (the most
//! significant bit) of the first byte, and a multi-bit code's most significant
//! bit is emitted first. Reading a byte-aligned `n`-byte field MSB-first is
//! therefore exactly a big-endian integer read.
//!
//! The two conventions are mutually incompatible at the bit level, so this
//! module exists as a sibling to [`crate::bitstream`] rather than replacing it.
//! The LSB-first types remain correct for DEFLATE (and for OxiArc's current,
//! not-yet-spec-compliant LZH format).
//!
//! # Correspondence to canonical LHA bit I/O
//!
//! The semantics mirror the reference `LHa for UNIX` primitives:
//!
//! - [`MsbBitWriter::put_bits`] behaves like LHA `putbits(n, x)` — it appends
//!   the low `n` bits of `x` to the stream, most significant first.
//! - [`MsbBitReader::get_bits`] behaves like LHA `getbits(n)` — it returns the
//!   next `n` bits, most significant first, and advances.
//! - [`MsbBitReader::peek_bits`] returns the next `n` bits **without** advancing
//!   (canonical Huffman decoding peeks a maximum-width code, looks it up, then
//!   [`skip_bits`](MsbBitReader::skip_bits)s exactly the matched code length).
//!
//! # End-of-stream convention
//!
//! Canonical LHA `fillbuf` does **not** error at end of input: once the known
//! compressed size is exhausted it feeds **zero bytes** (`subbitbuf = 0`), so a
//! Huffman decoder can always peek a full-width code even on the final byte.
//! [`MsbBitReader`] follows that convention: past the physical end of the
//! underlying reader it synthesizes zero bits instead of returning an error.
//! (This differs deliberately from the LSB-first
//! [`BitReader`](crate::bitstream::BitReader), which returns an
//! `unexpected_eof` error — that type is not used where LHA's zero-padding
//! semantics are required.)
//!
//! Because zero-padding never terminates on its own, callers must bound their
//! reads by a known size (the compressed/original sizes from the LZH header,
//! exactly as canonical LHA bounds decoding by `compsize`/`origsize`).
//! [`MsbBitReader::padding_bits`] reports how many synthetic zero bits have been
//! injected, so a higher layer can detect a truncated stream.
//!
//! Genuine I/O failures of the underlying reader/writer are still surfaced as
//! `io::Error`; only clean end-of-input is treated as zero padding.
//!
//! # Example
//!
//! ```
//! use oxiarc_core::msb_bitstream::{MsbBitReader, MsbBitWriter};
//! use std::io::Cursor;
//!
//! # fn main() -> std::io::Result<()> {
//! // Writing bits, MSB-first.
//! let mut output = Vec::new();
//! {
//!     let mut writer = MsbBitWriter::new(&mut output);
//!     writer.put_bits(3, 0b101)?; // emits 1, 0, 1
//!     writer.put_bits(1, 0b1)?;   // emits 1
//!     writer.flush()?;            // zero-pads the final byte
//! }
//! // 101 1 padded with 0000 -> 0b1011_0000 == 0xB0
//! assert_eq!(output, vec![0xB0]);
//!
//! // Reading them back.
//! let mut reader = MsbBitReader::new(Cursor::new(&output));
//! assert_eq!(reader.get_bits(3)?, 0b101);
//! assert_eq!(reader.get_bits(1)?, 0b1);
//! # Ok(())
//! # }
//! ```

use std::io::{self, Read, Write};

/// Maximum number of bits that a single `put`/`get`/`peek`/`skip` call handles.
///
/// Chosen to match the LSB-first [`BitReader`](crate::bitstream::BitReader),
/// which also caps at 32 bits per call. Canonical LHA only ever needs up to 16
/// bits at once, so this is comfortably sufficient.
const MAX_BITS: u8 = 32;

/// Return a mask selecting the low `n` bits (`n` in `0..=64`).
#[inline]
fn low_mask(n: u8) -> u64 {
    if n == 0 {
        0
    } else if n >= 64 {
        u64::MAX
    } else {
        (1u64 << n) - 1
    }
}

/// A most-significant-bit-first bit writer wrapping any [`Write`].
///
/// Bits accumulate in an internal buffer and complete bytes are emitted with
/// the first-written bit in the most significant position, matching canonical
/// LZH/LHA `putbits`. Call [`flush`](MsbBitWriter::flush) when finished to write
/// any trailing partial byte (zero-padded on the low side).
///
/// A best-effort [`flush`](MsbBitWriter::flush) also runs on drop, so forgetting
/// to flush does not silently lose the final byte; prefer an explicit `flush()`
/// so that any I/O error is observed.
#[derive(Debug)]
pub struct MsbBitWriter<W: Write> {
    /// Underlying writer.
    writer: W,
    /// Pending bits, right-aligned: the low `nbits` bits are valid and the
    /// most-significant of them is the next bit to be emitted.
    acc: u64,
    /// Number of valid pending bits in `acc` (`0..=7` between calls).
    nbits: u8,
    /// Total number of logical bits written (excludes flush zero-padding).
    total_bits_written: u64,
}

impl<W: Write> MsbBitWriter<W> {
    /// Create a new `MsbBitWriter` wrapping `writer`.
    pub fn new(writer: W) -> Self {
        Self {
            writer,
            acc: 0,
            nbits: 0,
            total_bits_written: 0,
        }
    }

    /// Get a shared reference to the underlying writer.
    pub fn get_ref(&self) -> &W {
        &self.writer
    }

    /// Get a mutable reference to the underlying writer.
    ///
    /// Writing directly through this reference while bits are buffered will
    /// interleave incorrectly; only use it when byte-aligned (that is, right
    /// after a [`flush`](MsbBitWriter::flush)).
    pub fn get_mut(&mut self) -> &mut W {
        &mut self.writer
    }

    /// Total number of logical bits written so far (flush padding not counted).
    pub fn bits_written(&self) -> u64 {
        self.total_bits_written
    }

    /// Write the low `n` bits of `value`, most-significant bit first.
    ///
    /// Bit `n - 1` of `value` is written first and bit `0` last. Bits of
    /// `value` above bit `n - 1` are ignored. Writing `n == 0` is a no-op.
    ///
    /// `n` must be `<= 32`.
    ///
    /// # Errors
    ///
    /// Returns any I/O error raised while writing completed bytes to the
    /// underlying writer.
    pub fn put_bits(&mut self, n: u8, value: u32) -> io::Result<()> {
        debug_assert!(n <= MAX_BITS, "put_bits: n must be <= 32, got {n}");
        if n == 0 {
            return Ok(());
        }

        // Append the new bits below the existing pending bits so that
        // earlier-written bits stay more significant (emitted first).
        self.acc = (self.acc << n) | (u64::from(value) & low_mask(n));
        self.nbits += n;
        self.total_bits_written += u64::from(n);

        // Drain every complete byte from the top of the valid region.
        // At most (7 + 32) / 8 == 4 bytes can become available per call.
        let mut out = [0u8; 8];
        let mut count = 0usize;
        while self.nbits >= 8 {
            self.nbits -= 8;
            out[count] = ((self.acc >> self.nbits) & 0xFF) as u8;
            count += 1;
        }
        if count > 0 {
            self.writer.write_all(&out[..count])?;
        }

        // Keep only the still-pending low bits; drop the just-emitted ones so
        // stale high bits cannot leak into a later shift.
        self.acc &= low_mask(self.nbits);
        Ok(())
    }

    /// Write a single bit (`true` == 1) most-significant-first.
    ///
    /// # Errors
    ///
    /// Returns any I/O error raised while writing a completed byte.
    pub fn put_bit(&mut self, bit: bool) -> io::Result<()> {
        self.put_bits(1, u32::from(bit))
    }

    /// Flush any buffered partial byte and the underlying writer.
    ///
    /// A trailing partial byte is left-justified (its valid bits occupy the
    /// most significant positions) and zero-padded on the low side, matching
    /// canonical LHA, which writes `subbitbuf` as-is at end of stream. This is
    /// idempotent: after a flush the buffer is empty, so a second flush only
    /// forwards to the underlying writer.
    ///
    /// # Errors
    ///
    /// Returns any I/O error raised while writing the final byte or flushing
    /// the underlying writer.
    pub fn flush(&mut self) -> io::Result<()> {
        if self.nbits > 0 {
            let byte = ((self.acc << (8 - self.nbits)) & 0xFF) as u8;
            self.writer.write_all(&[byte])?;
            self.acc = 0;
            self.nbits = 0;
        }
        self.writer.flush()
    }
}

impl<W: Write> Drop for MsbBitWriter<W> {
    fn drop(&mut self) {
        // Best-effort flush so a forgotten explicit flush does not drop the
        // final partial byte. Errors cannot be reported from drop.
        let _ = self.flush();
    }
}

/// A most-significant-bit-first bit reader wrapping any [`Read`].
///
/// Returns bits with the first bit of the stream taken from the most
/// significant bit of the first byte, matching canonical LZH/LHA `getbits`.
/// Past physical end-of-input it synthesizes zero bits (see the
/// [module documentation](self) for the rationale and
/// [`padding_bits`](MsbBitReader::padding_bits) for detecting it).
#[derive(Debug)]
pub struct MsbBitReader<R: Read> {
    /// Underlying reader.
    reader: R,
    /// Buffered bits, right-aligned: the low `nbits` bits are valid and the
    /// most-significant of them is the next bit to be returned.
    acc: u64,
    /// Number of valid buffered bits in `acc`.
    nbits: u8,
    /// Total number of bits consumed via `get`/`skip` (peeks do not count).
    total_bits_read: u64,
    /// Count of synthetic zero bits injected past physical end-of-input.
    padding_bits: u64,
    /// Whether the underlying reader has reported end-of-input at least once.
    eof: bool,
}

impl<R: Read> MsbBitReader<R> {
    /// Create a new `MsbBitReader` wrapping `reader`.
    pub fn new(reader: R) -> Self {
        Self {
            reader,
            acc: 0,
            nbits: 0,
            total_bits_read: 0,
            padding_bits: 0,
            eof: false,
        }
    }

    /// Get a shared reference to the underlying reader.
    pub fn get_ref(&self) -> &R {
        &self.reader
    }

    /// Get a mutable reference to the underlying reader.
    ///
    /// Reading directly through this reference while bits are buffered will
    /// desynchronize the stream; only use it when byte-aligned.
    pub fn get_mut(&mut self) -> &mut R {
        &mut self.reader
    }

    /// Total number of bits consumed so far (peeks are not counted).
    pub fn bits_read(&self) -> u64 {
        self.total_bits_read
    }

    /// Number of synthetic zero bits injected past the physical end of input.
    ///
    /// Zero means no read has gone past the real data. A non-zero value means a
    /// caller requested more bits than the stream contained and the reader
    /// zero-padded (in whole-byte units), which a higher layer can treat as a
    /// truncated/over-read stream.
    pub fn padding_bits(&self) -> u64 {
        self.padding_bits
    }

    /// Read one byte from the underlying reader, returning `0` at end-of-input.
    ///
    /// Clean end-of-input is reported as a zero byte (and recorded in
    /// [`padding_bits`](Self::padding_bits)); genuine I/O errors propagate.
    fn pull_byte(&mut self) -> io::Result<u8> {
        let mut buf = [0u8; 1];
        loop {
            match self.reader.read(&mut buf) {
                Ok(0) => {
                    self.eof = true;
                    self.padding_bits += 8;
                    return Ok(0);
                }
                Ok(_) => return Ok(buf[0]),
                Err(ref e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
    }

    /// Ensure at least `n` bits are buffered, pulling bytes (zero past EOF).
    fn fill(&mut self, n: u8) -> io::Result<()> {
        while self.nbits < n {
            let byte = self.pull_byte()?;
            self.acc = (self.acc << 8) | u64::from(byte);
            self.nbits += 8;
        }
        Ok(())
    }

    /// Read `n` bits, most-significant bit first, and advance.
    ///
    /// The first bit read occupies bit `n - 1` of the result. Reading `n == 0`
    /// returns `0`. `n` must be `<= 32`.
    ///
    /// # Errors
    ///
    /// Returns any genuine I/O error from the underlying reader. End-of-input
    /// is not an error: exhausted input yields zero bits.
    pub fn get_bits(&mut self, n: u8) -> io::Result<u32> {
        debug_assert!(n <= MAX_BITS, "get_bits: n must be <= 32, got {n}");
        if n == 0 {
            return Ok(0);
        }
        self.fill(n)?;
        self.nbits -= n;
        let value = ((self.acc >> self.nbits) & low_mask(n)) as u32;
        self.acc &= low_mask(self.nbits);
        self.total_bits_read += u64::from(n);
        Ok(value)
    }

    /// Read a single bit (`true` == 1) most-significant-first, and advance.
    ///
    /// # Errors
    ///
    /// Returns any genuine I/O error from the underlying reader.
    pub fn get_bit(&mut self) -> io::Result<bool> {
        Ok(self.get_bits(1)? != 0)
    }

    /// Read `n` bits, most-significant bit first, **without** advancing.
    ///
    /// A subsequent [`get_bits`](Self::get_bits) / [`skip_bits`](Self::skip_bits)
    /// of the same width consumes exactly these bits. Peeking `n == 0` returns
    /// `0`. `n` must be `<= 32`.
    ///
    /// # Errors
    ///
    /// Returns any genuine I/O error from the underlying reader. End-of-input
    /// is not an error: exhausted input yields zero bits.
    pub fn peek_bits(&mut self, n: u8) -> io::Result<u32> {
        debug_assert!(n <= MAX_BITS, "peek_bits: n must be <= 32, got {n}");
        if n == 0 {
            return Ok(0);
        }
        self.fill(n)?;
        Ok(((self.acc >> (self.nbits - n)) & low_mask(n)) as u32)
    }

    /// Discard `n` bits, most-significant first. Skipping `n == 0` is a no-op.
    ///
    /// `n` must be `<= 32`.
    ///
    /// # Errors
    ///
    /// Returns any genuine I/O error from the underlying reader. End-of-input
    /// is not an error: exhausted input is treated as zero bits.
    pub fn skip_bits(&mut self, n: u8) -> io::Result<()> {
        debug_assert!(n <= MAX_BITS, "skip_bits: n must be <= 32, got {n}");
        if n == 0 {
            return Ok(());
        }
        self.fill(n)?;
        self.nbits -= n;
        self.acc &= low_mask(self.nbits);
        self.total_bits_read += u64::from(n);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// Collect a writer's output for a closure that drives it, flushing first.
    fn write_out<F>(f: F) -> Vec<u8>
    where
        F: FnOnce(&mut MsbBitWriter<&mut Vec<u8>>) -> io::Result<()>,
    {
        let mut out = Vec::new();
        {
            let mut w = MsbBitWriter::new(&mut out);
            f(&mut w).expect("write closure");
            w.flush().expect("flush");
        }
        out
    }

    #[test]
    fn put_bits_byte_exact_101_then_1() {
        // 0b101 (3 bits) then 0b1 (1 bit) -> 1011 -> padded 1011_0000 == 0xB0.
        let out = write_out(|w| {
            w.put_bits(3, 0b101)?;
            w.put_bits(1, 0b1)?;
            Ok(())
        });
        assert_eq!(out, vec![0xB0]);
    }

    #[test]
    fn put_bits_single_high_bit() {
        // One '1' bit lands in the most significant position: 0b1000_0000.
        let out = write_out(|w| w.put_bits(1, 1));
        assert_eq!(out, vec![0x80]);
    }

    #[test]
    fn put_bits_full_byte() {
        let out = write_out(|w| w.put_bits(8, 0xFF));
        assert_eq!(out, vec![0xFF]);
    }

    #[test]
    fn put_bits_two_nibbles() {
        // 0xA then 0xB -> 0xAB (MSB-first nibble packing).
        let out = write_out(|w| {
            w.put_bits(4, 0xA)?;
            w.put_bits(4, 0xB)?;
            Ok(())
        });
        assert_eq!(out, vec![0xAB]);
    }

    #[test]
    fn put_bits_twelve_bits_spans_two_bytes() {
        // 0xAF3 = 1010_1111_0011 -> 1010_1111 (0xAF), 0011 padded 0011_0000 (0x30).
        let out = write_out(|w| w.put_bits(12, 0xAF3));
        assert_eq!(out, vec![0xAF, 0x30]);
    }

    #[test]
    fn put_bits_nine_bits() {
        // 0x101 = 1_0000_0001 -> 1000_0000 (0x80), then 1 padded 1000_0000 (0x80).
        let out = write_out(|w| w.put_bits(9, 0x101));
        assert_eq!(out, vec![0x80, 0x80]);
    }

    #[test]
    fn put_bits_sixteen_is_big_endian() {
        // MSB-first 16-bit write is a big-endian u16.
        let out = write_out(|w| w.put_bits(16, 0x1234));
        assert_eq!(out, vec![0x12, 0x34]);
    }

    #[test]
    fn get_bits_sixteen_is_big_endian() {
        // Symmetric big-endian read.
        let mut r = MsbBitReader::new(Cursor::new(vec![0x12u8, 0x34]));
        assert_eq!(r.get_bits(16).expect("get 16"), 0x1234);
    }

    #[test]
    fn canonical_lha_getbits_semantics() {
        // First stream bit == MSB of first byte. 0xB0 == 1011_0000.
        let mut r = MsbBitReader::new(Cursor::new(vec![0xB0u8, 0x00]));
        assert_eq!(r.get_bits(3).expect("get 3"), 0b101);
        assert_eq!(r.get_bits(1).expect("get 1"), 0b1);
        assert_eq!(r.get_bits(4).expect("get 4"), 0b0000);
    }

    #[test]
    fn byte_aligned_stream_reads_as_bytes() {
        let data = vec![0x12u8, 0x34, 0x56, 0x78];
        let mut r = MsbBitReader::new(Cursor::new(data.clone()));
        for &b in &data {
            assert_eq!(r.get_bits(8).expect("get byte"), u32::from(b));
        }
    }

    #[test]
    fn peek_then_get_matches_and_advances() {
        let mut r = MsbBitReader::new(Cursor::new(vec![0xB7u8, 0x2Cu8]));
        // peek is stable and equals the following get.
        let p1 = r.peek_bits(6).expect("peek 6");
        let p1b = r.peek_bits(6).expect("peek 6 again");
        assert_eq!(p1, p1b);
        let g1 = r.get_bits(6).expect("get 6");
        assert_eq!(p1, g1);
        // The stream advanced: the next peek differs from the first bits.
        let g2 = r.get_bits(6).expect("get 6 more");
        // 0xB72C = 1011_0111_0010_1100; first 6 = 101101, next 6 = 110010.
        assert_eq!(g1, 0b101101);
        assert_eq!(g2, 0b110010);
    }

    #[test]
    fn cross_byte_boundary_splits_are_consistent() {
        // Write 12 bits, then read them back with two different split points.
        let out = write_out(|w| w.put_bits(12, 0xABC));
        assert_eq!(out, vec![0xAB, 0xC0]);

        let mut r48 = MsbBitReader::new(Cursor::new(out.clone()));
        let a = r48.get_bits(4).expect("get 4");
        let b = r48.get_bits(8).expect("get 8");
        assert_eq!(a, 0xA);
        assert_eq!(b, 0xBC);
        assert_eq!((a << 8) | b, 0xABC);

        let mut r39 = MsbBitReader::new(Cursor::new(out));
        let c = r39.get_bits(3).expect("get 3");
        let d = r39.get_bits(9).expect("get 9");
        assert_eq!((c << 9) | d, 0xABC);
    }

    #[test]
    fn skip_bits_advances_correctly() {
        let mut r = MsbBitReader::new(Cursor::new(vec![0xABu8, 0xCD]));
        assert_eq!(r.get_bits(4).expect("get 4"), 0xA);
        r.skip_bits(4).expect("skip 4");
        assert_eq!(r.get_bits(8).expect("get 8"), 0xCD);
    }

    #[test]
    fn zero_width_operations() {
        // Writing zero bits emits nothing.
        let out = write_out(|w| {
            w.put_bits(0, 0xDEAD)?;
            w.put_bits(4, 0x5)?;
            w.put_bits(0, 0x1)?;
            Ok(())
        });
        assert_eq!(out, vec![0x50]);

        let mut r = MsbBitReader::new(Cursor::new(vec![0xFFu8]));
        assert_eq!(r.get_bits(0).expect("get 0"), 0);
        assert_eq!(r.peek_bits(0).expect("peek 0"), 0);
        r.skip_bits(0).expect("skip 0");
        assert_eq!(r.bits_read(), 0);
    }

    #[test]
    fn eof_zero_pads_and_reports_padding() {
        let mut r = MsbBitReader::new(Cursor::new(vec![0xFFu8]));
        assert_eq!(r.get_bits(4).expect("get 4"), 0xF);
        assert_eq!(r.get_bits(4).expect("get 4"), 0xF);
        assert_eq!(r.padding_bits(), 0, "no padding while real data remained");
        // Now read past the single byte: zero-padded, padding recorded.
        assert_eq!(r.get_bits(8).expect("get past eof"), 0);
        assert!(r.padding_bits() >= 8);
        // Peeking past EOF also yields zeros without erroring.
        assert_eq!(r.peek_bits(16).expect("peek past eof"), 0);
    }

    #[test]
    fn reading_exact_content_injects_no_padding() {
        let mut r = MsbBitReader::new(Cursor::new(vec![0x12u8, 0x34]));
        assert_eq!(r.get_bits(16).expect("get 16"), 0x1234);
        assert_eq!(r.padding_bits(), 0);
        assert_eq!(r.bits_read(), 16);
    }

    #[test]
    fn drop_flushes_final_partial_byte() {
        let mut out = Vec::new();
        {
            let mut w = MsbBitWriter::new(&mut out);
            w.put_bits(3, 0b101).expect("put 3");
            // No explicit flush: Drop must flush the partial byte.
        }
        // 101 padded 0000_0 -> 1010_0000 == 0xA0.
        assert_eq!(out, vec![0xA0]);
    }

    #[test]
    fn flush_is_idempotent() {
        let mut out = Vec::new();
        {
            let mut w = MsbBitWriter::new(&mut out);
            w.put_bits(5, 0b10110).expect("put 5");
            w.flush().expect("flush 1");
            w.flush().expect("flush 2");
        }
        // 10110 padded 000 -> 1011_0000 == 0xB0.
        assert_eq!(out, vec![0xB0]);
    }

    #[test]
    fn bit_counters_track_logical_bits() {
        let mut out = Vec::new();
        let written;
        {
            let mut w = MsbBitWriter::new(&mut out);
            w.put_bits(3, 0b101).expect("put 3");
            w.put_bits(13, 0x1FFF).expect("put 13");
            written = w.bits_written();
            w.flush().expect("flush");
        }
        assert_eq!(written, 16);
        let mut r = MsbBitReader::new(Cursor::new(out));
        r.get_bits(3).expect("get 3");
        r.peek_bits(13).expect("peek 13"); // peek must not count
        r.get_bits(13).expect("get 13");
        assert_eq!(r.bits_read(), 16);
    }

    #[test]
    fn roundtrip_specified_widths() {
        // The widths the task calls out: 1, 3, 5, 7, 8, 9, 16 bits.
        let items: [(u8, u32); 7] = [
            (1, 0b1),
            (3, 0b101),
            (5, 0b1_0110),
            (7, 0b110_0101),
            (8, 0xC3),
            (9, 0x1A5),
            (16, 0xBEEF),
        ];
        let out = write_out(|w| {
            for &(n, v) in &items {
                w.put_bits(n, v)?;
            }
            Ok(())
        });
        let mut r = MsbBitReader::new(Cursor::new(out));
        for &(n, v) in &items {
            assert_eq!(r.get_bits(n).expect("roundtrip get"), v, "width {n}");
        }
    }

    #[test]
    fn roundtrip_pseudo_random_stream() {
        // Deterministic LCG-driven stream of varied widths and values, to stress
        // cross-byte packing without any external dependency.
        let mut state: u64 = 0x1234_5678_9abc_def0;
        let mut next = || {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            state
        };

        let mut items: Vec<(u8, u32)> = Vec::new();
        for _ in 0..5000 {
            let r = next();
            let n = ((r >> 40) % 32) as u8 + 1; // width in 1..=32
            let v = (r as u32) & (low_mask(n) as u32);
            items.push((n, v));
        }

        let out = write_out(|w| {
            for &(n, v) in &items {
                w.put_bits(n, v)?;
            }
            Ok(())
        });

        let mut reader = MsbBitReader::new(Cursor::new(out));
        for &(n, v) in &items {
            assert_eq!(reader.get_bits(n).expect("get"), v, "width {n}");
        }
        // Everything consumed exactly matched what was written.
        assert_eq!(reader.padding_bits(), 0);
    }

    #[test]
    fn peek_skip_matches_get_over_random_stream() {
        // For each item, peeking then skipping must equal a plain get.
        let mut state: u64 = 0xdead_beef_cafe_babe;
        let mut next = || {
            state = state
                .wrapping_mul(2862933555777941757)
                .wrapping_add(3037000493);
            state
        };
        let mut items: Vec<(u8, u32)> = Vec::new();
        for _ in 0..2000 {
            let r = next();
            let n = ((r >> 33) % 16) as u8 + 1; // 1..=16, LHA's practical range
            let v = (r as u32) & (low_mask(n) as u32);
            items.push((n, v));
        }
        let out = write_out(|w| {
            for &(n, v) in &items {
                w.put_bits(n, v)?;
            }
            Ok(())
        });
        let mut reader = MsbBitReader::new(Cursor::new(out));
        for &(n, v) in &items {
            let peeked = reader.peek_bits(n).expect("peek");
            assert_eq!(peeked, v, "peek width {n}");
            reader.skip_bits(n).expect("skip");
        }
    }
}
