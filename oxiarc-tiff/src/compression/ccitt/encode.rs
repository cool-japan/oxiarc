//! CCITT encoders: Modified Huffman rows, Group 3 one- and two-dimensional
//! coding, and Group 4.
//!
//! The encoders work from the same changing-element representation the decoder
//! produces, so a round trip through this module is a round trip through one
//! model of a row rather than through two independent ones.

use super::bits::BitWriter;
use super::tables::{EOL_BITS, EOL_CODE, Mode, encode_run, mode_bits};
use super::uncompressed;

/// The changing element of `changes` that follows `a0` for a run of `white`.
///
/// Changing elements alternate colour and every line starts white, so an even
/// index is a change to black and an odd index a change to white. `width` is
/// the answer when the row has no further change.
pub(super) fn next_change(changes: &[u32], a0: i64, white: bool, width: u32) -> (u32, u32) {
    let mut index = 0usize;
    while index < changes.len() {
        match changes.get(index) {
            Some(position) if i64::from(*position) <= a0 => index += 1,
            _ => break,
        }
    }
    if (index % 2 == 0) != white {
        index += 1;
    }
    let first = changes.get(index).copied().unwrap_or(width).min(width);
    let second = changes.get(index + 1).copied().unwrap_or(width).min(width);
    (first, second)
}

/// Writes one row as Modified Huffman runs.
pub(super) fn encode_1d_row(writer: &mut BitWriter, changes: &[u32], width: u32) {
    let mut position = 0u32;
    let mut white = true;
    for change in changes {
        let end = (*change).min(width);
        if end < position {
            continue;
        }
        encode_run(end - position, white, |code, bits| writer.write(code, bits));
        position = end;
        white = !white;
    }
    if position < width || changes.is_empty() {
        encode_run(width - position, white, |code, bits| {
            writer.write(code, bits)
        });
    }
}

/// Writes one row against `reference` using the T.4/T.6 two-dimensional modes.
pub(super) fn encode_2d_row(
    writer: &mut BitWriter,
    changes: &[u32],
    reference: &[u32],
    width: u32,
) {
    let mut a0: i64 = -1;
    let mut white = true;
    while a0 < i64::from(width) {
        let (a1, a2) = next_change(changes, a0, white, width);
        let (b1, b2) = next_change(reference, a0, white, width);
        if b2 < a1 {
            let (code, bits) = mode_bits(Mode::Pass);
            writer.write(code, bits);
            a0 = i64::from(b2);
            continue;
        }
        let delta = i64::from(a1) - i64::from(b1);
        if (-3..=3).contains(&delta) {
            let (code, bits) = mode_bits(Mode::Vertical(delta as i8));
            writer.write(code, bits);
            a0 = i64::from(a1);
            white = !white;
            continue;
        }
        let (code, bits) = mode_bits(Mode::Horizontal);
        writer.write(code, bits);
        let start = a0.max(0) as u32;
        encode_run(a1.saturating_sub(start), white, |code, bits| {
            writer.write(code, bits);
        });
        encode_run(a2.saturating_sub(a1), !white, |code, bits| {
            writer.write(code, bits);
        });
        a0 = i64::from(a2);
    }
}

/// The pixels of one row, `true` for black, from its changing elements.
fn row_pixels(changes: &[u32], width: u32, out: &mut Vec<bool>) {
    out.clear();
    out.resize(width as usize, false);
    let mut black = false;
    let mut position = 0u32;
    for change in changes {
        let end = (*change).min(width);
        if black {
            if let Some(run) = out.get_mut(position as usize..end as usize) {
                run.fill(true);
            }
        }
        position = end;
        black = !black;
    }
    if black {
        if let Some(run) = out.get_mut(position as usize..) {
            run.fill(true);
        }
    }
}

/// Whether a whole row is cheaper in uncompressed mode than in `coded_bits`.
///
/// The comparison is exact rather than heuristic — both sides are counted in
/// bits — because uncompressed mode is a *loss* on ordinary fax content and a
/// win only on dithered or halftoned regions, and guessing wrong makes files
/// bigger with no compensating benefit.
fn uncompressed_is_cheaper(pixels: &[bool], coded_bits: usize, entrance_bits: u8) -> bool {
    usize::from(entrance_bits) + uncompressed::encoded_bits(pixels) < coded_bits
}

