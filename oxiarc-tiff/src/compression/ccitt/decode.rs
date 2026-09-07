//! The changing-element engine: one-dimensional and two-dimensional row
//! decoding shared by CCITT RLE, Group 3 and Group 4.
//!
//! A row is represented by its **changing elements** — the pixel positions at
//! which the colour flips, in increasing order, starting from an implicit white
//! run. That is the representation T.4 and T.6 are written in, it makes the
//! two-dimensional `b1`/`b2` search a cursor walk rather than a pixel scan, and
//! it costs two `Vec<u32>` per chunk that are cleared and reused for every row.

use super::bits::BitReader;
use super::tables::{EOL_BITS, EOL_CODE, LOOKUP_BITS, Mode, RunCode, mode_code, run_code};
use super::uncompressed::{self, Word};

/// What went wrong inside one row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RowFault {
    /// The bits are not a code word.
    InvalidCode {
        /// Bit offset in the chunk, for the message.
        position: usize,
    },
    /// A run would have run past the right edge of the row.
    Overrun {
        /// Where the run would have ended.
        position: u32,
    },
    /// The chunk ended in the middle of a row.
    Truncated,
    /// An end-of-line code arrived before the row was full.
    ShortRow {
        /// Pixels the row was missing.
        missing: u32,
    },
    /// The extension code selected uncompressed mode, but the segment that
    /// followed was not decodable.
    UncompressedMode,
    /// The extension code selected something T.4 does not define.
    UnknownExtension {
        /// The three extension bits.
        code: u8,
    },
}

/// The maximum number of code words one row may contain.
///
/// Two per pixel is already unreachable for a well-formed row (a changing
/// element cannot repeat forever); the bound exists so a stream of zero-length
/// horizontal runs cannot spin.
const fn code_budget(width: u32) -> u32 {
    width.saturating_mul(2).saturating_add(64)
}

/// Fill bits are tolerated before an end-of-line code; this caps how many.
const MAX_FILL_BITS: usize = 4096;

/// The row engine.
#[derive(Debug)]
pub(super) struct FaxDecoder<'a> {
    /// The chunk's bit stream.
    pub(super) bits: BitReader<'a>,
    /// Row width in pixels.
    width: u32,
    /// Changing elements of the row above (empty = an all-white line).
    reference: Vec<u32>,
    /// Changing elements of the row being decoded.
    current: Vec<u32>,
    /// How many reference elements are at or left of the current `a0`.
    ///
    /// `a0` never moves backwards inside a row (T.6's changing elements are
    /// non-decreasing, and [`FaxDecoder::decode_2d_row`] rejects a stream that
    /// says otherwise), so the search in [`FaxDecoder::locate`] can resume
    /// where the last one stopped instead of restarting at element zero. That
    /// turns a row with `n` changing elements from `O(n^2)` into `O(n)`, which
    /// on a 4096-pixel fax page with a few hundred runs per row is the
    /// difference between 353 ms and 84 ms for the whole image.
    ref_cursor: usize,
}

impl<'a> FaxDecoder<'a> {
    /// A decoder over `data`, with the imaginary all-white reference line T.4
    /// requires at the top of every strip or tile.
    pub(super) fn new(data: &'a [u8], width: u32, reversed: bool) -> Self {
        Self::with_scratch(
            data,
            width,
            reversed,
            (Vec::with_capacity(64), Vec::with_capacity(64)),
        )
    }

    /// The same, over two changing-element buffers borrowed from a pool.
    ///
    /// Both are cleared before use, so a caller may hand over the buffers of
    /// the previous chunk and keep their capacity.
    pub(super) fn with_scratch(
        data: &'a [u8],
        width: u32,
        reversed: bool,
        scratch: (Vec<u32>, Vec<u32>),
    ) -> Self {
        let (mut reference, mut current) = scratch;
        reference.clear();
        current.clear();
        Self {
            bits: BitReader::new(data, reversed),
            width,
            reference,
            current,
            ref_cursor: 0,
        }
    }

    /// Gives the changing-element buffers back to the pool.
    pub(super) fn into_scratch(self) -> (Vec<u32>, Vec<u32>) {
        (self.reference, self.current)
    }

