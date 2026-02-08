//! DEFLATE decompression (inflate).
//!
//! This module implements the DEFLATE decompression algorithm as specified
//! in RFC 1951. It supports all three block types:
//! - Type 0: Stored (uncompressed)
//! - Type 1: Fixed Huffman codes
//! - Type 2: Dynamic Huffman codes

use crate::huffman::HuffmanTree;
use crate::tables::{
    CODE_LENGTH_ORDER, DISTANCE_EXTRA_BITS, LENGTH_EXTRA_BITS, decode_distance, decode_length,
    fixed_distance_tree, fixed_litlen_tree,
};
use oxiarc_core::error::{OxiArcError, Result};
use oxiarc_core::traits::{DecompressStatus, Decompressor};
use oxiarc_core::{BitReader, OutputRingBuffer};
use std::io::Read;

/// Maximum dictionary size for DEFLATE (32KB).
pub const MAX_DICTIONARY_SIZE: usize = 32768;

/// DEFLATE decompressor.
#[derive(Debug)]
pub struct Inflater {
    /// Output ring buffer.
    output: OutputRingBuffer,
    /// Whether we've seen the final block.
    final_block: bool,
    /// Whether decompression is complete.
    finished: bool,
    /// Expected dictionary checksum (if dictionary is required).
    expected_dict_checksum: Option<u32>,
}

impl Inflater {
    /// Create a new DEFLATE decompressor.
    pub fn new() -> Self {
        Self {
            output: OutputRingBuffer::with_capacity(32768, 65536),
            final_block: false,
            finished: false,
            expected_dict_checksum: None,
        }
    }

    /// Create a new DEFLATE decompressor with a preset dictionary.
    ///
    /// The dictionary must match the one used during compression.
    /// The decompressor uses the dictionary to resolve back-references
    /// that point into the dictionary content.
    ///
    /// # Arguments
    ///
    /// * `dictionary` - Dictionary data (up to 32KB). If larger, only the
    ///   last 32KB is used.
    ///
    /// # Returns
    ///
    /// A new Inflater with the dictionary preloaded.
    pub fn with_dictionary(dictionary: &[u8]) -> Self {
        let mut inflater = Self::new();
        inflater.set_dictionary(dictionary);
        inflater
    }

    /// Set a preset dictionary for decompression.
    ///
    /// # Arguments
    ///
    /// * `dictionary` - Dictionary data (up to 32KB). If larger, only the
    ///   last 32KB is used.
    ///
    /// # Returns
    ///
    /// The Adler-32 checksum of the dictionary.
    pub fn set_dictionary(&mut self, dictionary: &[u8]) -> u32 {
        self.output.preload_dictionary(dictionary);
        self.expected_dict_checksum = Some(Self::adler32(dictionary));
        self.expected_dict_checksum.unwrap_or(1)
    }

    /// Get the expected dictionary checksum.
    pub fn expected_dictionary_checksum(&self) -> Option<u32> {
        self.expected_dict_checksum
    }

    /// Check if a dictionary is currently set.
    pub fn has_dictionary(&self) -> bool {
        self.expected_dict_checksum.is_some()
    }

    /// Calculate Adler-32 checksum (for dictionary identification).
    fn adler32(data: &[u8]) -> u32 {
        const MOD_ADLER: u32 = 65521;
        const NMAX: usize = 5552;

        let mut a: u32 = 1;
        let mut b: u32 = 0;

        let mut remaining = data;

        while remaining.len() >= NMAX {
            let (chunk, rest) = remaining.split_at(NMAX);
            remaining = rest;

            for &byte in chunk {
                a += byte as u32;
                b += a;
            }

            a %= MOD_ADLER;
            b %= MOD_ADLER;
        }

        for &byte in remaining {
            a += byte as u32;
            b += a;
        }

        ((b % MOD_ADLER) << 16) | (a % MOD_ADLER)
    }

    /// Reset the decompressor.
    pub fn reset(&mut self) {
        self.output.clear();
        self.final_block = false;
        self.finished = false;
        self.expected_dict_checksum = None;
    }

    /// Reset the decompressor but keep the dictionary.
    pub fn reset_keep_dictionary(&mut self) {
        let checksum = self.expected_dict_checksum;
        self.output.clear();
        self.final_block = false;
        self.finished = false;
        self.expected_dict_checksum = checksum;
    }

    /// Decompress data from a reader.
    pub fn inflate_reader<R: Read>(&mut self, reader: &mut R) -> Result<Vec<u8>> {
        let mut bit_reader = BitReader::new(reader);
        self.inflate(&mut bit_reader)
    }

