//! GIF LZW compression and decompression.
//!
//! GIF uses a specific variant of LZW:
//! - LSB-first (Least Significant Bit) bit ordering
//! - Variable initial code size driven by `minimum_lzw_code_size` from the GIF header
//! - Clear code = `1 << minimum_code_size`
//! - End-of-Information (EOI) code = clear code + 1
//! - Dictionary entries start at clear code + 2
//! - Initial code width = `minimum_code_size + 1` bits
//! - Code width grows as the dictionary grows (max 12 bits / 4096 codes)
//! - When dictionary is full, a clear code is emitted and the dictionary resets

use crate::bitstream_lsb::{LsbBitReader, LsbBitWriter};
use crate::error::{LzwError, Result};
use std::collections::HashMap;

// ─────────────────────────────────────────────────────────────────────────────
// Helper types
// ─────────────────────────────────────────────────────────────────────────────

/// Compact GIF LZW state shared between encoder and decoder.
struct GifState {
    /// `1 << minimum_code_size` — the clear code value.
    clear_code: u16,
    /// `clear_code + 1` — the end-of-information code value.
    eoi_code: u16,
    /// `minimum_code_size + 1` — the initial code width in bits.
    initial_width: usize,
    /// Maximum allowed code width (always 12 for GIF).
    max_width: usize,
}

impl GifState {
    fn new(minimum_code_size: u8) -> Self {
        let clear_code = 1u16 << minimum_code_size;
        GifState {
            clear_code,
            eoi_code: clear_code + 1,
            initial_width: minimum_code_size as usize + 1,
            max_width: 12,
        }
    }

    /// First available user dictionary code (clear_code + 2).
    #[inline]
    fn first_code(&self) -> u16 {
        self.eoi_code + 1
    }

