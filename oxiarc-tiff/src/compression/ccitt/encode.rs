//! CCITT encoders: Modified Huffman rows, Group 3 one- and two-dimensional
//! coding, and Group 4.
//!
//! The encoders work from the same changing-element representation the decoder
//! produces, so a round trip through this module is a round trip through one
//! model of a row rather than through two independent ones.

use super::bits::BitWriter;
use super::tables::{EOL_BITS, EOL_CODE, Mode, encode_run, mode_bits};

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
