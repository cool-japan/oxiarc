//! LZH-specific Huffman coding.
//!
//! LZH uses a different Huffman format than DEFLATE. It encodes:
//! - Character/length codes (NC = 510 symbols)
//! - Position/distance codes (varies by method)

use crate::methods::constants::{NC, NT};
use oxiarc_core::BitReader;
use oxiarc_core::error::{OxiArcError, Result};
use std::io::Read;

/// Maximum code length for LZH Huffman codes.
pub const MAX_CODE_LENGTH: usize = 16;

/// Entry in the Huffman lookup table.
/// Encodes both symbol (lower 10 bits) and length (upper 6 bits).
/// -1 indicates an invalid entry.
#[derive(Debug, Clone, Copy)]
struct TableEntry(i32);

impl TableEntry {
    const INVALID: TableEntry = TableEntry(-1);

    fn new(symbol: u16, length: u8) -> Self {
        TableEntry(((length as i32) << 16) | (symbol as i32))
    }

    fn is_valid(self) -> bool {
        self.0 >= 0
    }

    fn symbol(self) -> u16 {
        (self.0 & 0xFFFF) as u16
    }

    fn length(self) -> u8 {
        ((self.0 >> 16) & 0xFF) as u8
    }
}

/// LZH Huffman tree for decoding.
#[derive(Debug, Clone)]
pub struct LzhHuffmanTree {
    /// Lookup table for fast decoding (stores symbol + length).
    table: Vec<TableEntry>,
    /// Table bits (for fast lookup).
    table_bits: u8,
    /// Maximum code length.
    max_length: u8,
}

impl LzhHuffmanTree {
    /// Create a Huffman tree from code lengths.
    pub fn from_lengths(lengths: &[u8], table_bits: u8) -> Result<Self> {
        let table_size = 1 << table_bits;
        let mut table = vec![TableEntry::INVALID; table_size];

        if lengths.is_empty() {
            return Ok(Self {
                table,
                table_bits,
                max_length: 0,
            });
        }

        // Find max length
        let max_length = *lengths.iter().max().unwrap_or(&0);
        if max_length == 0 {
            return Ok(Self {
                table,
                table_bits,
                max_length: 0,
            });
        }

        // Count codes of each length
        let mut bl_count = [0u32; MAX_CODE_LENGTH + 1];
        for &len in lengths {
            if len > 0 {
                bl_count[len as usize] += 1;
            }
        }

        // Calculate starting codes
        let mut next_code = [0u32; MAX_CODE_LENGTH + 1];
        let mut code = 0u32;
        for bits in 1..=max_length as usize {
            code = (code + bl_count[bits - 1]) << 1;
            next_code[bits] = code;
        }

        // Build lookup table
        for (symbol, &len) in lengths.iter().enumerate() {
            if len > 0 && len <= table_bits {
                let len_usize = len as usize;
                let code = next_code[len_usize];
                next_code[len_usize] += 1;

                // Fill table entries (reversed for LSB-first)
                let reversed = Self::reverse_bits(code as u16, len);
                let fill_count = 1 << (table_bits as usize - len_usize);

                for i in 0..fill_count {
                    let index = reversed as usize | (i << len_usize);
                    if index < table_size {
                        table[index] = TableEntry::new(symbol as u16, len);
                    }
                }
            }
        }

        Ok(Self {
            table,
            table_bits,
            max_length,
        })
    }

    /// Reverse bits.
    fn reverse_bits(mut value: u16, length: u8) -> u16 {
        let mut result = 0u16;
        for _ in 0..length {
            result = (result << 1) | (value & 1);
            value >>= 1;
        }
        result
    }

    /// Decode a symbol from the bit reader.
    pub fn decode<R: Read>(&self, reader: &mut BitReader<R>) -> Result<u16> {
        if self.max_length == 0 {
            return Err(OxiArcError::invalid_huffman(reader.bit_position()));
        }

        // Try to peek table_bits, but fall back to less if near end of stream
        let bits = match reader.peek_bits(self.table_bits) {
            Ok(b) => b,
            Err(_) => {
                // Try to peek whatever bits are available
                // and pad with zeros (which is what the byte padding does)
                let mut available = 0u8;
                for i in 1..=self.table_bits {
                    if reader.peek_bits(i).is_ok() {
                        available = i;
                    } else {
                        break;
                    }
                }
                if available == 0 {
                    return Err(OxiArcError::unexpected_eof(1));
                }
                // Peek available bits
                reader.peek_bits(available)?
            }
        };

        let entry = self.table[bits as usize];

        if entry.is_valid() {
            // Skip only the actual code length, not all table_bits
            reader.skip_bits(entry.length())?;
            Ok(entry.symbol())
        } else {
            // Need slow path for longer codes
            Err(OxiArcError::invalid_huffman(reader.bit_position()))
        }
    }
}