    /// Maximum number of codes at max_width bits (4096 for GIF).
    #[inline]
    fn max_codes(&self) -> u16 {
        (1u16 << self.max_width) - 1
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Encoder
// ─────────────────────────────────────────────────────────────────────────────

/// Compress `data` using GIF LZW with the given `minimum_code_size`.
///
/// `minimum_code_size` is the value stored in the GIF Image Descriptor
/// (typically the color depth in bits, e.g. 8 for a 256-colour image).
/// It must satisfy `2 <= minimum_code_size <= 11` (GIF spec §22).
///
/// Returns the raw LZW-compressed byte sequence (without GIF sub-block
/// framing — the caller is responsible for that).
pub fn gif_compress(data: &[u8], minimum_code_size: u8) -> Result<Vec<u8>> {
    if !(2..=11).contains(&minimum_code_size) {
        return Err(LzwError::InvalidBitWidth(minimum_code_size));
    }

    let state = GifState::new(minimum_code_size);
    let mut writer = LsbBitWriter::new();

    // Encoder dictionary: maps byte-string → code.
    let mut enc_dict: HashMap<Vec<u8>, u16> = HashMap::new();
    let mut code_width = state.initial_width;
    let mut next_code = state.first_code();

    /// Reset helper — reinitialises the encoding dictionary with the
    /// single-byte entries and resets code width / next_code.
    fn reset_encoder(
        enc_dict: &mut HashMap<Vec<u8>, u16>,
        code_width: &mut usize,
        next_code: &mut u16,
        state: &GifState,
    ) {
        enc_dict.clear();
        for i in 0..state.clear_code {
            enc_dict.insert(vec![i as u8], i);
        }
        *code_width = state.initial_width;
        *next_code = state.first_code();
    }

    // Initialise dictionary.
    reset_encoder(&mut enc_dict, &mut code_width, &mut next_code, &state);

    // Emit the mandatory clear code at the start.
    writer.write_bits(state.clear_code, code_width);

    if data.is_empty() {
        writer.write_bits(state.eoi_code, code_width);
        return Ok(writer.into_bytes());
    }

    // ── Main encoding loop ──────────────────────────────────────────────────

    // `current` is the string we are currently matching in the dictionary.
    let mut current: Vec<u8> = vec![data[0]];

    for &byte in &data[1..] {
        // Build the extended candidate string.
        let mut candidate = current.clone();
        candidate.push(byte);

        if enc_dict.contains_key(&candidate) {
            // The extended string exists — keep growing.
            current = candidate;
        } else {
            // Emit the code for the currently matched string.
            let code = enc_dict
                .get(&current)
                .copied()
                .ok_or(LzwError::InvalidCode(0))?;
            writer.write_bits(code, code_width);

            // Add the new string to the dictionary if there is room.
            if next_code <= state.max_codes() {
                enc_dict.insert(candidate, next_code);
                next_code += 1;
                // Grow code width when the next code would overflow the current width.
                if next_code > (1u16 << code_width) && code_width < state.max_width {
                    code_width += 1;
                }
            } else {
                // Dictionary is full — emit a clear code and reset.
                writer.write_bits(state.clear_code, code_width);
                reset_encoder(&mut enc_dict, &mut code_width, &mut next_code, &state);
            }

            // Start a new match with the current byte.
            current.clear();
            current.push(byte);
        }
    }

    // Emit the code for the last matched string.
    let last_code = enc_dict
        .get(&current)
        .copied()
        .ok_or(LzwError::InvalidCode(0))?;
    writer.write_bits(last_code, code_width);

    // Emit the EOI code and flush.
    writer.write_bits(state.eoi_code, code_width);

    Ok(writer.into_bytes())
}

// ─────────────────────────────────────────────────────────────────────────────
// Decoder
// ─────────────────────────────────────────────────────────────────────────────

/// Decompress GIF LZW-encoded `data` using the given `minimum_code_size`.
///
/// The `data` parameter must be the raw LZW byte stream (without GIF
/// sub-block framing).
pub fn gif_decompress(data: &[u8], minimum_code_size: u8) -> Result<Vec<u8>> {
    if !(2..=11).contains(&minimum_code_size) {
        return Err(LzwError::InvalidBitWidth(minimum_code_size));
    }

    let state = GifState::new(minimum_code_size);
    let mut reader = LsbBitReader::new(data);

    // Decoder dictionary: maps code → byte string.
    // We use a flat Vec indexed by code for O(1) decode lookups.
    // Codes 0..clear_code are single bytes; we reconstruct them on reset.
    let capacity = (state.max_codes() as usize) + 1;
    let mut dec_dict: Vec<Vec<u8>> = Vec::with_capacity(capacity);
    let mut code_width = state.initial_width;
    let mut next_code: u16 = 0;

    /// Reset the decoding dictionary to the initial single-byte state.
    fn reset_decoder(
        dec_dict: &mut Vec<Vec<u8>>,
        code_width: &mut usize,
        next_code: &mut u16,
        state: &GifState,
    ) {
        dec_dict.clear();
        for i in 0..state.clear_code {
            dec_dict.push(vec![i as u8]);
        }
        // Placeholders for clear code and EOI.
        dec_dict.push(Vec::new()); // clear_code
        dec_dict.push(Vec::new()); // eoi_code
        *code_width = state.initial_width;
        *next_code = state.first_code();
    }

    reset_decoder(&mut dec_dict, &mut code_width, &mut next_code, &state);

    let mut output: Vec<u8> = Vec::new();
    let mut prev_code: Option<u16> = None;

    while let Some(code) = reader.read_bits(code_width) {
        if code == state.clear_code {
            // Clear code: reset the dictionary.
            reset_decoder(&mut dec_dict, &mut code_width, &mut next_code, &state);
            prev_code = None;
            continue;
        }

        if code == state.eoi_code {
            // End of Information: normal termination.
            break;
        }

        // ── Resolve the string for `code` ───────────────────────────────────

        let entry: Vec<u8> = if (code as usize) < dec_dict.len() {
            // Common case: code is already in the dictionary.
            dec_dict[code as usize].clone()
        } else if code == next_code {
            // KwKwK special case: the code refers to the entry we are about
            // to add.  The entry is prev_string + prev_string[0].
            match prev_code {
                Some(pc) => {
                    let prev = dec_dict.get(pc as usize).ok_or(LzwError::InvalidCode(pc))?;
                    let first = *prev.first().ok_or(LzwError::InvalidCode(pc))?;
                    let mut s = prev.clone();
                    s.push(first);
                    s
                }
                None => return Err(LzwError::InvalidCode(code)),
            }
        } else {
            // Code beyond next_code is always an error.
            return Err(LzwError::InvalidCode(code));
        };

        // Output the decoded bytes.
        output.extend_from_slice(&entry);

        // ── Add a new dictionary entry ───────────────────────────────────────
        // New entry = prev_string + entry[0].
        if let Some(pc) = prev_code {
            if next_code <= state.max_codes() {
                let first_byte = *entry.first().ok_or(LzwError::InvalidCode(code))?;
                let prev = dec_dict.get(pc as usize).ok_or(LzwError::InvalidCode(pc))?;
                let mut new_entry = prev.clone();
                new_entry.push(first_byte);

                // The invariant `next_code as usize == dec_dict.len()` must
                // hold at this point; any deviation indicates a logic bug.
                if next_code as usize == dec_dict.len() {
                    dec_dict.push(new_entry);
                } else {
                    // Invariant violated — should never happen.
                    return Err(LzwError::InvalidCode(next_code));
                }

                next_code += 1;

                // Grow code width when needed.
                // The decoder is always one entry behind the encoder, so the
                // decoder must increase the width one step earlier than the
                // encoder.  The encoder fires at `next_code > 2^width`
                // (i.e., next_code == 2^width + 1); the decoder fires at
                // `next_code >= 2^width` (i.e., next_code == 2^width).
                if next_code >= (1u16 << code_width) && code_width < state.max_width {
                    code_width += 1;
                }
            }
            // If next_code > max_codes the dictionary is full; we stop adding
            // entries (clear code will reset when the encoder does the same).
        }

        prev_code = Some(code);
    }

    Ok(output)
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_gif_lzw_classic_string() {
        let data = b"TOBEORNOTTOBEORTOBEORNOT";
        let compressed = gif_compress(data, 8).expect("compress failed");
        let decompressed = gif_decompress(&compressed, 8).expect("decompress failed");
        assert_eq!(decompressed.as_slice(), data.as_slice());
    }

    #[test]
    fn test_gif_lzw_roundtrip() {
        let data = b"TOBEORNOTTOBEORTOBEORNOT";
        let compressed = gif_compress(data, 8).expect("compress failed");
        let decompressed = gif_decompress(&compressed, 8).expect("decompress failed");
        assert_eq!(decompressed.as_slice(), data.as_slice());
    }

    #[test]
    fn test_gif_lzw_empty() {
        let data: &[u8] = b"";
        let compressed = gif_compress(data, 8).expect("compress failed");
        let decompressed = gif_decompress(&compressed, 8).expect("decompress failed");
        assert_eq!(decompressed.as_slice(), data);
    }

    #[test]
    fn test_gif_lzw_single_byte() {
        let data = b"A";
        let compressed = gif_compress(data, 8).expect("compress failed");
        let decompressed = gif_decompress(&compressed, 8).expect("decompress failed");
        assert_eq!(decompressed.as_slice(), data.as_slice());
    }

    #[test]
    fn test_gif_lzw_repeating() {
        let data: Vec<u8> = vec![b'X'; 1000];
        let compressed = gif_compress(&data, 8).expect("compress failed");
        let decompressed = gif_decompress(&compressed, 8).expect("decompress failed");
        assert_eq!(decompressed, data);
    }

    #[test]
    fn test_gif_lzw_all_bytes() {
        let data: Vec<u8> = (0..=255).collect();
        let compressed = gif_compress(&data, 8).expect("compress failed");
        let decompressed = gif_decompress(&compressed, 8).expect("decompress failed");
        assert_eq!(decompressed, data);
    }

    #[test]
    fn test_gif_lzw_large_input() {
        // This exercises dictionary-full reset logic.
        let data = b"The quick brown fox jumps over the lazy dog. ".repeat(100);
        let compressed = gif_compress(&data, 8).expect("compress failed");
        let decompressed = gif_decompress(&compressed, 8).expect("decompress failed");
        assert_eq!(decompressed, data);
    }

    #[test]
    fn test_gif_lzw_min_code_size_2() {
        // Small minimum code size (e.g. 1-bit colour GIF uses min_code_size=2)
        let data: Vec<u8> = (0..4).cycle().take(200).collect();
        let compressed = gif_compress(&data, 2).expect("compress failed");
        let decompressed = gif_decompress(&compressed, 2).expect("decompress failed");
        assert_eq!(decompressed, data);
    }

    #[test]
    fn test_gif_lzw_310_bytes() {
        let data = b"This is a test of compression! ".repeat(10);
        assert_eq!(data.len(), 310);
        let compressed = gif_compress(&data, 8).expect("compress failed");
        let decompressed = gif_decompress(&compressed, 8).expect("decompress failed");
        assert_eq!(decompressed.len(), 310);
        assert_eq!(decompressed.as_slice(), data.as_slice());
    }
}
