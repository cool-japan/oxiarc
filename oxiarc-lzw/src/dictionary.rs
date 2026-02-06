//! LZW dictionary (code table) management.

use crate::config::LzwConfig;
use crate::error::{LzwError, Result};
use std::collections::HashMap;

/// LZW dictionary for encoding and decoding.
///
/// The dictionary maintains a mapping between codes and byte sequences.
/// For encoding, we also maintain a reverse mapping (string -> code).
#[derive(Debug)]
pub struct LzwDictionary {
    /// Code table: code -> byte sequence.
    table: Vec<Vec<u8>>,
    /// Reverse lookup: byte sequence -> code (for encoding only).
    reverse: HashMap<Vec<u8>, u16>,
    /// Configuration.
    config: LzwConfig,
    /// Next available code.
    next_code: u16,
    /// Current code bit width.
    current_bits: u8,
}

impl LzwDictionary {
    /// Create a new LZW dictionary with the given configuration.
    pub fn new(config: LzwConfig) -> Result<Self> {
        if config.min_bits < 9 || config.min_bits > config.max_bits || config.max_bits > 12 {
            return Err(LzwError::InvalidBitWidth(config.min_bits));
        }

        let mut dict = Self {
            table: Vec::with_capacity(config.max_code() as usize + 1),
            reverse: HashMap::new(),
            config,
            next_code: 0,
            current_bits: config.min_bits,
        };

        dict.reset();
        Ok(dict)
    }

    /// Reset the dictionary to its initial state.
    pub fn reset(&mut self) {
        self.table.clear();
        self.reverse.clear();
        self.current_bits = self.config.min_bits;

        // Initialize with single-byte codes (0-255)
        let clear_code = self.config.clear_code();
        for i in 0..clear_code {
            let byte_seq = vec![i as u8];
            self.table.push(byte_seq.clone());
            self.reverse.insert(byte_seq, i);
        }

        // Add placeholder for clear code and EOI code
        self.table.push(Vec::new()); // clear_code (256)
        self.table.push(Vec::new()); // eoi_code (257)

        // Next available code
        self.next_code = self.config.first_code();
    }

    /// Add a new string to the dictionary (for encoding).
    ///
    /// Returns the assigned code, or error if table is full.
    pub fn add_string(&mut self, string: Vec<u8>) -> Result<u16> {
        if self.next_code > self.config.max_code() {
            return Err(LzwError::TableFull {
                max_codes: self.config.max_code(),
            });
        }

        let code = self.next_code;
        self.table.push(string.clone());
        self.reverse.insert(string, code);
        self.next_code += 1;

        // Increase bit width if needed
        self.update_bit_width();

        Ok(code)
    }

    /// Add a string to the dictionary (for decoding).
    ///
    /// Similar to add_string but doesn't update reverse map.
    /// Uses decoder-specific bit width update logic to account for
    /// the one-entry lag between encoder and decoder.
    pub fn add_string_decode(&mut self, string: Vec<u8>) -> Result<u16> {
        if self.next_code > self.config.max_code() {
            return Err(LzwError::TableFull {
                max_codes: self.config.max_code(),
            });
        }

        let code = self.next_code;
        self.table.push(string);
        self.next_code += 1;

        // Decoder increases bit width one code earlier than encoder
        // to compensate for the one-entry lag
        self.update_bit_width_decode();

        Ok(code)
    }

    /// Update bit width based on next_code.
    ///
    /// TIFF uses "early change": bit width increases when the next code
    /// equals 2^current_bits (one code earlier than standard LZW).
    fn update_bit_width(&mut self) {
        if self.current_bits < self.config.max_bits {
            let threshold = if self.config.early_change {
                // Early change: increase when next_code == 2^current_bits
                1 << self.current_bits
            } else {
                // Standard: increase when next_code == 2^current_bits + 1
                (1 << self.current_bits) + 1
            };

            if self.next_code >= threshold {
                self.current_bits += 1;
            }
        }
    }

