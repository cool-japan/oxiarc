//! LZH decompression.
//!
//! This module implements decompression for LZH methods (lh4-lh7).

use crate::huffman::{LzhHuffmanTree, read_c_tree, read_p_tree};
use crate::lzss::LzssDecoder;
use crate::methods::LzhMethod;
use oxiarc_core::BitReader;
use oxiarc_core::error::{OxiArcError, Result};
use oxiarc_core::traits::{DecompressStatus, Decompressor};
use std::io::Read;

/// Block size for LZH compressed data (reserved for future use).
#[allow(dead_code)]
const BLOCK_SIZE: usize = 0x4000; // 16KB

/// LZH decompressor.
#[derive(Debug)]
pub struct LzhDecoder {
    /// Compression method.
    method: LzhMethod,
    /// LZSS decoder.
    lzss: LzssDecoder,
    /// Expected uncompressed size.
    uncompressed_size: u64,
    /// Bytes decoded so far.
    bytes_decoded: u64,
    /// Whether decoding is finished.
    finished: bool,
}

impl LzhDecoder {
    /// Create a new LZH decoder.
    pub fn new(method: LzhMethod, uncompressed_size: u64) -> Self {
        let window_size = method.window_size().max(256);
        Self {
            method,
            lzss: LzssDecoder::new(window_size),
            uncompressed_size,
            bytes_decoded: 0,
            finished: false,
        }
    }

    /// Reset the decoder.
    pub fn reset(&mut self) {
        self.lzss.reset();
        self.bytes_decoded = 0;
        self.finished = false;
    }

    /// Decode compressed data.
    pub fn decode<R: Read>(&mut self, reader: &mut R) -> Result<Vec<u8>> {
        if self.method.is_stored() {
            return self.decode_stored(reader);
        }

        let mut bit_reader = BitReader::new(reader);
        self.decode_compressed(&mut bit_reader)
    }

    /// Decode stored (lh0) data.
    fn decode_stored<R: Read>(&mut self, reader: &mut R) -> Result<Vec<u8>> {
        let mut output = vec![0u8; self.uncompressed_size as usize];
        reader.read_exact(&mut output)?;
        self.bytes_decoded = self.uncompressed_size;
        self.finished = true;
        Ok(output)
    }

    /// Decode compressed data.
    fn decode_compressed<R: Read>(&mut self, reader: &mut BitReader<R>) -> Result<Vec<u8>> {
        let np = match self.method {
            LzhMethod::Lh4 => 14,
            LzhMethod::Lh5 => 14,
            LzhMethod::Lh6 => 16,
            LzhMethod::Lh7 => 17,
            LzhMethod::Lh0 => return Err(OxiArcError::unsupported_method("lh0")),
        };

        #[cfg(test)]
        eprintln!(
            "[decode] starting, uncompressed_size={}",
            self.uncompressed_size
        );

        while self.bytes_decoded < self.uncompressed_size {
            // Read block
            let block_size = reader.read_bits(16)? as usize;
            #[cfg(test)]
            eprintln!(
                "[decode] block_size={}, bit_pos={}",
                block_size,
                reader.bit_position()
            );
            if block_size == 0 {
                break;
            }

            // Read Huffman trees
            let c_tree = read_c_tree(reader)?;
            let p_tree = read_p_tree(reader, np)?;

            #[cfg(test)]
            eprintln!(
                "[decode] after reading trees, bit_pos={}",
                reader.bit_position()
            );

            // Decode block
            self.decode_block(reader, &c_tree, &p_tree, block_size)?;
        }

        self.finished = true;
        Ok(self.lzss.take_output())
    }

    /// Decode a single block.
    fn decode_block<R: Read>(
        &mut self,
        reader: &mut BitReader<R>,
        c_tree: &LzhHuffmanTree,
        p_tree: &LzhHuffmanTree,
        block_size: usize,
    ) -> Result<()> {
        let target = self.bytes_decoded + block_size as u64;
        let target = target.min(self.uncompressed_size);

        #[cfg(test)]
        eprintln!(
            "[decode_block] target={}, bytes_decoded={}",
            target, self.bytes_decoded
        );

        while self.bytes_decoded < target {
            #[cfg(test)]
            let before_pos = reader.bit_position();
            let c = c_tree.decode(reader)?;
            #[cfg(test)]
            eprintln!(
                "[decode_block] decoded c={}, bits consumed={}, bit_pos={}",
                c,
                reader.bit_position() - before_pos,
                reader.bit_position()
            );

            if c < 256 {
                // Literal
                self.lzss.decode_literal(c as u8);
                self.bytes_decoded += 1;
                #[cfg(test)]
                eprintln!(
                    "[decode_block]   -> literal '{}' (0x{:02x}), bytes_decoded={}",
                    c as u8 as char, c as u8, self.bytes_decoded
                );
            } else {
                // Length + distance
                let length = c - 256 + 3; // Minimum match = 3

                // Read position code
                let p = p_tree.decode(reader)?;

                // Calculate distance
                // For p >= 1, we read p extra bits
                // distance = (1 << p) + extra_value
                let distance = if p == 0 {
                    1
                } else {
                    let extra_bits = p as u8;
                    let extra = reader.read_bits(extra_bits)?;
                    (1 << p) + extra as u16
                };

                self.lzss.decode_match(length, distance)?;
                self.bytes_decoded += length as u64;
                #[cfg(test)]
                eprintln!(
                    "[decode_block]   -> match len={}, dist={}",
                    length, distance
                );
            }
        }

        #[cfg(test)]
        eprintln!("[decode_block] done, bytes_decoded={}", self.bytes_decoded);

        Ok(())
    }

    /// Get the decoded output.
    pub fn output(&self) -> &[u8] {
        self.lzss.output()
    }

    /// Check if decoding is finished.
    pub fn is_done(&self) -> bool {
        self.finished
    }
}

impl Decompressor for LzhDecoder {
    fn decompress(
        &mut self,
        input: &[u8],
        output: &mut [u8],
    ) -> Result<(usize, usize, DecompressStatus)> {
        if self.finished {
            return Ok((0, 0, DecompressStatus::Done));
        }

        let mut cursor = std::io::Cursor::new(input);
        let result = self.decode(&mut cursor)?;

        let consumed = cursor.position() as usize;
        let to_copy = result.len().min(output.len());
        output[..to_copy].copy_from_slice(&result[..to_copy]);

        Ok((consumed, to_copy, DecompressStatus::Done))
    }

    fn reset(&mut self) {
        LzhDecoder::reset(self);
    }

    fn is_finished(&self) -> bool {
        self.finished
    }
}

/// Decompress LZH data.
pub fn decode_lzh(data: &[u8], method: LzhMethod, uncompressed_size: u64) -> Result<Vec<u8>> {
    let mut decoder = LzhDecoder::new(method, uncompressed_size);
    let mut cursor = std::io::Cursor::new(data);
    decoder.decode(&mut cursor)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_decode_stored() {
        let data = b"Hello, World!";
        let result = decode_lzh(data, LzhMethod::Lh0, data.len() as u64).unwrap();
        assert_eq!(result, data);
    }

    // Note: Testing compressed data would require valid LZH-compressed samples
}