/// Writes one row in uncompressed mode: the entrance code, the pixels, and
/// the exit code whose tag bit opens the next line's first (white) run.
///
/// The mode is entered only at the *start* of a row, which is what makes
/// T.4's NOTE 4 hazard structurally impossible: the note warns that a
/// one-dimensional coder must not switch into uncompressed mode after a code
/// word ending in `000`, because those three zeros plus the entrance code's
/// eight would spell the twelve-bit end-of-line code. At a row start the
/// preceding bit is the `1` that ends the previous end-of-line code, or its
/// tag bit, or (in Group 4, which has no end-of-line codes at all) the last
/// bit of the previous row — and no T.4 code word ends in more than three
/// zeros, so a Group 4 row can contribute at most three, giving nine before
/// the two-dimensional entrance code's terminating one. Eleven are needed to
/// look like an end-of-line.
fn write_uncompressed_row(writer: &mut BitWriter, pixels: &[bool], two_dimensional: bool) {
    if two_dimensional {
        writer.write(uncompressed::ENTER_2D_CODE, uncompressed::ENTER_2D_BITS);
    } else {
        writer.write(uncompressed::ENTER_1D_CODE, uncompressed::ENTER_1D_BITS);
    }
    // The row ends here, so the "next run" the tag bit names is the one after
    // the row's last pixel: white, because every line starts white.
    uncompressed::write_pixels(writer, pixels, false);
}

/// Writes one row, choosing uncompressed mode when it is smaller.
///
/// `allowed` is the option-tag bit (`T4Options` bit 1 / `T6Options` bit 1):
/// without it the mode is never written at all, because libtiff refuses to
/// read it and a file that used it uninvited would be unreadable there.
pub(super) fn encode_row(
    writer: &mut BitWriter,
    changes: &[u32],
    reference: &[u32],
    width: u32,
    two_dimensional: bool,
    allowed: bool,
    scratch: &mut RowScratch,
) {
    if !allowed {
        encode_coded_row(writer, changes, reference, width, two_dimensional);
        return;
    }
    // Code the row into the scratch writer to price it, rather than
    // estimating: an estimate that guessed wrong would make the file bigger.
    // No `Vec` is allocated per row — the scratch writer keeps its buffer.
    scratch.probe.clear();
    encode_coded_row(
        &mut scratch.probe,
        changes,
        reference,
        width,
        two_dimensional,
    );
    let coded_bits = scratch.probe.bit_len();
    row_pixels(changes, width, &mut scratch.pixels);
    let entrance_bits = if two_dimensional {
        uncompressed::ENTER_2D_BITS
    } else {
        uncompressed::ENTER_1D_BITS
    };
    if uncompressed_is_cheaper(&scratch.pixels, coded_bits, entrance_bits) {
        write_uncompressed_row(writer, &scratch.pixels, two_dimensional);
    } else {
        encode_coded_row(writer, changes, reference, width, two_dimensional);
    }
}

/// Reusable buffers for [`encode_row`], so the choice costs no allocation
/// per row.
#[derive(Debug, Default)]
pub(super) struct RowScratch {
    /// One entry per pixel, `true` for black.
    pixels: Vec<bool>,
    /// The writer the Huffman coding of a row is priced in.
    probe: BitWriter,
}

/// Writes one row with the ordinary Huffman coding of its dialect.
fn encode_coded_row(
    writer: &mut BitWriter,
    changes: &[u32],
    reference: &[u32],
    width: u32,
    two_dimensional: bool,
) {
    if two_dimensional {
        encode_2d_row(writer, changes, reference, width);
    } else {
        encode_1d_row(writer, changes, width);
    }
}