    /// Update bit width for decoder (compensates for one-entry lag).
    ///
    /// The decoder adds dictionary entries one iteration later than the encoder,
    /// so the decoder's next_code is always one behind the encoder's next_code.
    /// To maintain bit-width synchronization, the decoder must increase its
    /// bit width one code earlier than the encoder.
    ///
    /// # Synchronization Analysis
    ///
    /// When encoder adds entry 511:
    /// - Encoder: next_code = 512, bit width increases to 10
    /// - Decoder: next_code = 511 (one behind!)
    ///
    /// Solution: Decoder threshold = encoder threshold - 1
    fn update_bit_width_decode(&mut self) {
        if self.current_bits < self.config.max_bits {
            let threshold = if self.config.early_change {
                // Decoder threshold is one less than encoder threshold
                // Encoder: increase when next_code == 2^current_bits
                // Decoder: increase when next_code == 2^current_bits - 1
                (1 << self.current_bits) - 1
            } else {
                // Standard LZW
                // Encoder: increase when next_code == 2^current_bits + 1
                // Decoder: increase when next_code == 2^current_bits
                1 << self.current_bits
            };

            if self.next_code >= threshold {
                self.current_bits += 1;
            }
        }
    }

    /// Get the byte sequence for a code.
    pub fn get_string(&self, code: u16) -> Result<&[u8]> {
        self.table
            .get(code as usize)
            .map(|v| v.as_slice())
            .ok_or(LzwError::InvalidCode(code))
    }

    /// Find the code for a byte sequence (for encoding).
    pub fn find_code(&self, string: &[u8]) -> Option<u16> {
        self.reverse.get(string).copied()
    }

    /// Check if the dictionary is full.
    pub fn is_full(&self) -> bool {
        self.next_code > self.config.max_code()
    }

    /// Get the current bit width.
    pub fn current_bits(&self) -> u8 {
        self.current_bits
    }

    /// Get the next code that will be assigned.
    pub fn next_code(&self) -> u16 {
        self.next_code
    }

    /// Get the clear code.
    pub fn clear_code(&self) -> u16 {
        self.config.clear_code()
    }

    /// Get the end-of-information code.
    pub fn eoi_code(&self) -> u16 {
        self.config.eoi_code()
    }

    /// Get the configuration.
    pub fn config(&self) -> &LzwConfig {
        &self.config
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dictionary_init() {
        let dict = LzwDictionary::new(LzwConfig::TIFF).unwrap();

        // Check initial single-byte codes
        for i in 0..256u16 {
            let string = dict.get_string(i).unwrap();
            assert_eq!(string, &[i as u8]);
        }

        // Check special codes
        assert_eq!(dict.clear_code(), 256);
        assert_eq!(dict.eoi_code(), 257);
        assert_eq!(dict.next_code(), 258);
        assert_eq!(dict.current_bits(), 9);
    }

    #[test]
    fn test_add_string() {
        let mut dict = LzwDictionary::new(LzwConfig::TIFF).unwrap();

        let code = dict.add_string(vec![b'A', b'B']).unwrap();
        assert_eq!(code, 258);

        let retrieved = dict.get_string(code).unwrap();
        assert_eq!(retrieved, b"AB");
    }

    #[test]
    fn test_bit_width_increase() {
        let mut dict = LzwDictionary::new(LzwConfig::TIFF).unwrap();

        // Initially 9 bits
        assert_eq!(dict.current_bits(), 9);

        // Add entries until bit width increases
        // With early change, bit width increases when next_code == 512 (2^9)
        // We start at 258, so need to add 254 entries (258 + 254 = 512)
        for i in 0..254 {
            dict.add_string(vec![i as u8, (i + 1) as u8]).unwrap();
        }

        // After adding 254 entries, next_code should be 512
        assert_eq!(dict.next_code(), 512);

        // Bit width should now be 10
        assert_eq!(dict.current_bits(), 10);
    }

    #[test]
    fn test_find_code() {
        let mut dict = LzwDictionary::new(LzwConfig::TIFF).unwrap();

        // Single-byte codes should be findable
        assert_eq!(dict.find_code(&[65]), Some(65));

        // Add a new code
        let code = dict.add_string(vec![b'A', b'B']).unwrap();
        assert_eq!(dict.find_code(b"AB"), Some(code));

        // Non-existent code
        assert_eq!(dict.find_code(b"XYZ"), None);
    }
}
