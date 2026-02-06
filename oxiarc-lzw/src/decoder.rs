//! LZW decoder (decompression).
//!
//! This module implements LZW decompression with proper loop termination
//! to fix the truncation bug found in weezl.

use crate::bitstream_msb::MsbBitReader;
use crate::config::LzwConfig;
use crate::dictionary::LzwDictionary;
use crate::error::{LzwError, Result};

/// LZW decoder for decompression.
#[derive(Debug)]
pub struct LzwDecoder {
    /// Dictionary for code lookup.
    dict: LzwDictionary,
}

impl LzwDecoder {
    /// Create a new LZW decoder with the given configuration.
    pub fn new(config: LzwConfig) -> Result<Self> {
        let dict = LzwDictionary::new(config)?;
        Ok(Self { dict })
    }

    /// Decode LZW-compressed data.
    ///
    /// # Critical Fix
    ///
    /// This implementation fixes the weezl truncation bug by ensuring the loop
    /// continues until one of these conditions is met:
    /// - EOI (End of Information) code is encountered
    /// - Expected size is reached
    /// - Input is exhausted
    ///
    /// The weezl decoder had a bug where it would terminate early, causing
    /// truncation (e.g., 310 bytes truncated to ~250 bytes).
    ///
    /// # Parameters
    ///
    /// - `input`: LZW-compressed data
    /// - `expected_size`: Expected size of decompressed output
    ///
    /// # Returns
    ///
    /// Decompressed byte sequence of exactly `expected_size` bytes (or less if
    /// EOI code is encountered early).
    pub fn decode(&mut self, input: &[u8], expected_size: usize) -> Result<Vec<u8>> {
        let mut reader = MsbBitReader::new(input);
        let mut output = Vec::with_capacity(expected_size);

        // Previous code (for dictionary building)
        let mut prev_code: Option<u16> = None;

        // Decode until we reach expected size or encounter EOI
        while output.len() < expected_size {
            // Read next code with current bit width
            let code = match reader.read_bits(self.dict.current_bits()) {
                Ok(c) => c,
                Err(LzwError::UnexpectedEof { .. }) => {
                    // Input exhausted - this is OK if we have all expected bytes
                    if output.len() >= expected_size {
                        break;
                    }
                    // Otherwise, we have incomplete data
                    return Err(LzwError::UnexpectedEof {
                        position: reader.bits_read(),
                    });
                }
                Err(e) => return Err(e),
            };

            // Handle special codes
            if code == self.dict.clear_code() {
                // Clear code: reset dictionary
                if self.dict.config().use_clear_code {
                    self.dict.reset();
                    prev_code = None;
                    continue;
                } else {
                    // TIFF doesn't use clear codes in the stream
                    return Err(LzwError::InvalidClearCode {
                        position: reader.bits_read(),
                    });
                }
            }

            if code == self.dict.eoi_code() {
                // End of information - stop decoding
                break;
            }

            // Get the byte sequence for this code
            let string = if code < self.dict.next_code() {
                // Code exists in dictionary - this is the common case
                self.dict.get_string(code)?.to_vec()
            } else if code == self.dict.next_code() {
                // Special case: code not yet in dictionary
                // This happens when we have a pattern like "ABABAB..."
                // The string is: prev_string + prev_string[0]
                if let Some(prev) = prev_code {
                    let prev_string = self.dict.get_string(prev)?;
                    let mut new_string = prev_string.to_vec();
                    new_string.push(prev_string[0]);
                    new_string
                } else {
                    return Err(LzwError::InvalidCode(code));
                }
            } else {
                // Code is beyond next_code - this is an error
                return Err(LzwError::InvalidCode(code));
            };

            // Output the decoded string
            output.extend_from_slice(&string);

            // Add new entry to dictionary (if we have a previous code)
            if let Some(prev) = prev_code
                && !self.dict.is_full()
            {
                let prev_string = self.dict.get_string(prev)?;
                let mut new_entry = prev_string.to_vec();
                new_entry.push(string[0]);

                // Only add if table not full
                if !self.dict.is_full() {
                    let _ = self.dict.add_string_decode(new_entry);
                }
            }

            // Update previous code
            prev_code = Some(code);
        }

        // Truncate to expected size if we decoded more than needed
        // This can happen if the last code expands beyond expected_size
        if output.len() > expected_size {
            output.truncate(expected_size);
        }

        Ok(output)
    }

    /// Reset the decoder to initial state.
    pub fn reset(&mut self) {
        self.dict.reset();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encoder::LzwEncoder;

    #[test]
    fn test_decode_simple() {
        // Manually create a simple LZW stream
        // This is "TOBEORNOTTOBEORTOBEORNOT" compressed
        let config = LzwConfig::TIFF;
        let mut decoder = LzwDecoder::new(config).unwrap();

        // For this test, we'll use the encoder to create valid data
        let original = b"TOBEORNOTTOBEORTOBEORNOT";
        let mut encoder = LzwEncoder::new(config).unwrap();
        let compressed = encoder.encode(original).unwrap();

        // Now decode it
        let decompressed = decoder.decode(&compressed, original.len()).unwrap();

        assert_eq!(decompressed, original);
    }

    #[test]
    fn test_decode_310_bytes() {
        // THE CRITICAL TEST - this must not truncate!
        let config = LzwConfig::TIFF;
        let mut decoder = LzwDecoder::new(config).unwrap();

        let original = b"This is a test of compression! ".repeat(10);
        assert_eq!(original.len(), 310);

        // Encode it first
        let mut encoder = LzwEncoder::new(config).unwrap();
        let compressed = encoder.encode(&original).unwrap();

        // Decode it
        let decompressed = decoder.decode(&compressed, original.len()).unwrap();

        // CRITICAL: Must be 310 bytes, not ~250!
        assert_eq!(
            decompressed.len(),
            310,
            "Decompressed length must be 310, not truncated!"
        );
        assert_eq!(decompressed, &original[..]);
    }

    #[test]
    fn test_decode_repeating_pattern() {
        let config = LzwConfig::TIFF;
        let mut decoder = LzwDecoder::new(config).unwrap();

        let original = b"ABABABABABABABABAB";

        let mut encoder = LzwEncoder::new(config).unwrap();
        let compressed = encoder.encode(original).unwrap();

        let decompressed = decoder.decode(&compressed, original.len()).unwrap();

        assert_eq!(decompressed, original);
    }

    #[test]
    fn test_decode_single_byte() {
        let config = LzwConfig::TIFF;
        let mut decoder = LzwDecoder::new(config).unwrap();

        let original = b"A";

        let mut encoder = LzwEncoder::new(config).unwrap();
        let compressed = encoder.encode(original).unwrap();

        let decompressed = decoder.decode(&compressed, original.len()).unwrap();

        assert_eq!(decompressed, original);
    }

    #[test]
    fn test_decode_all_same() {
        let config = LzwConfig::TIFF;
        let mut decoder = LzwDecoder::new(config).unwrap();

        let original = vec![b'X'; 500];

        let mut encoder = LzwEncoder::new(config).unwrap();
        let compressed = encoder.encode(&original).unwrap();

        let decompressed = decoder.decode(&compressed, original.len()).unwrap();

        assert_eq!(decompressed, original);
    }
}