    /// Makes the row just decoded the reference line for the next one.
    pub(super) fn promote_row(&mut self) {
        core::mem::swap(&mut self.reference, &mut self.current);
    }

    /// Reads one complete run of `white` pixels.
    fn read_run(&mut self, white: bool) -> Result<u32, RowFault> {
        let mut total = 0u32;
        loop {
            if self.bits.remaining() == 0 {
                return Err(RowFault::Truncated);
            }
            let window = self.bits.peek(LOOKUP_BITS);
            match run_code(window, white) {
                RunCode::Terminating { bits, run } => {
                    self.bits.skip(usize::from(bits));
                    return Ok(total.saturating_add(u32::from(run)));
                }
                RunCode::MakeUp { bits, run } => {
                    self.bits.skip(usize::from(bits));
                    total = total.saturating_add(u32::from(run));
                }
                RunCode::Eol => {
                    return Err(RowFault::ShortRow { missing: 0 });
                }
                RunCode::Invalid => {
                    return Err(RowFault::InvalidCode {
                        position: self.bits.position(),
                    });
                }
            }
        }
    }

    /// Decodes one one-dimensional (Modified Huffman) row.
    pub(super) fn decode_1d_row(&mut self) -> Result<(), RowFault> {
        self.current.clear();
        let mut a0 = 0u32;
        let mut white = true;
        let mut budget = code_budget(self.width);
        while a0 < self.width {
            if budget == 0 {
                return Err(RowFault::InvalidCode {
                    position: self.bits.position(),
                });
            }
            budget -= 1;
            if self.bits.peek(uncompressed::ENTER_1D_BITS) == uncompressed::ENTER_1D_CODE {
                // `000000001111`: no run code has eight leading zeros (the
                // longest, the extended make-ups, have seven), so this can
                // only be the one-dimensional entrance code.
                self.bits.skip(usize::from(uncompressed::ENTER_1D_BITS));
                let (at, next_is_black) = self.decode_uncompressed(a0, !white)?;
                a0 = at;
                white = !next_is_black;
                continue;
            }
            let run = match self.read_run(white) {
                Ok(run) => run,
                Err(RowFault::ShortRow { .. }) => {
                    return Err(RowFault::ShortRow {
                        missing: self.width - a0,
                    });
                }
                Err(other) => return Err(other),
            };
            let end = u64::from(a0) + u64::from(run);
            if end > u64::from(self.width) {
                return Err(RowFault::Overrun {
                    position: end.min(u64::from(u32::MAX)) as u32,
                });
            }
            a0 = end as u32;
            if a0 < self.width {
                // A "change" at the row width is outside the row: leaving it
                // out keeps the decoder's representation identical to the
                // encoder's, so a row can be re-coded from what was decoded.
                self.current.push(a0);
            }
            white = !white;
        }
        Ok(())
    }

    /// `b1` and `b2` for the current `a0` and colour.
    ///
    /// `b1` is the first changing element of the reference line strictly right
    /// of `a0` whose colour is opposite to the colour of `a0`; `b2` is the one
    /// after it. Changing elements at an even index change to black, at an odd
    /// index to white, because every line starts white.
    fn locate(&mut self, a0: i64, white: bool) -> (u32, u32) {
        // Resume where the previous call stopped: `a0` is non-decreasing
        // within a row, so every element the last search skipped is still at
        // or left of this `a0`.
        while self.ref_cursor < self.reference.len() {
            match self.reference.get(self.ref_cursor) {
                Some(position) if i64::from(*position) <= a0 => self.ref_cursor += 1,
                _ => break,
            }
        }
        // The parity fix-up is *not* carried into the cursor: which of the two
        // neighbouring elements is `b1` depends on the colour of `a0`, which
        // changes from call to call.
        let mut index = self.ref_cursor;
        if (index % 2 == 0) != white {
            index += 1;
        }
        let b1 = self
            .reference
            .get(index)
            .copied()
            .unwrap_or(self.width)
            .min(self.width);
        let b2 = self
            .reference
            .get(index + 1)
            .copied()
            .unwrap_or(self.width)
            .min(self.width);
        (b1, b2)
    }

