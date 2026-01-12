//! Huffman coding for Zstandard literals.
//!
//! Zstandard uses canonical Huffman coding for literal compression.
//! Maximum code length is 11 bits.

use crate::fse::{FseBitReader, FseDecoder, read_fse_table_description};
use oxiarc_core::error::{OxiArcError, Result};

/// Maximum Huffman code length in Zstandard.
pub const MAX_CODE_LENGTH: u8 = 11;

/// Maximum number of symbols (byte values).
pub const MAX_SYMBOLS: usize = 256;

/// Huffman decoding table entry.
#[derive(Debug, Clone, Copy, Default)]
pub struct HuffmanEntry {
    /// Decoded symbol.
    pub symbol: u8,
    /// Number of bits for this code.
    pub num_bits: u8,
}

/// Huffman decoding table.
#[derive(Debug, Clone)]
pub struct HuffmanTable {
    /// Decoding entries indexed by prefix.
    entries: Vec<HuffmanEntry>,
    /// Maximum code length used.
    max_bits: u8,
}

impl HuffmanTable {
    /// Build Huffman table from weights (symbol counts).
    pub fn from_weights(weights: &[u8]) -> Result<Self> {
        if weights.is_empty() {
            return Err(OxiArcError::CorruptedData {
                offset: 0,
                message: "empty Huffman weights".to_string(),
            });
        }

        // Calculate total weight
        let mut total_weight = 0u32;
        let mut max_weight = 0u8;

        for &w in weights {
            if w > 0 {
                total_weight += 1u32 << (w - 1);
                if w > max_weight {
                    max_weight = w;
                }
            }
        }

        if total_weight == 0 {
            return Err(OxiArcError::CorruptedData {
                offset: 0,
                message: "all Huffman weights are zero".to_string(),
            });
        }

        // Calculate max_bits (power of 2 >= total_weight)
        let max_bits = 32 - total_weight.leading_zeros();
        let max_bits = max_bits.min(MAX_CODE_LENGTH as u32) as u8;

        // Build canonical Huffman codes
        let table_size = 1usize << max_bits;
        let mut entries = vec![HuffmanEntry::default(); table_size];

        // Assign codes by weight
        let mut code = 0u32;
        let mut code_lengths = vec![0u8; weights.len()];

        // Convert weights to code lengths
        // weight -> num_bits: max_bits + 1 - weight
        for (symbol, &weight) in weights.iter().enumerate() {
            if weight > 0 {
                code_lengths[symbol] = max_bits + 1 - weight;
            }
        }

        // Sort symbols by code length for canonical ordering
        let mut symbols: Vec<(usize, u8)> = weights
            .iter()
            .enumerate()
            .filter(|&(_, w)| *w > 0)
            .map(|(s, _)| (s, code_lengths[s]))
            .collect();

        symbols.sort_by_key(|&(_, len)| len);

        // Assign canonical codes
        let mut prev_len = 0u8;
        for (symbol, len) in symbols {
            if len > prev_len {
                code <<= len - prev_len;
                prev_len = len;
            }

            // Fill table entries
            let num_entries = 1 << (max_bits - len);
            let base_code = (code as usize) << (max_bits - len);

            for i in 0..num_entries {
                entries[base_code + i] = HuffmanEntry {
                    symbol: symbol as u8,
                    num_bits: len,
                };
            }

            code += 1;
        }

        Ok(Self { entries, max_bits })
    }

    /// Decode a symbol from bits.
    #[inline]
    pub fn decode(&self, bits: u32) -> &HuffmanEntry {
        let idx = bits as usize & ((1 << self.max_bits) - 1);
        &self.entries[idx]
    }

    /// Get max bits for this table.
    pub fn max_bits(&self) -> u8 {
        self.max_bits
    }
}

/// Read Huffman table from compressed format.
pub fn read_huffman_table(data: &[u8]) -> Result<(HuffmanTable, usize)> {
    if data.is_empty() {
        return Err(OxiArcError::CorruptedData {
            offset: 0,
            message: "empty Huffman table data".to_string(),
        });
    }

    let header = data[0];

    if header < 128 {
        // FSE-compressed weights
        read_huffman_table_fse(data)
    } else {
        // Direct representation
        read_huffman_table_direct(data)
    }
}

/// Read Huffman table with direct 4-bit weights.
fn read_huffman_table_direct(data: &[u8]) -> Result<(HuffmanTable, usize)> {
    let header = data[0];
    let num_symbols = (header - 127) as usize;

    if num_symbols == 0 || num_symbols > MAX_SYMBOLS {
        return Err(OxiArcError::CorruptedData {
            offset: 0,
            message: format!("invalid number of Huffman symbols: {}", num_symbols),
        });
    }

    let bytes_needed = num_symbols.div_ceil(2);
    if data.len() < 1 + bytes_needed {
        return Err(OxiArcError::CorruptedData {
            offset: 0,
            message: "truncated Huffman table".to_string(),
        });
    }

    let mut weights = vec![0u8; num_symbols];

    for (i, weight) in weights.iter_mut().enumerate() {
        let byte_idx = 1 + i / 2;
        let is_high = i % 2 == 0;

        *weight = if is_high {
            data[byte_idx] >> 4
        } else {
            data[byte_idx] & 0x0F
        };
    }

    let table = HuffmanTable::from_weights(&weights)?;
    Ok((table, 1 + bytes_needed))
}

