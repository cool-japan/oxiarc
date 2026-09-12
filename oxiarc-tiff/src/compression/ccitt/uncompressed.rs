//! T.4 §4.2.1.3.2 uncompressed mode, decode and encode.
//!
//! # What the mode is for
//!
//! Modified Huffman coding costs at least four bits per run, so a region of
//! isolated pixels — dithered artwork, halftones, a scan of a photograph —
//! *expands* under Group 3 or Group 4. T.4 answers that with a mode that
//! transmits the pixels themselves, at close to one bit per pixel, and
//! `T4Options` (292) bit 1 / `T6Options` (293) bit 1 declare that a file may
//! use it.
//!
//! # The code table (Table 5/T.4, ITU-T T.4 (07/2003))
//!
//! Entered by an extension code word — `0000001` + `111` on a
//! two-dimensionally coded line, `000000001` + `111` on a one-dimensionally
//! coded one — after which the row is spelled out with these:
//!
//! | image pattern | code word |
//! |---|---|
//! | `1` | `1` |
//! | `01` | `01` |
//! | `001` | `001` |
//! | `0001` | `0001` |
//! | `00001` | `00001` |
//! | `00000` | `000001` |
//!
//! and left with one of these, `T` being a tag bit that gives the colour of
//! the run that follows (black = 1, white = 0):
//!
//! | image pattern | code word |
//! |---|---|
//! | (nothing) | `0000001T` |
//! | `0` | `00000001T` |
//! | `00` | `000000001T` |
//! | `000` | `0000000001T` |
//! | `0000` | `00000000001T` |
//!
//! In the image patterns `1` is a black picture element and `0` a white one,
//! the same convention the exit tag bit uses.
//!
//! Read as one rule rather than eleven rows: **count the zeros before the next
//! one bit.** Zero to four of them are white pixels followed by a black one;
//! five are five white pixels (the sixth bit is an escape, which is what keeps
//! six consecutive zeros out of the data and free to mean "exit"); six to ten
//! are an exit carrying `z - 6` trailing white pixels and then the tag bit.
//! Eleven would collide with the end-of-line code, so it is a stream defect.
//!
//! # Interoperability
//!
//! libtiff 4.7.1 **rejects** uncompressed mode on read (`Fax3Decode2D` reports
//! `Uncompressed data (not supported)`), and no encoder in the wild emits it,
//! which is why this crate never writes it unless asked:
//! [`ImageSpec::with_ccitt_uncompressed`](crate::ImageSpec::with_ccitt_uncompressed)
//! is the opt-in, and it sets the option-tag bit that tells a reader the file
//! may contain the mode. Decoding it costs nothing and is unambiguous, so it
//! is always accepted — a reader that refused valid data because tag 292 was
//! left out would be strictly worse than one that did not.

use super::bits::{BitReader, BitWriter};

/// The extension code that enters uncompressed mode from a 2D-coded line.
///
/// `0000001` (the 2D extension prefix) followed by `111`.
pub(super) const ENTER_2D_CODE: u16 = 0b00_0000_1111;
/// Bit length of [`ENTER_2D_CODE`].
pub(super) const ENTER_2D_BITS: u8 = 10;
/// The extension code that enters uncompressed mode from a 1D-coded line.
///
/// `000000001` (the 1D extension prefix) followed by `111`.
pub(super) const ENTER_1D_CODE: u16 = 0b0000_0000_1111;
/// Bit length of [`ENTER_1D_CODE`].
pub(super) const ENTER_1D_BITS: u8 = 12;

/// The most zeros a code word of this mode may carry: `00000000001T`.
const MAX_ZEROS: u8 = 10;
/// Zeros before a one bit, at or above which the word is an exit code.
const EXIT_ZEROS: u8 = 6;

/// One decoded code word.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Word {
    /// `white` white pixels followed by one black pixel.
    Pixels {
        /// Leading white pixels, `0..=4`.
        white: u8,
    },
    /// Five white pixels and nothing else (`000001`).
    FiveWhite,
    /// Leave the mode after `white` more white pixels.
    Exit {
        /// Trailing white pixels, `0..=4`.
        white: u8,
        /// The tag bit: `true` when the next run is black.
        next_is_black: bool,
    },
    /// More than ten zeros: not a code word of this mode.
    Invalid,
    /// The reader ran out of bits inside a code word.
    Truncated,
}

impl Word {
    /// Bits this word occupies, once its shape is known.
    const fn bits(self, zeros: u8) -> u8 {
        match self {
            // The zeros, the one that ends them, and the tag bit.
            Self::Exit { .. } => zeros + 2,
            // The zeros and the one that ends them.
            _ => zeros + 1,
        }
    }
}