    /// Decompress data from a bit reader.
    pub fn inflate<R: Read>(&mut self, reader: &mut BitReader<R>) -> Result<Vec<u8>> {
        while !self.final_block {
            self.inflate_block(reader)?;
        }

        self.finished = true;
        Ok(self.output.output().to_vec())
    }

    /// Decompress a single block.
    fn inflate_block<R: Read>(&mut self, reader: &mut BitReader<R>) -> Result<()> {
        // Read block header
        let bfinal = reader.read_bit()?;
        let btype = reader.read_bits(2)?;

        self.final_block = bfinal;

        match btype {
            0 => self.inflate_stored(reader),
            1 => self.inflate_fixed(reader),
            2 => self.inflate_dynamic(reader),
            3 => Err(OxiArcError::invalid_header("Reserved block type 3")),
            _ => unreachable!(),
        }
    }

    /// Decompress a stored (uncompressed) block.
    fn inflate_stored<R: Read>(&mut self, reader: &mut BitReader<R>) -> Result<()> {
        // Align to byte boundary
        reader.align_to_byte();

        // Read LEN and NLEN
        let len = reader.read_bits(16)? as u16;
        let nlen = reader.read_bits(16)? as u16;

        // Validate
        if len != !nlen {
            return Err(OxiArcError::corrupted(
                reader.bit_position() / 8,
                format!("LEN/NLEN mismatch: {} vs {}", len, !nlen),
            ));
        }

        // Copy bytes
        let mut buf = vec![0u8; len as usize];
        reader.read_bytes(&mut buf)?;
        self.output.write_literals(&buf);

        Ok(())
    }

    /// Decompress a block with fixed Huffman codes.
    fn inflate_fixed<R: Read>(&mut self, reader: &mut BitReader<R>) -> Result<()> {
        let litlen_tree = fixed_litlen_tree()?;
        let dist_tree = fixed_distance_tree()?;

        self.inflate_huffman(reader, litlen_tree, dist_tree)
    }

    /// Decompress a block with dynamic Huffman codes.
    fn inflate_dynamic<R: Read>(&mut self, reader: &mut BitReader<R>) -> Result<()> {
        // Read code counts
        let hlit = reader.read_bits(5)? as usize + 257; // literal/length codes
        let hdist = reader.read_bits(5)? as usize + 1; // distance codes
        let hclen = reader.read_bits(4)? as usize + 4; // code length codes

        // Read code length code lengths
        let mut code_length_lengths = [0u8; 19];
        for i in 0..hclen {
            code_length_lengths[CODE_LENGTH_ORDER[i]] = reader.read_bits(3)? as u8;
        }

        // Build code length tree
        let code_length_tree = HuffmanTree::from_code_lengths(&code_length_lengths)?;

        // Read literal/length and distance code lengths
        let mut all_lengths = vec![0u8; hlit + hdist];
        let mut i = 0;

        while i < all_lengths.len() {
            let code = code_length_tree.decode(reader)?;

            match code {
                0..=15 => {
                    all_lengths[i] = code as u8;
                    i += 1;
                }
                16 => {
                    // Copy previous length 3-6 times
                    if i == 0 {
                        return Err(OxiArcError::corrupted(
                            reader.bit_position() / 8,
                            "Code 16 at start of lengths",
                        ));
                    }
                    let repeat = reader.read_bits(2)? as usize + 3;
                    let prev = all_lengths[i - 1];
                    for _ in 0..repeat {
                        if i >= all_lengths.len() {
                            return Err(OxiArcError::corrupted(
                                reader.bit_position() / 8,
                                "Code length overflow",
                            ));
                        }
                        all_lengths[i] = prev;
                        i += 1;
                    }
                }
                17 => {
                    // Repeat 0 for 3-10 times
                    let repeat = reader.read_bits(3)? as usize + 3;
                    for _ in 0..repeat {
                        if i >= all_lengths.len() {
                            return Err(OxiArcError::corrupted(
                                reader.bit_position() / 8,
                                "Code length overflow",
                            ));
                        }
                        all_lengths[i] = 0;
                        i += 1;
                    }
                }
                18 => {
                    // Repeat 0 for 11-138 times
                    let repeat = reader.read_bits(7)? as usize + 11;
                    for _ in 0..repeat {
                        if i >= all_lengths.len() {
                            return Err(OxiArcError::corrupted(
                                reader.bit_position() / 8,
                                "Code length overflow",
                            ));
                        }
                        all_lengths[i] = 0;
                        i += 1;
                    }
                }
                _ => {
                    return Err(OxiArcError::invalid_huffman(reader.bit_position()));
                }
            }
        }

        // Split into literal/length and distance lengths
        let litlen_lengths = &all_lengths[..hlit];
        let dist_lengths = &all_lengths[hlit..];

        // Build trees
        let litlen_tree = HuffmanTree::from_code_lengths(litlen_lengths)?;
        let dist_tree = HuffmanTree::from_code_lengths(dist_lengths)?;

        self.inflate_huffman(reader, &litlen_tree, &dist_tree)
    }

