//! LZW encoder (compression).

use crate::bitstream_msb::MsbBitWriter;
use crate::config::LzwConfig;
use crate::dictionary::{LzwCodeIndex, LzwDictionary};
use crate::error::Result;

/// LZW encoder for compression.
#[derive(Debug)]
pub struct LzwEncoder {
    /// Shared prefix/suffix code table.
    dict: LzwDictionary,
    /// `(prefix code, byte) -> code` lookup over that table.
    index: LzwCodeIndex,
}

impl LzwEncoder {
    /// Create a new LZW encoder with the given configuration.
    ///
    /// # Errors
    ///
    /// Returns [`crate::LzwError::InvalidBitWidth`] when `config` fails
    /// [`LzwConfig::validate`].
    pub fn new(config: LzwConfig) -> Result<Self> {
        let dict = LzwDictionary::new(config)?;
        let index = LzwCodeIndex::with_capacity(dict.capacity());
        Ok(Self { dict, index })
    }

    /// Encode data with LZW compression.
    ///
    /// # Algorithm
    ///
    /// The LZW encoding algorithm (TIFF 6.0 §13 / libtiff-compatible):
    /// 1. Initialize dictionary with single-byte codes (0-255)
    /// 2. Emit a ClearCode (256) as the very first code (when clear codes
    ///    are enabled — TIFF 6.0 mandates this for every strip)
    /// 3. Read input byte by byte, extending the current match while
    ///    `(current code, next byte)` is already in the table
    /// 4. Output the code for that match and add `match + next byte` to
    ///    the dictionary
    /// 5. When the table reaches entry 4093 (`next_code` == 4094), emit a
    ///    ClearCode and reset the table (early-change/TIFF mode), matching
    ///    libtiff's `CODE_MAX - 1` reset
    /// 6. Output the code for the final match, account for the decoder's
    ///    phantom final table entry, and output EOI (257)
    ///
    /// Matches are tracked as *codes*, never as owned byte strings, so the
    /// encoder allocates nothing per input byte.
    ///
    /// # Parameters
    ///
    /// - `input`: Data to compress
    ///
    /// # Returns
    ///
    /// LZW-compressed byte sequence.
    ///
    /// # Errors
    ///
    /// Returns [`crate::LzwError::InvalidBitWidth`] if the configured code
    /// width is out of range for the bit writer, or an I/O error from the
    /// bit writer.
    pub fn encode(&mut self, input: &[u8]) -> Result<Vec<u8>> {
        // Always start from a clean dictionary so a reused encoder produces
        // an independently decodable stream.
        self.dict.reset();
        self.index.clear();

        let mut writer = MsbBitWriter::new();
        let use_clear_code = self.dict.config().use_clear_code;
        let clear_code = self.dict.clear_code();
        let reset_trigger = self.reset_trigger();

        // TIFF 6.0: every LZW strip must begin with a ClearCode.
        if use_clear_code {
            writer.write_bits(clear_code, self.dict.current_bits())?;
        }

        let Some((&first, rest)) = input.split_first() else {
            // Empty input - just write EOI
            writer.write_bits(self.dict.eoi_code(), self.dict.current_bits())?;
            return writer.into_vec();
        };

        // Code of the string matched so far. Every byte is a root code, so
        // this is always a valid table entry.
        let mut current = u16::from(first);

        for &byte in rest {
            if let Some(code) = self.index.find(current, byte) {
                // The extended string exists in the dictionary - keep going.
                current = code;
                continue;
            }

            // The extended string is new: emit the match we had, learn the
            // extension, and restart the match at `byte`.
            writer.write_bits(current, self.dict.current_bits())?;

            if !self.dict.is_full() {
                let code = self.dict.add_entry_encode(current, byte)?;
                self.index.insert(current, byte, code);
            }

            // Table-reset handling (only meaningful with clear codes).
            if use_clear_code && self.dict.next_code() >= reset_trigger {
                // Emit a ClearCode at the current width, then reset the
                // table and drop back to the minimum code width.
                writer.write_bits(clear_code, self.dict.current_bits())?;
                self.dict.reset();
                self.index.clear();
            }

            current = u16::from(byte);
        }

        // Output code for the final match.
        writer.write_bits(current, self.dict.current_bits())?;

        // The decoder creates one more table entry while processing that
        // final code; mirror it (libtiff `LZWPostEncode` does the same) so
        // the EOI code below is written at the width the decoder will use
        // to read it. At exact boundary sizes this also means the phantom
        // entry can hit the reset trigger, in which case libtiff emits a
        // ClearCode before EOI — mirror that too.
        self.dict.note_final_code();
        if use_clear_code && self.dict.next_code() >= reset_trigger {
            writer.write_bits(clear_code, self.dict.current_bits())?;
            self.dict.reset();
            self.index.clear();
        }

        // Write EOI code
        writer.write_bits(self.dict.eoi_code(), self.dict.current_bits())?;

        // Flush remaining bits
        writer.into_vec()
    }

    /// The `next_code` value at which the encoder emits a ClearCode and
    /// resets the table.
    ///
    /// - Early-change (TIFF): `max_code - 1` (4094), matching libtiff's
    ///   `free_ent == CODE_MAX - 1` reset — entries 4094/4095 are never used.
    /// - Standard change: only once the table is completely full
    ///   (`max_code + 1`), preserving the previous GIF-style behaviour of
    ///   this generic encoder (the real GIF path lives in `gif_lzw`).
    fn reset_trigger(&self) -> u16 {
        let config = self.dict.config();
        if config.early_change {
            config.max_code().saturating_sub(1)
        } else {
            config.max_code().saturating_add(1)
        }
    }