/// Writes an end-of-line code, optionally padded so it ends on a byte
/// boundary (T.4's `EncodedByteAlign`, libtiff's `FAXMODE_BYTEALIGN`).
pub(super) fn write_eol(writer: &mut BitWriter, byte_align: bool) {
    if byte_align {
        let pad = (8 - ((writer.bit_len() + usize::from(EOL_BITS)) % 8)) % 8;
        writer.write_zeros(pad);
    }
    writer.write(EOL_CODE, EOL_BITS);
}

/// Reads the changing elements out of one packed 1-bit row.
///
/// `white_bit` is the bit value that means white, so the caller's photometric
/// interpretation never leaks into the coder.
pub(super) fn row_changes(row: &[u8], width: u32, white_bit: u8, out: &mut Vec<u32>) {
    out.clear();
    let mut white = true;
    for x in 0..width {
        let byte = row.get((x / 8) as usize).copied().unwrap_or(0);
        let bit = (byte >> (7 - (x % 8))) & 1;
        let is_white = bit == white_bit;
        if is_white != white {
            out.push(x);
            white = is_white;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::decode::FaxDecoder;
    use super::*;

    fn round_trip_1d(pixels: &[bool]) {
        let width = pixels.len() as u32;
        let mut changes = Vec::new();
        let mut white = true;
        for (index, black) in pixels.iter().enumerate() {
            if *black == white {
                changes.push(index as u32);
                white = !white;
            }
        }
        let mut writer = BitWriter::new();
        encode_1d_row(&mut writer, &changes, width);
        let bytes = writer.finish(false);
        let mut decoder = FaxDecoder::new(&bytes, width, false);
        decoder.decode_1d_row().expect("decode");
        let mut row = vec![0u8; (width as usize).div_ceil(8)];
        decoder.paint(&mut row, 0);
        for (index, black) in pixels.iter().enumerate() {
            let bit = (row[index / 8] >> (7 - index % 8)) & 1;
            assert_eq!(bit == 1, *black, "pixel {index} of {width}");
        }
    }

    #[test]
    fn all_white_all_black_and_alternating_rows_round_trip() {
        round_trip_1d(&[false; 64]);
        round_trip_1d(&[true; 64]);
        round_trip_1d(&(0..64).map(|i| i % 2 == 0).collect::<Vec<_>>());
        round_trip_1d(&(0..1).map(|_| true).collect::<Vec<_>>());
        round_trip_1d(&(0..1728 + 100).map(|i| i > 1800).collect::<Vec<_>>());
        round_trip_1d(&(0..3000).map(|i| i > 2900).collect::<Vec<_>>());
    }

    #[test]
    fn row_changes_reads_both_photometrics() {
        // 0b1100_0000: two set bits.
        let row = [0b1100_0000u8];
        let mut changes = Vec::new();
        row_changes(&row, 8, 0, &mut changes);
        assert_eq!(changes, vec![0, 2], "white = 0 means the ones are black");
        row_changes(&row, 8, 1, &mut changes);
        assert_eq!(changes, vec![2], "white = 1 means the zeros are black");
    }

    #[test]
    fn a_byte_aligned_eol_ends_on_a_byte_boundary() {
        let mut writer = BitWriter::new();
        writer.write(0b101, 3);
        write_eol(&mut writer, true);
        assert_eq!(writer.bit_len() % 8, 0);
        let bytes = writer.finish(false);
        let mut decoder = FaxDecoder::new(&bytes, 8, false);
        decoder.bits.skip(3);
        assert!(decoder.consume_eol());
        assert_eq!(decoder.bits.position() % 8, 0);
    }

    #[test]
    fn an_unaligned_eol_is_twelve_bits() {
        let mut writer = BitWriter::new();
        writer.write(0b101, 3);
        write_eol(&mut writer, false);
        assert_eq!(writer.bit_len(), 15);
    }

    #[test]
    fn row_pixels_inverts_row_changes() {
        for row in [
            vec![0b0000_0000u8],
            vec![0b1111_1111],
            vec![0b1010_1010],
            vec![0b1100_0011],
            vec![0b0000_0001, 0b1000_0000],
        ] {
            let width = (row.len() * 8) as u32;
            let mut changes = Vec::new();
            row_changes(&row, width, 0, &mut changes);
            let mut pixels = Vec::new();
            row_pixels(&changes, width, &mut pixels);
            for (index, black) in pixels.iter().enumerate() {
                let bit = (row[index / 8] >> (7 - index % 8)) & 1;
                assert_eq!(bit == 1, *black, "pixel {index} of {row:?}");
            }
        }
    }

    #[test]
    fn uncompressed_mode_is_chosen_only_where_it_wins() {
        let mut scratch = RowScratch::default();
        // A dithered row: every other pixel black, so Modified Huffman spends
        // a whole code word per pixel and uncompressed mode spends one bit.
        let dithered: Vec<u32> = (0..256).collect();
        let mut writer = BitWriter::new();
        encode_row(&mut writer, &dithered, &[], 256, false, true, &mut scratch);
        let with_mode = writer.bit_len();
        let mut writer = BitWriter::new();
        encode_row(&mut writer, &dithered, &[], 256, false, false, &mut scratch);
        let without = writer.bit_len();
        assert!(
            with_mode < without,
            "uncompressed mode must win on dithered data: {with_mode} vs {without}"
        );

        // A long-run row: uncompressed mode would cost a bit per pixel, so
        // it must not be chosen even when it is allowed.
        let plain = [128u32];
        let mut writer = BitWriter::new();
        encode_row(&mut writer, &plain, &[], 256, false, true, &mut scratch);
        let with_mode = writer.bit_len();
        let mut writer = BitWriter::new();
        encode_row(&mut writer, &plain, &[], 256, false, false, &mut scratch);
        let without = writer.bit_len();
        assert_eq!(
            with_mode, without,
            "long runs must stay Huffman-coded even when the mode is allowed"
        );
    }

    #[test]
    fn a_row_written_in_uncompressed_mode_decodes_back_to_itself() {
        let mut scratch = RowScratch::default();
        for width in [1u32, 7, 8, 9, 64, 251] {
            for seed in [1u32, 3, 7] {
                let pixels: Vec<bool> = (0..width).map(|x| (x * seed / 2) % 3 == 0).collect();
                let mut changes = Vec::new();
                let mut black = false;
                for (index, is_black) in pixels.iter().enumerate() {
                    if *is_black != black {
                        changes.push(index as u32);
                        black = *is_black;
                    }
                }
                for two_dimensional in [false, true] {
                    let mut writer = BitWriter::new();
                    write_uncompressed_row(&mut writer, &pixels, two_dimensional);
                    let bytes = writer.finish(false);
                    let mut decoder = FaxDecoder::new(&bytes, width, false);
                    let outcome = if two_dimensional {
                        decoder.decode_2d_row()
                    } else {
                        decoder.decode_1d_row()
                    };
                    outcome.expect("decode an uncompressed row");
                    let mut row = vec![0u8; (width as usize).div_ceil(8)];
                    decoder.paint(&mut row, 0);
                    for (index, want) in pixels.iter().enumerate() {
                        let bit = (row[index / 8] >> (7 - index % 8)) & 1;
                        assert_eq!(
                            bit == 1,
                            *want,
                            "width {width} seed {seed} 2d {two_dimensional} pixel {index}"
                        );
                    }
                }
            }
        }
        // The scratch must be usable afterwards, which is what the encoder
        // relies on.
        let mut writer = BitWriter::new();
        encode_row(&mut writer, &[1], &[], 8, false, true, &mut scratch);
        assert!(writer.bit_len() > 0);
    }

    #[test]
    fn next_change_respects_colour_parity() {
        let changes = [4u32, 9, 20];
        // Starting white before the row: the next change to black is 4.
        assert_eq!(next_change(&changes, -1, true, 32), (4, 9));
        // After 4 the run is black; the next change to white is 9.
        assert_eq!(next_change(&changes, 4, false, 32), (9, 20));
        // Past every change the answers saturate at the row width.
        assert_eq!(next_change(&changes, 25, true, 32), (32, 32));
    }
}