/// Read the character/length Huffman tree from the stream.
pub fn read_c_tree<R: Read>(reader: &mut BitReader<R>) -> Result<LzhHuffmanTree> {
    let n = reader.read_bits(9)? as usize; // Number of codes
    #[cfg(test)]
    eprintln!("[read_c_tree] n={}, bit_pos={}", n, reader.bit_position());

    if n == 0 {
        // Special case: single code
        let c = reader.read_bits(9)? as usize;
        let mut lengths = vec![0u8; NC];
        if c < NC {
            lengths[c] = 1;
        }
        return LzhHuffmanTree::from_lengths(&lengths, 12);
    }

    // Read the temporary tree for decoding lengths
    let pt = read_pt_tree(reader)?;
    #[cfg(test)]
    eprintln!(
        "[read_c_tree] after PT tree, bit_pos={}",
        reader.bit_position()
    );

    // Read character/length code lengths
    let mut lengths = vec![0u8; NC];
    let mut i = 0;

    while i < n.min(NC) {
        #[cfg(test)]
        let before_pos = reader.bit_position();
        let c = pt.decode(reader)?;
        #[cfg(test)]
        if i < 5 || i > n - 3 {
            eprintln!(
                "[read_c_tree] i={}, decoded c={}, bits consumed={}",
                i,
                c,
                reader.bit_position() - before_pos
            );
        }

        if c <= 2 {
            // Run of zeros
            let count = match c {
                0 => 1,
                1 => reader.read_bits(4)? as usize + 3,
                2 => reader.read_bits(9)? as usize + 20,
                _ => unreachable!(),
            };
            #[cfg(test)]
            if i < 5 || i > n - 3 {
                eprintln!("[read_c_tree]   -> {} zeros", count);
            }
            for _ in 0..count {
                if i < lengths.len() {
                    lengths[i] = 0;
                    i += 1;
                }
            }
        } else if c == 3 {
            // PT code 3 is unused (reserved for skip mechanism in PT tree)
            // Treat as error or single zero for robustness
            #[cfg(test)]
            eprintln!("[read_c_tree]   -> PT code 3 (should not occur)");
            lengths[i] = 0;
            i += 1;
        } else {
            // PT code >= 4: C-length = PT_code - 3
            lengths[i] = (c - 3) as u8;
            #[cfg(test)]
            if i < 5 || i > n - 3 {
                eprintln!("[read_c_tree]   -> length {}", lengths[i]);
            }
            i += 1;
        }
    }

    #[cfg(test)]
    eprintln!("[read_c_tree] done, bit_pos={}", reader.bit_position());

    LzhHuffmanTree::from_lengths(&lengths, 12)
}

/// Read the position/distance Huffman tree from the stream.
pub fn read_p_tree<R: Read>(reader: &mut BitReader<R>, np: usize) -> Result<LzhHuffmanTree> {
    #[cfg(test)]
    eprintln!("[read_p_tree] start, bit_pos={}", reader.bit_position());

    let n = reader.read_bits(4)? as usize; // Number of codes
    #[cfg(test)]
    eprintln!("[read_p_tree] n={}", n);

    if n == 0 {
        // Special case: single code
        let c = reader.read_bits(4)? as usize;
        #[cfg(test)]
        eprintln!(
            "[read_p_tree] single code: {}, bit_pos={}",
            c,
            reader.bit_position()
        );
        let mut lengths = vec![0u8; np];
        if c < np {
            lengths[c] = 1;
        }
        return LzhHuffmanTree::from_lengths(&lengths, 8);
    }

    // Read position code lengths
    let mut lengths = vec![0u8; np];

    for length in lengths.iter_mut().take(n.min(np)) {
        let len = reader.read_bits(3)?;
        *length = len as u8;

        // Special escape for length 7
        if len == 7 {
            while reader.read_bit()? {
                *length += 1;
            }
        }
        #[cfg(test)]
        eprintln!("[read_p_tree] P = {}", *length);
    }

    #[cfg(test)]
    eprintln!("[read_p_tree] done, bit_pos={}", reader.bit_position());

    LzhHuffmanTree::from_lengths(&lengths, 8)
}

/// Read the temporary tree (for reading c_tree lengths).
fn read_pt_tree<R: Read>(reader: &mut BitReader<R>) -> Result<LzhHuffmanTree> {
    let n = reader.read_bits(5)? as usize;

    if n == 0 {
        // Single code
        let c = reader.read_bits(5)? as usize;
        let mut lengths = vec![0u8; NT];
        if c < NT {
            lengths[c] = 1;
        }
        return LzhHuffmanTree::from_lengths(&lengths, 5);
    }

    let mut lengths = vec![0u8; NT];

    for i in 0..n.min(NT) {
        if i == 3 {
            // Special: skip count
            let skip = reader.read_bits(2)? as usize;
            for j in 0..skip {
                if i + j < lengths.len() {
                    lengths[i + j] = 0;
                }
            }
            continue;
        }

        let len = reader.read_bits(3)?;
        lengths[i] = len as u8;

        if len == 7 {
            while reader.read_bit()? {
                lengths[i] += 1;
            }
        }
    }

    LzhHuffmanTree::from_lengths(&lengths, 5)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_reverse_bits() {
        assert_eq!(LzhHuffmanTree::reverse_bits(0b101, 3), 0b101);
        assert_eq!(LzhHuffmanTree::reverse_bits(0b1100, 4), 0b0011);
    }

    #[test]
    fn test_empty_tree() {
        let tree = LzhHuffmanTree::from_lengths(&[], 8).unwrap();
        assert_eq!(tree.max_length, 0);
    }

    #[test]
    fn test_single_symbol_tree() {
        let mut lengths = vec![0u8; 256];
        lengths[65] = 1; // Only 'A'

        let tree = LzhHuffmanTree::from_lengths(&lengths, 8).unwrap();
        assert_eq!(tree.max_length, 1);
    }
}