    /// Decompress using Huffman codes.
    fn inflate_huffman<R: Read>(
        &mut self,
        reader: &mut BitReader<R>,
        litlen_tree: &HuffmanTree,
        dist_tree: &HuffmanTree,
    ) -> Result<()> {
        loop {
            let code = litlen_tree.decode(reader)?;

            if code < 256 {
                // Literal byte
                self.output.write_literal(code as u8);
            } else if code == 256 {
                // End of block
                break;
            } else if code <= 285 {
                // Length code
                let length_idx = (code - 257) as usize;
                let extra_bits = LENGTH_EXTRA_BITS[length_idx];
                let extra = reader.read_bits(extra_bits)? as u16;
                let length = decode_length(code, extra);

                // Read distance
                let dist_code = dist_tree.decode(reader)?;
                if dist_code >= 30 {
                    return Err(OxiArcError::corrupted(
                        reader.bit_position() / 8,
                        format!("Invalid distance code: {}", dist_code),
                    ));
                }

                let dist_extra_bits = DISTANCE_EXTRA_BITS[dist_code as usize];
                let dist_extra = reader.read_bits(dist_extra_bits)? as u16;
                let distance = decode_distance(dist_code, dist_extra);

                // Copy from history
                self.output.copy_match(distance as usize, length as usize)?;
            } else {
                return Err(OxiArcError::corrupted(
                    reader.bit_position() / 8,
                    format!("Invalid literal/length code: {}", code),
                ));
            }
        }

        Ok(())
    }

    /// Get the decompressed output.
    pub fn output(&self) -> &[u8] {
        self.output.output()
    }

    /// Take ownership of the decompressed output.
    pub fn into_output(self) -> Vec<u8> {
        self.output.into_output()
    }
}

impl Default for Inflater {
    fn default() -> Self {
        Self::new()
    }
}

impl Decompressor for Inflater {
    fn decompress(
        &mut self,
        input: &[u8],
        output: &mut [u8],
    ) -> Result<(usize, usize, DecompressStatus)> {
        // Simple implementation: decompress all at once
        if self.finished {
            return Ok((0, 0, DecompressStatus::Done));
        }

        let mut cursor = std::io::Cursor::new(input);
        let result = self.inflate_reader(&mut cursor)?;

        let consumed = cursor.position() as usize;
        let to_copy = result.len().min(output.len());
        output[..to_copy].copy_from_slice(&result[..to_copy]);

        self.finished = true;

        Ok((consumed, to_copy, DecompressStatus::Done))
    }

    fn reset(&mut self) {
        Inflater::reset(self);
    }

    fn is_finished(&self) -> bool {
        self.finished
    }
}

/// Decompress DEFLATE data.
pub fn inflate(data: &[u8]) -> Result<Vec<u8>> {
    let mut inflater = Inflater::new();
    let mut cursor = std::io::Cursor::new(data);
    inflater.inflate_reader(&mut cursor)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_inflate_stored() {
        // Stored block: BFINAL=1, BTYPE=00, then aligned LEN=5, NLEN=!5, "Hello"
        // Header: 0b00000001 (BFINAL=1, BTYPE=00)
        // LEN: 0x05, 0x00
        // NLEN: 0xFA, 0xFF
        // Data: "Hello"
        let compressed = vec![
            0x01, // BFINAL=1, BTYPE=00, padding
            0x05, 0x00, // LEN=5
            0xFA, 0xFF, // NLEN=65530
            b'H', b'e', b'l', b'l', b'o',
        ];

        let result = inflate(&compressed).unwrap();
        assert_eq!(result, b"Hello");
    }

    #[test]
    fn test_inflate_empty() {
        // Empty stored block
        let compressed = vec![
            0x01, // BFINAL=1, BTYPE=00
            0x00, 0x00, // LEN=0
            0xFF, 0xFF, // NLEN
        ];

        let result = inflate(&compressed).unwrap();
        assert!(result.is_empty());
    }

    // Note: More comprehensive tests would require generating valid
    // compressed data with fixed/dynamic Huffman codes
}