/// Reads one code word without consuming it, returning it and its bit length.
///
/// Splitting "what is it" from "consume it" keeps the encoder's round-trip
/// test able to decode a word out of a buffer it built by hand.
pub(super) fn peek_word(bits: &BitReader<'_>) -> (Word, u8) {
    // Twelve bits cover the longest word, `00000000001T`.
    let window = bits.peek(12);
    let available = bits.remaining();
    if available == 0 {
        return (Word::Truncated, 0);
    }
    let mut zeros = 0u8;
    while zeros <= MAX_ZEROS && (window >> (11 - u32::from(zeros))) & 1 == 0 {
        zeros += 1;
    }
    if zeros > MAX_ZEROS {
        // Eleven zeros in the middle of a chunk is a stream defect — the
        // end-of-line code belongs to the row layer, never here. At the end
        // of one it is the reader's own zero padding, which is not.
        return (
            if available < 12 {
                Word::Truncated
            } else {
                Word::Invalid
            },
            0,
        );
    }
    let word = match zeros {
        0..=4 => Word::Pixels { white: zeros },
        5 => Word::FiveWhite,
        _ => Word::Exit {
            white: zeros - EXIT_ZEROS,
            next_is_black: (window >> (11 - u32::from(zeros) - 1)) & 1 == 1,
        },
    };
    let length = word.bits(zeros);
    if available < usize::from(length) {
        // The word runs past the last byte of the chunk.
        return (Word::Truncated, 0);
    }
    (word, length)
}

/// Consumes one code word and returns it.
pub(super) fn read_word(bits: &mut BitReader<'_>) -> Word {
    let (word, length) = peek_word(bits);
    bits.skip(usize::from(length));
    word
}

/// Writes the pixels of one row as uncompressed-mode code words.
///
/// `pixels` is one entry per pixel, `true` for black. The entrance code is
/// **not** written — the caller knows whether the line is one- or
/// two-dimensionally coded — but the exit code is, with `next_is_black` in
/// its tag bit.
///
/// The escape rule is what makes the output decodable: a run of white pixels
/// is emitted five at a time as `000001`, so the data can never contain the
/// six consecutive zeros that open an exit code.
pub(super) fn write_pixels(writer: &mut BitWriter, pixels: &[bool], next_is_black: bool) {
    let mut white_run = 0u8;
    for black in pixels {
        if *black {
            // `1`, `01`, `001`, `0001` or `00001`: the pending white pixels
            // and the black one, in one code word.
            writer.write(1, white_run + 1);
            white_run = 0;
            continue;
        }
        white_run += 1;
        if white_run == 5 {
            // `000001`: five white pixels, no black one.
            writer.write(1, 6);
            white_run = 0;
        }
    }
    // The exit code carries the last `0..=4` white pixels itself.
    let code = (1u16 << 1) | u16::from(next_is_black);
    writer.write(code, EXIT_ZEROS + white_run + 2);
}