    /// Reset the encoder to initial state.
    pub fn reset(&mut self) {
        self.dict.reset();
        self.index.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decoder::LzwDecoder;

    #[test]
    fn test_encode_simple() {
        let config = LzwConfig::TIFF;
        let mut encoder = LzwEncoder::new(config).expect("create lzw encoder");

        let original = b"TOBEORNOTTOBEORTOBEORNOT";
        let compressed = encoder.encode(original).expect("lzw encode simple");

        // Compressed should be smaller (or at least not much larger)
        // For this highly repetitive string, compression should be effective
        assert!(compressed.len() < original.len() * 2);

        // Verify round-trip
        let mut decoder = LzwDecoder::new(config).expect("create lzw decoder");
        let decompressed = decoder
            .decode(&compressed, original.len())
            .expect("lzw decode simple");
        assert_eq!(decompressed, original);
    }

    #[test]
    fn test_encode_310_bytes() {
        let config = LzwConfig::TIFF;
        let mut encoder = LzwEncoder::new(config).expect("create lzw encoder 310");

        let original = b"This is a test of compression! ".repeat(10);
        assert_eq!(original.len(), 310);

        let compressed = encoder.encode(&original).expect("lzw encode 310 bytes");

        // Verify round-trip
        let mut decoder = LzwDecoder::new(config).expect("create lzw decoder 310");
        let decompressed = decoder
            .decode(&compressed, original.len())
            .expect("lzw decode 310 bytes");
        assert_eq!(decompressed.len(), 310);
        assert_eq!(decompressed, &original[..]);
    }

    #[test]
    fn test_encode_empty() {
        let config = LzwConfig::TIFF;
        let mut encoder = LzwEncoder::new(config).expect("create lzw encoder empty");

        let original = b"";
        let compressed = encoder.encode(original).expect("lzw encode empty");

        // Should contain at least EOI code
        assert!(!compressed.is_empty());

        // Verify round-trip
        let mut decoder = LzwDecoder::new(config).expect("create lzw decoder empty");
        let decompressed = decoder.decode(&compressed, 0).expect("lzw decode empty");
        assert_eq!(decompressed, original);
    }

    #[test]
    fn test_encode_single_byte() {
        let config = LzwConfig::TIFF;
        let mut encoder = LzwEncoder::new(config).expect("create lzw encoder single byte");

        let original = b"A";
        let compressed = encoder.encode(original).expect("lzw encode single byte");

        // Verify round-trip
        let mut decoder = LzwDecoder::new(config).expect("create lzw decoder single byte");
        let decompressed = decoder
            .decode(&compressed, original.len())
            .expect("lzw decode single byte");
        assert_eq!(decompressed, original);
    }

    #[test]
    fn test_encode_repeating() {
        let config = LzwConfig::TIFF;
        let mut encoder = LzwEncoder::new(config).expect("create lzw encoder repeating");

        let original = vec![b'X'; 500];
        let compressed = encoder.encode(&original).expect("lzw encode repeating");

        // Highly repetitive data should compress well
        assert!(compressed.len() < original.len() / 2);

        // Verify round-trip
        let mut decoder = LzwDecoder::new(config).expect("create lzw decoder repeating");
        let decompressed = decoder
            .decode(&compressed, original.len())
            .expect("lzw decode repeating");
        assert_eq!(decompressed, original);
    }

    #[test]
    fn test_encode_all_bytes() {
        let config = LzwConfig::TIFF;
        let mut encoder = LzwEncoder::new(config).expect("create lzw encoder all bytes");

        // Test with all possible byte values
        let original: Vec<u8> = (0..=255).collect();
        let compressed = encoder.encode(&original).expect("lzw encode all bytes");

        // Verify round-trip
        let mut decoder = LzwDecoder::new(config).expect("create lzw decoder all bytes");
        let decompressed = decoder
            .decode(&compressed, original.len())
            .expect("lzw decode all bytes");
        assert_eq!(decompressed, original);
    }

    #[test]
    fn test_encode_alternating() {
        let config = LzwConfig::TIFF;
        let mut encoder = LzwEncoder::new(config).expect("create lzw encoder alternating");

        let original = b"ABABABABABABABABAB";
        let compressed = encoder.encode(original).expect("lzw encode alternating");

        // Verify round-trip
        let mut decoder = LzwDecoder::new(config).expect("create lzw decoder alternating");
        let decompressed = decoder
            .decode(&compressed, original.len())
            .expect("lzw decode alternating");
        assert_eq!(decompressed, original);
    }

    #[test]
    fn test_encoder_reuse_matches_fresh_encoder() {
        // A reused encoder must produce byte-identical output to a fresh
        // one: `encode` resets both the table and the lookup index.
        let config = LzwConfig::TIFF;
        let inputs: [Vec<u8>; 3] = [
            b"TOBEORNOTTOBEORTOBEORNOT".to_vec(),
            vec![b'Z'; 9000],
            (0..20_000u32).map(|i| (i % 251) as u8).collect(),
        ];

        let mut reused = LzwEncoder::new(config).expect("create reused encoder");
        for input in &inputs {
            let from_reused = reused.encode(input).expect("reused encode");
            let mut fresh = LzwEncoder::new(config).expect("create fresh encoder");
            let from_fresh = fresh.encode(input).expect("fresh encode");
            assert_eq!(from_reused, from_fresh);
        }
    }
}