    /// Opens a run of `black` pixels at `position`.
    ///
    /// A changing element is pushed only where the colour actually changes,
    /// and never at or past the row width — the same representation
    /// `encode::row_changes` builds from a packed row, so a row that went
    /// through uncompressed mode re-encodes exactly like one that did not.
    fn open_run(&mut self, position: u32, black: bool, open: &mut bool) {
        if black == *open {
            return;
        }
        if position < self.width {
            self.current.push(position);
        }
        *open = black;
    }

    /// Decodes one uncompressed-mode segment (T.4 Table 5), entrance code
    /// already consumed.
    ///
    /// `position` is the first pixel the segment covers and `black_run` the
    /// colour of the run open at it. Returns the position and colour to
    /// resume ordinary coding with — the colour comes from the exit code's
    /// tag bit, which is the whole point of having one.
    fn decode_uncompressed(
        &mut self,
        position: u32,
        black_run: bool,
    ) -> Result<(u32, bool), RowFault> {
        let mut at = position;
        let mut black = black_run;
        // Every word covers at least one pixel except the exit code, so the
        // row width plus one bounds the loop even on a hostile stream.
        let mut budget = code_budget(self.width);
        loop {
            if budget == 0 {
                return Err(RowFault::InvalidCode {
                    position: self.bits.position(),
                });
            }
            budget -= 1;
            let word = uncompressed::read_word(&mut self.bits);
            let (white_pixels, black_pixel, exit) = match word {
                Word::Pixels { white } => (u32::from(white), true, None),
                Word::FiveWhite => (5, false, None),
                Word::Exit {
                    white,
                    next_is_black,
                } => (u32::from(white), false, Some(next_is_black)),
                Word::Truncated => return Err(RowFault::Truncated),
                Word::Invalid => {
                    return Err(RowFault::InvalidCode {
                        position: self.bits.position(),
                    });
                }
            };
            let span = white_pixels + u32::from(black_pixel);
            let end = u64::from(at) + u64::from(span);
            if end > u64::from(self.width) {
                return Err(RowFault::Overrun {
                    position: end.min(u64::from(u32::MAX)) as u32,
                });
            }
            if white_pixels > 0 {
                self.open_run(at, false, &mut black);
                at += white_pixels;
            }
            if black_pixel {
                self.open_run(at, true, &mut black);
                at += 1;
            }
            if let Some(next_is_black) = exit {
                // The tag bit opens the run that follows the segment; a
                // colour change at the exit point is a changing element like
                // any other.
                self.open_run(at, next_is_black, &mut black);
                return Ok((at, next_is_black));
            }
        }
    }

    /// Decodes one two-dimensional row against the reference line.
    pub(super) fn decode_2d_row(&mut self) -> Result<(), RowFault> {
        self.current.clear();
        self.ref_cursor = 0;
        let mut a0: i64 = -1;
        let mut white = true;
        let mut budget = code_budget(self.width);
        while a0 < i64::from(self.width) {
            if budget == 0 {
                return Err(RowFault::InvalidCode {
                    position: self.bits.position(),
                });
            }
            budget -= 1;
            if self.bits.remaining() == 0 {
                return Err(RowFault::Truncated);
            }
            let (b1, b2) = self.locate(a0, white);
            let window = (self.bits.peek(7)) as u8;
            if window == 0 {
                // Seven zero bits can only be an end-of-line (or fill).
                if self.bits.peek(EOL_BITS) == EOL_CODE {
                    let missing = i64::from(self.width) - a0.max(0);
                    return Err(RowFault::ShortRow {
                        missing: missing.max(0) as u32,
                    });
                }
                return Err(RowFault::InvalidCode {
                    position: self.bits.position(),
                });
            }
            if window == 0b000_0001 {
                // `0000001xxx`: an extension.
                let code = (self.bits.peek(10) & 0b111) as u8;
                if code != 0b111 {
                    return Err(RowFault::UnknownExtension { code });
                }
                self.bits.skip(usize::from(uncompressed::ENTER_2D_BITS));
                // `a0 = -1` means "just before pixel zero", which is where
                // the segment's first pixel goes.
                let (at, next_is_black) = self.decode_uncompressed(a0.max(0) as u32, !white)?;
                a0 = i64::from(at);
                white = !next_is_black;
                continue;
            }
            let Some((mode, bits)) = mode_code(window) else {
                return Err(RowFault::InvalidCode {
                    position: self.bits.position(),
                });
            };
            self.bits.skip(usize::from(bits));
            match mode {
                Mode::Pass => {
                    a0 = i64::from(b2);
                }
                Mode::Horizontal => {
                    let start = a0.max(0) as u32;
                    let run1 = self.read_run(white)?;
                    let run2 = self.read_run(!white)?;
                    let a1 = u64::from(start) + u64::from(run1);
                    let a2 = a1 + u64::from(run2);
                    if a2 > u64::from(self.width) {
                        return Err(RowFault::Overrun {
                            position: a2.min(u64::from(u32::MAX)) as u32,
                        });
                    }
                    if (a1 as u32) < self.width {
                        self.current.push(a1 as u32);
                    }
                    if (a2 as u32) < self.width {
                        self.current.push(a2 as u32);
                    }
                    a0 = a2 as i64;
                }
                Mode::Vertical(delta) => {
                    let a1 = i64::from(b1) + i64::from(delta);
                    if a1 < 0 || a1 > i64::from(self.width) {
                        return Err(RowFault::Overrun {
                            position: a1.clamp(0, i64::from(u32::MAX)) as u32,
                        });
                    }
                    if a1 <= a0 && a0 >= 0 {
                        // Changing elements must increase; a stream that says
                        // otherwise would loop forever.
                        return Err(RowFault::InvalidCode {
                            position: self.bits.position(),
                        });
                    }
                    if (a1 as u32) < self.width {
                        self.current.push(a1 as u32);
                    }
                    a0 = a1;
                    white = !white;
                }
            }
        }
        Ok(())
    }