/// Bits [`write_pixels`] would produce, without producing them.
///
/// The encoder compares this with the Huffman coding of the same row, so it
/// has to be exact rather than an estimate.
pub(super) fn encoded_bits(pixels: &[bool]) -> usize {
    let mut total = 0usize;
    let mut white_run = 0usize;
    for black in pixels {
        if *black {
            total += white_run + 1;
            white_run = 0;
            continue;
        }
        white_run += 1;
        if white_run == 5 {
            total += 6;
            white_run = 0;
        }
    }
    total + usize::from(EXIT_ZEROS) + white_run + 2
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The code words of Table 5/T.4, spelled out from the Recommendation.
    ///
    /// Spec-derived: these are the table's own bit strings, not something a
    /// round trip through this module produced.
    fn spec_table() -> Vec<(&'static str, Word)> {
        vec![
            ("1", Word::Pixels { white: 0 }),
            ("01", Word::Pixels { white: 1 }),
            ("001", Word::Pixels { white: 2 }),
            ("0001", Word::Pixels { white: 3 }),
            ("00001", Word::Pixels { white: 4 }),
            ("000001", Word::FiveWhite),
            (
                "00000010",
                Word::Exit {
                    white: 0,
                    next_is_black: false,
                },
            ),
            (
                "00000011",
                Word::Exit {
                    white: 0,
                    next_is_black: true,
                },
            ),
            (
                "000000010",
                Word::Exit {
                    white: 1,
                    next_is_black: false,
                },
            ),
            (
                "000000011",
                Word::Exit {
                    white: 1,
                    next_is_black: true,
                },
            ),
            (
                "0000000011",
                Word::Exit {
                    white: 2,
                    next_is_black: true,
                },
            ),
            (
                "00000000011",
                Word::Exit {
                    white: 3,
                    next_is_black: true,
                },
            ),
            (
                "000000000011",
                Word::Exit {
                    white: 4,
                    next_is_black: true,
                },
            ),
        ]
    }

    /// Packs a bit string into bytes, MSB first, zero-padded.
    fn pack(bits: &str) -> Vec<u8> {
        let mut writer = BitWriter::new();
        for bit in bits.chars() {
            writer.write(u16::from(bit == '1'), 1);
        }
        writer.finish(false)
    }

    #[test]
    fn every_spec_code_word_decodes_to_its_table_row() {
        for (spelling, want) in spec_table() {
            // Padded with a one bit so the word is not the end of the buffer,
            // which would make the reader's zero padding part of the answer.
            let bytes = pack(&format!("{spelling}1111111111111111"));
            let mut reader = BitReader::new(&bytes, false);
            let (got, length) = peek_word(&reader);
            assert_eq!(got, want, "{spelling}");
            assert_eq!(
                usize::from(length),
                spelling.len(),
                "{spelling}: bit length"
            );
            assert_eq!(read_word(&mut reader), want, "{spelling}");
            assert_eq!(reader.position(), spelling.len(), "{spelling}: consumed");
        }
    }

    #[test]
    fn eleven_zeros_are_not_a_code_word() {
        // Twelve zeros then a one is the end-of-line code, which is exactly
        // why the mode's own words stop at ten zeros.
        let bytes = pack("0000000000011111");
        let reader = BitReader::new(&bytes, false);
        assert_eq!(peek_word(&reader).0, Word::Invalid);
    }

    #[test]
    fn a_word_cut_short_by_the_end_of_the_chunk_is_truncated() {
        // No bits at all.
        let reader = BitReader::new(&[], false);
        assert_eq!(peek_word(&reader).0, Word::Truncated);

        // One byte of zeros: eleven zeros would be a defect *inside* a chunk,
        // but here they are the byte's own padding, so the honest answer is
        // "the chunk ended", not "the stream is broken".
        let bytes = pack("00000000");
        let reader = BitReader::new(&bytes, false);
        assert_eq!(peek_word(&reader).0, Word::Truncated);

        // A word whose tag bit is past the last byte. `0000000011` is ten
        // bits; after the six leading ones only nine of the buffer's sixteen
        // are left, so the word cannot be complete.
        let bytes = pack("1111111000000001");
        let mut reader = BitReader::new(&bytes, false);
        reader.skip(7);
        assert_eq!(peek_word(&reader).0, Word::Truncated);

        // One more bit of buffer and the same word is whole.
        let bytes = pack("11111110000000011111111");
        let mut reader = BitReader::new(&bytes, false);
        reader.skip(7);
        assert_eq!(
            peek_word(&reader).0,
            Word::Exit {
                white: 2,
                next_is_black: true
            }
        );
    }

    #[test]
    fn the_escape_keeps_six_zeros_out_of_the_data() {
        // Forty white pixels: eight `000001` words and no run of six zeros
        // anywhere before the exit code.
        let mut writer = BitWriter::new();
        write_pixels(&mut writer, &[false; 40], false);
        let bits = writer.bit_len();
        let bytes = writer.finish(false);
        let spelling: String = (0..bits)
            .map(|index| {
                let byte = bytes[index / 8];
                if (byte >> (7 - index % 8)) & 1 == 1 {
                    '1'
                } else {
                    '0'
                }
            })
            .collect();
        assert_eq!(spelling, "000001".repeat(8) + "00000010");
        assert!(
            !spelling[..48].contains("000000"),
            "the data must not contain six zeros: {spelling}"
        );
    }

    #[test]
    fn the_bit_count_matches_what_is_written() {
        for pattern in [
            vec![],
            vec![true],
            vec![false],
            vec![false; 4],
            vec![false; 5],
            vec![false; 9],
            (0..64).map(|i| i % 3 == 0).collect(),
            (0..97).map(|i| i % 7 < 2).collect(),
            vec![true; 33],
        ] {
            for next_is_black in [false, true] {
                let mut writer = BitWriter::new();
                write_pixels(&mut writer, &pattern, next_is_black);
                assert_eq!(
                    writer.bit_len(),
                    encoded_bits(&pattern),
                    "{} pixels, next black {next_is_black}",
                    pattern.len()
                );
            }
        }
    }

    #[test]
    fn written_pixels_read_back_one_word_at_a_time() {
        let pattern: Vec<bool> = (0..200).map(|i| (i * i / 7) % 5 == 0).collect();
        let mut writer = BitWriter::new();
        write_pixels(&mut writer, &pattern, true);
        let bytes = writer.finish(false);
        let mut reader = BitReader::new(&bytes, false);
        let mut got: Vec<bool> = Vec::new();
        loop {
            match read_word(&mut reader) {
                Word::Pixels { white } => {
                    got.extend(core::iter::repeat_n(false, usize::from(white)));
                    got.push(true);
                }
                Word::FiveWhite => got.extend(core::iter::repeat_n(false, 5)),
                Word::Exit {
                    white,
                    next_is_black,
                } => {
                    got.extend(core::iter::repeat_n(false, usize::from(white)));
                    assert!(next_is_black, "the tag bit must survive");
                    break;
                }
                other => panic!("unexpected word {other:?}"),
            }
        }
        assert_eq!(got, pattern);
    }
}