/// Read Huffman table with FSE-compressed weights.
fn read_huffman_table_fse(data: &[u8]) -> Result<(HuffmanTable, usize)> {
    let compressed_size = data[0] as usize;

    if compressed_size == 0 {
        return Err(OxiArcError::CorruptedData {
            offset: 0,
            message: "zero-length FSE Huffman table".to_string(),
        });
    }

    if data.len() < 1 + compressed_size {
        return Err(OxiArcError::CorruptedData {
            offset: 0,
            message: "truncated FSE Huffman table".to_string(),
        });
    }

    let fse_data = &data[1..1 + compressed_size];

    // Read FSE table for weights (max symbol is 12 for weight values 0-12)
    let (fse_table, fse_bytes) = read_fse_table_description(fse_data, 12)?;

    // Decode weights using FSE
    let bitstream_data = &fse_data[fse_bytes..];
    let mut reader = FseBitReader::new(bitstream_data)?;
    let mut decoder = FseDecoder::new(&fse_table, &mut reader);

    let mut weights = Vec::with_capacity(MAX_SYMBOLS);

    while weights.len() < MAX_SYMBOLS && !reader.is_empty() {
        let weight = decoder.decode(&mut reader);
        weights.push(weight);
    }

    if weights.is_empty() {
        return Err(OxiArcError::CorruptedData {
            offset: 0,
            message: "no Huffman weights decoded".to_string(),
        });
    }

    let table = HuffmanTable::from_weights(&weights)?;
    Ok((table, 1 + compressed_size))
}

/// Huffman bitstream reader (reads backwards).
pub struct HuffmanBitReader<'a> {
    /// Input bytes.
    data: &'a [u8],
    /// Current bit position from start.
    bit_pos: usize,
    /// Total bits available.
    total_bits: usize,
}

impl<'a> HuffmanBitReader<'a> {
    /// Create a new Huffman bit reader.
    pub fn new(data: &'a [u8]) -> Result<Self> {
        if data.is_empty() {
            return Err(OxiArcError::CorruptedData {
                offset: 0,
                message: "empty Huffman bitstream".to_string(),
            });
        }

        // Find sentinel bit in last byte
        let last_byte = data[data.len() - 1];
        if last_byte == 0 {
            return Err(OxiArcError::CorruptedData {
                offset: 0,
                message: "Huffman stream ends with zero".to_string(),
            });
        }

        let padding = 7 - (31 - last_byte.leading_zeros()) as usize;
        let total_bits = data.len() * 8 - padding - 1; // Exclude padding and sentinel

        Ok(Self {
            data,
            bit_pos: 0,
            total_bits,
        })
    }

    /// Peek up to 16 bits without consuming.
    pub fn peek_bits(&self, n: u8) -> u32 {
        if n == 0 || self.bit_pos >= self.total_bits {
            return 0;
        }

        // Calculate position from end (read backwards)
        let read_pos = self.total_bits - self.bit_pos - 1;

        let byte_pos = read_pos / 8;
        let bit_offset = read_pos % 8;

        // Read bytes
        let mut value = 0u32;
        for i in 0..3 {
            if byte_pos >= i && byte_pos - i < self.data.len() {
                value |= (self.data[byte_pos - i] as u32) << (i * 8);
            }
        }

        // Extract bits (MSB-first for Huffman)
        (value >> (24 - bit_offset - n as usize)) & ((1 << n) - 1)
    }

    /// Consume n bits.
    pub fn consume(&mut self, n: u8) {
        self.bit_pos += n as usize;
    }

    /// Check if stream is exhausted.
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.bit_pos >= self.total_bits
    }

    /// Remaining bits.
    #[allow(dead_code)]
    pub fn remaining(&self) -> usize {
        self.total_bits.saturating_sub(self.bit_pos)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_huffman_table_from_weights() {
        // Simple test: two symbols with equal weight
        let weights = [1u8, 1];
        let table = HuffmanTable::from_weights(&weights).unwrap();

        assert!(table.max_bits() >= 1);
    }

    #[test]
    fn test_huffman_table_varying_weights() {
        // More complex: varying weights
        let weights = [4u8, 3, 2, 1, 1, 0, 0, 0];
        let table = HuffmanTable::from_weights(&weights).unwrap();

        // Should have valid entries
        assert!(table.max_bits() > 0);
    }

    #[test]
    fn test_direct_huffman_table() {
        // Build a direct representation
        // Header byte > 127: num_symbols = header - 127
        // Then 4-bit weights packed into bytes

        let mut data = vec![127 + 4]; // 4 symbols (header = 131)
        data.push(0x21); // weights 2, 1
        data.push(0x11); // weights 1, 1

        let (table, consumed) = read_huffman_table(&data).unwrap();

        assert_eq!(consumed, 3);
        assert!(table.max_bits() > 0);
    }

    #[test]
    fn test_empty_weights_fails() {
        let weights: [u8; 0] = [];
        assert!(HuffmanTable::from_weights(&weights).is_err());
    }
}