    /// Replaces the current row with one that is white from `from` onwards.
    ///
    /// Used by the lenient recovery paths: a row that could not be decoded is
    /// completed in the background colour rather than left holding whatever the
    /// previous row had.
    pub(super) fn truncate_row(&mut self, from: u32) {
        self.current.retain(|position| *position < from);
        if self.current.len() % 2 == 1 {
            self.current.push(from);
        }
    }

    /// Paints the decoded row into `row`, one bit per pixel, MSB first.
    ///
    /// `white_bit` is 1 when the photometric interpretation says white pixels
    /// are ones (`BlackIsZero`), 0 when white is zero (`WhiteIsZero`, the
    /// default for fax data).
    pub(super) fn paint(&self, row: &mut [u8], white_bit: u8) {
        row.fill(if white_bit == 1 { 0xff } else { 0x00 });
        // Everything starts white; each changing element flips the colour.
        let mut start = 0u32;
        let mut white = true;
        for change in &self.current {
            let end = (*change).min(self.width);
            if !white {
                set_run(row, start, end, white_bit ^ 1);
            }
            start = end;
            white = !white;
        }
        if !white {
            set_run(row, start, self.width, white_bit ^ 1);
        }
    }

    /// Paints an entirely white row.
    pub(super) fn paint_white(row: &mut [u8], white_bit: u8) {
        row.fill(if white_bit == 1 { 0xff } else { 0x00 });
    }

    /// The changing elements of the row just decoded.
    #[cfg(test)]
    pub(super) fn row(&self) -> &[u32] {
        &self.current
    }

    /// Installs a reference line directly (tests only).
    #[cfg(test)]
    pub(super) fn set_reference(&mut self, changes: &[u32]) {
        self.reference.clear();
        self.reference.extend_from_slice(changes);
    }

    /// Consumes an end-of-line code that starts at the current bit, if there
    /// is one.
    ///
    /// Unlike [`FaxDecoder::consume_eol`] this skips no fill bits, because it
    /// is how Group 4 recognises `EOFB`: T.6 puts the two EOLs immediately
    /// after the last row, and a scan that tolerated fill would read eleven
    /// zero bits of *corrupt* data as an early end of image.
    pub(super) fn consume_eol_here(&mut self) -> bool {
        if self.bits.remaining() >= usize::from(EOL_BITS) && self.bits.peek(EOL_BITS) == EOL_CODE {
            self.bits.skip(usize::from(EOL_BITS));
            return true;
        }
        false
    }

