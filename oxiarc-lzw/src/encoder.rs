//! LZW encoder (compression).

use crate::bitstream_msb::MsbBitWriter;
use crate::config::LzwConfig;
use crate::dictionary::LzwDictionary;
use crate::error::Result;

/// LZW encoder for compression.
#[derive(Debug)]
pub struct LzwEncoder {
    /// Dictionary for string lookup.
    dict: LzwDictionary,
}

impl LzwEncoder {
    /// Create a new LZW encoder with the given configuration.
    pub fn new(config: LzwConfig) -> Result<Self> {
        let dict = LzwDictionary::new(config)?;
        Ok(Self { dict })
    }

    /// Encode data with LZW compression.
    ///
    /// # Algorithm
    ///
    /// The LZW encoding algorithm:
    /// 1. Initialize dictionary with single-byte codes (0-255)
    /// 2. Read input byte by byte
    /// 3. Build longest matching string in dictionary
    /// 4. Output code for that string
    /// 5. Add string + next byte to dictionary
    /// 6. Repeat until all input processed
    /// 7. Output EOI (End of Information) code
    ///
    /// # Parameters
    ///
    /// - `input`: Data to compress
    ///
    /// # Returns
    ///
    /// LZW-compressed byte sequence.
    pub fn encode(&mut self, input: &[u8]) -> Result<Vec<u8>> {
        let mut writer = MsbBitWriter::new();

        // Write clear code at start (if enabled)
        if self.dict.config().use_clear_code {
            writer.write_bits(self.dict.clear_code(), self.dict.current_bits())?;
        }

        if input.is_empty() {
            // Empty input - just write EOI
            writer.write_bits(self.dict.eoi_code(), self.dict.current_bits())?;
            return writer.into_vec();
        }

        // Current string being built
        let mut current = vec![input[0]];

        // Process each byte
        for &byte in &input[1..] {
            // Try to extend current string
            let mut candidate = current.clone();
            candidate.push(byte);

            if let Some(_code) = self.dict.find_code(&candidate) {
                // String exists in dictionary - continue building
                current = candidate;
            } else {
                // String not in dictionary
                // Output code for current string
                let code = self.dict.find_code(&current)
                    .expect("BUG: Current string should always exist in dictionary - it was either initialized or found in previous iteration");
                writer.write_bits(code, self.dict.current_bits())?;

                // Add new string to dictionary (if not full)
                if !self.dict.is_full() {
                    let _ = self.dict.add_string(candidate);
                } else if self.dict.config().use_clear_code {
                    // Table full - write clear code and reset (GIF-style)
                    writer.write_bits(self.dict.clear_code(), self.dict.current_bits())?;
                    self.dict.reset();
                }

                // Start new string with current byte
                current.clear();
                current.push(byte);
            }
        }

        // Output code for final string
        if !current.is_empty() {
            let code = self.dict.find_code(&current)
                .expect("BUG: Final string should always exist in dictionary - it was built from valid dictionary entries");
            writer.write_bits(code, self.dict.current_bits())?;
        }

        // Write EOI code
        writer.write_bits(self.dict.eoi_code(), self.dict.current_bits())?;

        // Flush remaining bits
        writer.into_vec()
    }

    /// Reset the encoder to initial state.
    pub fn reset(&mut self) {
        self.dict.reset();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decoder::LzwDecoder;

    #[test]
    fn test_encode_simple() {
        let config = LzwConfig::TIFF;
        let mut encoder = LzwEncoder::new(config).unwrap();

        let original = b"TOBEORNOTTOBEORTOBEORNOT";
        let compressed = encoder.encode(original).unwrap();

        // Compressed should be smaller (or at least not much larger)
        // For this highly repetitive string, compression should be effective
        assert!(compressed.len() < original.len() * 2);

        // Verify round-trip
        let mut decoder = LzwDecoder::new(config).unwrap();
        let decompressed = decoder.decode(&compressed, original.len()).unwrap();
        assert_eq!(decompressed, original);
    }

    #[test]
    fn test_encode_310_bytes() {
        let config = LzwConfig::TIFF;
        let mut encoder = LzwEncoder::new(config).unwrap();

        let original = b"This is a test of compression! ".repeat(10);
        assert_eq!(original.len(), 310);

        let compressed = encoder.encode(&original).unwrap();

        // Verify round-trip
        let mut decoder = LzwDecoder::new(config).unwrap();
        let decompressed = decoder.decode(&compressed, original.len()).unwrap();
        assert_eq!(decompressed.len(), 310);
        assert_eq!(decompressed, &original[..]);
    }

    #[test]
    fn test_encode_empty() {
        let config = LzwConfig::TIFF;
        let mut encoder = LzwEncoder::new(config).unwrap();

        let original = b"";
        let compressed = encoder.encode(original).unwrap();

        // Should contain at least EOI code
        assert!(!compressed.is_empty());

        // Verify round-trip
        let mut decoder = LzwDecoder::new(config).unwrap();
        let decompressed = decoder.decode(&compressed, 0).unwrap();
        assert_eq!(decompressed, original);
    }

    #[test]
    fn test_encode_single_byte() {
        let config = LzwConfig::TIFF;
        let mut encoder = LzwEncoder::new(config).unwrap();

        let original = b"A";
        let compressed = encoder.encode(original).unwrap();

        // Verify round-trip
        let mut decoder = LzwDecoder::new(config).unwrap();
        let decompressed = decoder.decode(&compressed, original.len()).unwrap();
        assert_eq!(decompressed, original);
    }

    #[test]
    fn test_encode_repeating() {
        let config = LzwConfig::TIFF;
        let mut encoder = LzwEncoder::new(config).unwrap();

        let original = vec![b'X'; 500];
        let compressed = encoder.encode(&original).unwrap();

        // Highly repetitive data should compress well
        assert!(compressed.len() < original.len() / 2);

        // Verify round-trip
        let mut decoder = LzwDecoder::new(config).unwrap();
        let decompressed = decoder.decode(&compressed, original.len()).unwrap();
        assert_eq!(decompressed, original);
    }

    #[test]
    fn test_encode_all_bytes() {
        let config = LzwConfig::TIFF;
        let mut encoder = LzwEncoder::new(config).unwrap();

        // Test with all possible byte values
        let original: Vec<u8> = (0..=255).collect();
        let compressed = encoder.encode(&original).unwrap();

        // Verify round-trip
        let mut decoder = LzwDecoder::new(config).unwrap();
        let decompressed = decoder.decode(&compressed, original.len()).unwrap();
        assert_eq!(decompressed, original);
    }

    #[test]
    fn test_encode_alternating() {
        let config = LzwConfig::TIFF;
        let mut encoder = LzwEncoder::new(config).unwrap();

        let original = b"ABABABABABABABABAB";
        let compressed = encoder.encode(original).unwrap();

        // Verify round-trip
        let mut decoder = LzwDecoder::new(config).unwrap();
        let decompressed = decoder.decode(&compressed, original.len()).unwrap();
        assert_eq!(decompressed, original);
    }
}