    /// Skips fill bits and one end-of-line code, if one is there.
    ///
    /// Returns `true` when an EOL was consumed. Leading zero bits are fill:
    /// T.4's `EncodedByteAlign` inserts them so the EOL ends on a byte
    /// boundary, and several writers pad much more generously.
    pub(super) fn consume_eol(&mut self) -> bool {
        let start = self.bits.position();
        let mut fill = 0usize;
        loop {
            if self.bits.remaining() < usize::from(EOL_BITS) {
                self.bits.seek(start);
                return false;
            }
            if self.bits.peek(EOL_BITS) == EOL_CODE {
                self.bits.skip(usize::from(EOL_BITS));
                return true;
            }
            if self.bits.peek(1) != 0 || fill >= MAX_FILL_BITS {
                self.bits.seek(start);
                return false;
            }
            self.bits.skip(1);
            fill += 1;
        }
    }
}

/// Sets `[from, to)` of a packed row to `value` (0 or 1).
fn set_run(row: &mut [u8], from: u32, to: u32, value: u8) {
    if to <= from {
        return;
    }
    for bit in from..to {
        let index = (bit / 8) as usize;
        let mask = 0x80u8 >> (bit % 8);
        if let Some(byte) = row.get_mut(index) {
            if value == 1 {
                *byte |= mask;
            } else {
                *byte &= !mask;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::bits::BitWriter;
    use super::super::encode::{encode_1d_row, encode_2d_row};
    use super::*;

    /// Packs `pixels` (true = black) into a row and back through the codec.
    fn changes_of(pixels: &[bool]) -> Vec<u32> {
        let mut changes = Vec::new();
        let mut white = true;
        for (index, black) in pixels.iter().enumerate() {
            if *black == white {
                changes.push(index as u32);
                white = !white;
            }
        }
        changes
    }

    #[test]
    fn a_one_dimensional_row_round_trips() {
        let pixels: Vec<bool> = (0..64).map(|i| (i / 5) % 2 == 1).collect();
        let changes = changes_of(&pixels);
        let mut writer = BitWriter::new();
        encode_1d_row(&mut writer, &changes, pixels.len() as u32);
        let bytes = writer.finish(false);

        let mut decoder = FaxDecoder::new(&bytes, pixels.len() as u32, false);
        decoder.decode_1d_row().expect("decode");
        assert_eq!(decoder.row(), changes.as_slice());

        let mut row = vec![0u8; pixels.len().div_ceil(8)];
        decoder.paint(&mut row, 0);
        for (index, black) in pixels.iter().enumerate() {
            let bit = (row[index / 8] >> (7 - index % 8)) & 1;
            assert_eq!(bit == 1, *black, "pixel {index}");
        }
    }

    #[test]
    fn the_photometric_flag_inverts_the_painted_row() {
        let pixels: Vec<bool> = (0..16).map(|i| i >= 8).collect();
        let changes = changes_of(&pixels);
        let mut writer = BitWriter::new();
        encode_1d_row(&mut writer, &changes, 16);
        let bytes = writer.finish(false);
        let mut decoder = FaxDecoder::new(&bytes, 16, false);
        decoder.decode_1d_row().expect("decode");

        let mut min_is_white = vec![0u8; 2];
        decoder.paint(&mut min_is_white, 0);
        let mut min_is_black = vec![0u8; 2];
        decoder.paint(&mut min_is_black, 1);
        assert_eq!(min_is_white[0], 0x00);
        assert_eq!(min_is_white[1], 0xff);
        assert_eq!(min_is_black[0], 0xff);
        assert_eq!(min_is_black[1], 0x00);
    }

    #[test]
    fn a_two_dimensional_row_round_trips_against_its_reference() {
        let reference: Vec<bool> = (0..40).map(|i| (8..20).contains(&i)).collect();
        let row: Vec<bool> = (0..40).map(|i| (9..24).contains(&i)).collect();
        let ref_changes = changes_of(&reference);
        let row_changes = changes_of(&row);

        let mut writer = BitWriter::new();
        encode_2d_row(&mut writer, &row_changes, &ref_changes, 40);
        let bytes = writer.finish(false);

        let mut decoder = FaxDecoder::new(&bytes, 40, false);
        decoder.set_reference(&ref_changes);
        decoder.decode_2d_row().expect("decode");
        assert_eq!(decoder.row(), row_changes.as_slice());
    }

    #[test]
    fn every_vertical_offset_is_exercised() {
        // Reference with a change at 20 so b1 = 20 and each vertical mode
        // places a1 at 17..=23.
        let reference = vec![20u32, 40];
        for delta in -3i64..=3 {
            let a1 = (20 + delta) as u32;
            // A change at the row width is implicit, so the row is one entry.
            let row_changes = vec![a1];
            let mut writer = BitWriter::new();
            encode_2d_row(&mut writer, &row_changes, &reference, 40);
            let bytes = writer.finish(false);
            let mut decoder = FaxDecoder::new(&bytes, 40, false);
            decoder.set_reference(&reference);
            decoder.decode_2d_row().expect("decode");
            assert_eq!(decoder.row(), row_changes.as_slice(), "delta {delta}");
        }
    }

    #[test]
    fn pass_mode_is_decoded() {
        // A reference run that the current row passes over entirely.
        let reference = vec![4u32, 8];
        let row_changes: Vec<u32> = vec![12];
        let mut writer = BitWriter::new();
        encode_2d_row(&mut writer, &row_changes, &reference, 40);
        let bytes = writer.finish(false);
        let mut decoder = FaxDecoder::new(&bytes, 40, false);
        decoder.set_reference(&reference);
        decoder.decode_2d_row().expect("decode");
        assert_eq!(decoder.row(), row_changes.as_slice());
    }

    #[test]
    fn an_invalid_code_is_reported_with_its_position() {
        // Find a 13-bit window that spells no white code word, and feed it.
        let window = (0u16..(1 << LOOKUP_BITS))
            .find(|w| run_code(*w, true) == RunCode::Invalid)
            .expect("the white table cannot be complete over 13 bits");
        let data = [(window >> 5) as u8, (window << 3) as u8, 0, 0];
        let mut decoder = FaxDecoder::new(&data, 64, false);
        assert!(matches!(
            decoder.decode_1d_row(),
            Err(RowFault::InvalidCode { .. })
        ));
    }

    #[test]
    fn a_run_past_the_edge_is_an_overrun() {
        // One white make-up of 1728 on a 16-pixel row.
        let mut writer = BitWriter::new();
        super::super::tables::encode_run(1728 + 32, true, |code, bits| writer.write(code, bits));
        let bytes = writer.finish(false);
        let mut decoder = FaxDecoder::new(&bytes, 16, false);
        assert!(matches!(
            decoder.decode_1d_row(),
            Err(RowFault::Overrun { .. })
        ));
    }

    #[test]
    fn an_uncompressed_mode_extension_is_named() {
        let mut writer = BitWriter::new();
        writer.write(0b00_0000_1111, 10);
        let bytes = writer.finish(false);
        let mut decoder = FaxDecoder::new(&bytes, 40, false);
        assert_eq!(
            decoder.decode_2d_row(),
            Err(RowFault::UncompressedMode),
            "the extension code must be named, never mistaken for a run"
        );
    }

    #[test]
    fn other_extensions_are_reported_with_their_code() {
        let mut writer = BitWriter::new();
        writer.write(0b00_0000_1010, 10);
        let bytes = writer.finish(false);
        let mut decoder = FaxDecoder::new(&bytes, 40, false);
        assert_eq!(
            decoder.decode_2d_row(),
            Err(RowFault::UnknownExtension { code: 0b010 })
        );
    }

    #[test]
    fn end_of_line_codes_are_consumed_with_their_fill_bits() {
        let mut writer = BitWriter::new();
        writer.write_zeros(9);
        writer.write(EOL_CODE, EOL_BITS);
        writer.write(0b101, 3);
        let bytes = writer.finish(false);
        let mut decoder = FaxDecoder::new(&bytes, 8, false);
        assert!(decoder.consume_eol());
        assert_eq!(decoder.bits.peek(3), 0b101);
        assert!(!decoder.consume_eol());
        assert_eq!(decoder.bits.peek(3), 0b101, "a failed probe must not move");
    }

    #[test]
    fn a_truncated_row_is_reported_not_guessed() {
        let bytes = [0u8; 0];
        let mut decoder = FaxDecoder::new(&bytes, 32, false);
        assert_eq!(decoder.decode_1d_row(), Err(RowFault::Truncated));
        assert_eq!(decoder.decode_2d_row(), Err(RowFault::Truncated));
    }
}
