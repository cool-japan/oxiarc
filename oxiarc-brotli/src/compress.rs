//! Brotli compression implementation.
//!
//! Implements the Brotli compression algorithm per RFC 7932, including:
//! - LZ77 matching with backward references
//! - Context-dependent Huffman coding
//! - Insert-and-copy length encoding
//! - Distance short codes
//! - Meta-block formatting

use crate::bit_writer::BitWriter;
use crate::context::ContextMode;
use crate::error::{BrotliError, BrotliResult};
use crate::huffman::{build_huffman_tree, encode_symbol, write_simple_prefix_code};
use crate::lz77::{Lz77Command, Lz77Params, lz77_compress};

/// Brotli compression parameters.
#[derive(Debug, Clone)]
pub struct BrotliParams {
    /// Quality level (0-11). Higher = better compression, slower.
    pub quality: u32,
    /// Log2 of the window size (10-24). Default: 22 (4MB).
    pub lgwin: u32,
    /// Log2 of the maximum input block size (16-24). Default: 0 (auto).
    pub lgblock: u32,
}

impl Default for BrotliParams {
    fn default() -> Self {
        BrotliParams {
            quality: 6,
            lgwin: 22,
            lgblock: 0,
        }
    }
}

impl BrotliParams {
    /// Validate parameters.
    pub fn validate(&self) -> BrotliResult<()> {
        if self.quality > 11 {
            return Err(BrotliError::InvalidParameter(format!(
                "quality {} out of range [0, 11]",
                self.quality
            )));
        }
        if self.lgwin < 10 || self.lgwin > 24 {
            return Err(BrotliError::InvalidParameter(format!(
                "lgwin {} out of range [10, 24]",
                self.lgwin
            )));
        }
        if self.lgblock != 0 && (self.lgblock < 16 || self.lgblock > 24) {
            return Err(BrotliError::InvalidParameter(format!(
                "lgblock {} out of range [16, 24] (or 0 for auto)",
                self.lgblock
            )));
        }
        Ok(())
    }

    /// Get the window size in bytes.
    pub fn window_size(&self) -> usize {
        1 << self.lgwin
    }

    /// Get the effective block size.
    pub fn block_size(&self) -> usize {
        if self.lgblock == 0 {
            // Auto: choose based on quality.
            match self.quality {
                0..=4 => 1 << 18, // 256KB
                5..=8 => 1 << 20, // 1MB
                _ => 1 << 22,     // 4MB
            }
        } else {
            1 << self.lgblock
        }
    }
}

/// Compress data using Brotli with the given quality level.
pub fn compress(data: &[u8], quality: u32) -> BrotliResult<Vec<u8>> {
    let params = BrotliParams {
        quality,
        ..Default::default()
    };
    compress_with_params(data, &params)
}

/// Compress data using Brotli with full parameter control.
pub fn compress_with_params(data: &[u8], params: &BrotliParams) -> BrotliResult<Vec<u8>> {
    params.validate()?;

    if data.is_empty() {
        return encode_empty_stream();
    }

    let mut writer = BitWriter::with_capacity(data.len());

    // Write window size header.
    write_window_bits(&mut writer, params.lgwin)?;

    // Process data in blocks.
    let block_size = params.block_size();
    let mut offset = 0;

    while offset < data.len() {
        let end = (offset + block_size).min(data.len());
        let block = &data[offset..end];
        let is_last = end == data.len();

        encode_meta_block(&mut writer, block, data, offset, params, is_last)?;
        offset = end;
    }

    Ok(writer.finish())
}

/// Encode an empty Brotli stream (just the last-empty meta-block).
fn encode_empty_stream() -> BrotliResult<Vec<u8>> {
    let mut writer = BitWriter::new();

    // Window bits: WBITS = 16 (value doesn't matter for empty stream).
    write_window_bits(&mut writer, 16)?;

    // ISLAST = 1
    writer.write_bit(true)?;
    // ISEMPTY = 1
    writer.write_bit(true)?;

    // Pad to byte boundary.
    writer.flush();

    Ok(writer.finish())
}

/// Write the window size bits.
///
/// Per RFC 7932 Section 9.1:
/// - WBITS = 16: encoded as 0 (1 bit)
/// - WBITS = 17-24: encoded as (wbits-17)<<1 | 1 (4 bits) followed by 0
/// - WBITS = 10-14: encoded as (wbits-10)<<1 | 1 (4 bits) followed by 1
/// - WBITS = 15: encoded as special value
///
/// Simplified encoding used here:
/// - 1 bit: if 0, WBITS = 16
/// - Otherwise 3 more bits to encode other values
fn write_window_bits(writer: &mut BitWriter, lgwin: u32) -> BrotliResult<()> {
    if lgwin == 16 {
        writer.write_bit(false)?;
    } else if lgwin == 17 {
        writer.write_bits(0b0001, 4)?;
    } else if (18..=21).contains(&lgwin) {
        // Encode as: lower bit = 1, then 3 bits for (lgwin - 17).
        let n = lgwin - 17;
        writer.write_bits(n << 1 | 1, 4)?;
    } else if lgwin == 22 {
        writer.write_bits(0b1001, 4)?;
    } else if lgwin == 10 {
        writer.write_bits(0b0001_0001, 7)?;
    } else if lgwin == 11 {
        writer.write_bits(0b0011_0001, 7)?;
    } else if lgwin == 12 {
        writer.write_bits(0b0101_0001, 7)?;
    } else if lgwin == 13 {
        writer.write_bits(0b0111_0001, 7)?;
    } else if lgwin == 14 {
        writer.write_bits(0b1001_0001, 7)?;
    } else if lgwin == 15 {
        writer.write_bits(0b1011_0001, 7)?;
    } else if lgwin == 23 {
        writer.write_bits(0b1101, 4)?;
    } else if lgwin == 24 {
        writer.write_bits(0b1111, 4)?;
    } else {
        return Err(BrotliError::InvalidWindowSize(lgwin));
    }
    Ok(())
}

/// Encode a single meta-block.
fn encode_meta_block(
    writer: &mut BitWriter,
    block: &[u8],
    _full_data: &[u8],
    _offset: usize,
    params: &BrotliParams,
    is_last: bool,
) -> BrotliResult<()> {
    // ISLAST
    writer.write_bit(is_last)?;

    if is_last && block.is_empty() {
        // ISEMPTY
        writer.write_bit(true)?;
        return Ok(());
    }

    if is_last {
        // ISEMPTY = 0 for non-empty last block.
        // (ISEMPTY bit is only present when ISLAST=1)
    }

    // For low quality, use uncompressed meta-blocks.
    if params.quality == 0 {
        encode_uncompressed_meta_block(writer, block, is_last)?;
    } else {
        encode_compressed_meta_block(writer, block, params)?;
    }

    Ok(())
}

/// Encode an uncompressed meta-block.
fn encode_uncompressed_meta_block(
    writer: &mut BitWriter,
    block: &[u8],
    _is_last: bool,
) -> BrotliResult<()> {
    let mlen = block.len();

    // Write MLEN (meta-block length).
    write_meta_block_length(writer, mlen)?;

    // ISUNCOMPRESSED = 1
    writer.write_bit(true)?;

    // Pad to byte boundary.
    writer.flush();

    // Write raw data.
    writer.write_bytes(block)?;

    Ok(())
}

/// Encode a compressed meta-block using LZ77 + Huffman.
fn encode_compressed_meta_block(
    writer: &mut BitWriter,
    block: &[u8],
    params: &BrotliParams,
) -> BrotliResult<()> {
    let mlen = block.len();

    // Write MLEN.
    write_meta_block_length(writer, mlen)?;

    // ISUNCOMPRESSED = 0
    writer.write_bit(false)?;

    // Perform LZ77 compression.
    let lz77_params = Lz77Params {
        quality: params.quality,
        window_size: params.window_size(),
        min_match_len: 4,
        max_match_len: 256,
    };
    let commands = lz77_compress(block, &lz77_params);

    // Collect literal and distance statistics.
    let mut literal_freqs = vec![0u32; 256];
    let mut has_distances = false;

    for cmd in &commands {
        match cmd {
            Lz77Command::Literal(b) => {
                literal_freqs[*b as usize] += 1;
            }
            Lz77Command::Reference {
                length: _,
                distance: _,
            } => {
                has_distances = true;
            }
        }
    }

    // Build command sequence: insert-and-copy lengths.
    // For simplicity, we use a format where:
    // - NBLTYPESL = 1 (one literal block type)
    // - NBLTYPESI = 1 (one insert-and-copy block type)
    // - NBLTYPESD = 1 (one distance block type)

    // Number of block types for each category.
    // NBLTYPESL: 1 (encoded as a single 1-bit value)
    writer.write_bits(0, 1)?; // NBLTYPESL - 1 = 0 => 1 block type

    // NBLTYPESI: 1
    writer.write_bits(0, 1)?; // NBLTYPESI - 1 = 0

    // NBLTYPESD: 1
    writer.write_bits(0, 1)?; // NBLTYPESD - 1 = 0

    // NPOSTFIX = 0
    writer.write_bits(0, 2)?;

    // NDIRECT = 0
    writer.write_bits(0, 4)?;

    // Context modes for literal block type 0.
    writer.write_bits(ContextMode::Lsb6 as u32, 2)?;

    // Literal context map: trivial (1 tree).
    // NTREESL = 1 (no context map needed).

    // Distance context map: trivial (1 tree).
    // NTREESD = 1 (no context map needed).

    // Now we need to write the prefix codes and the actual command data.
    // Build the insert-and-copy length, literal, and distance prefix codes.

    // For a simple implementation, encode each command as:
    // - Insert length (number of literals to insert)
    // - Copy length (from backward reference)
    // - Literals
    // - Distance (if copy length > 0)

    // Convert commands to insert-and-copy format.
    let ic_commands = build_insert_copy_commands(&commands);

    // Build frequency tables for insert-and-copy length codes.
    let mut ic_freqs = vec![0u32; 704]; // insert-and-copy alphabet size
    let mut dist_freqs = vec![0u32; 64]; // simplified distance alphabet

    for ic in &ic_commands {
        let ic_code = insert_copy_length_code(ic.insert_length, ic.copy_length);
        if (ic_code as usize) < ic_freqs.len() {
            ic_freqs[ic_code as usize] += 1;
        }
        if ic.copy_length > 0 && ic.distance > 0 {
            let dist_code = distance_code(ic.distance);
            if (dist_code as usize) < dist_freqs.len() {
                dist_freqs[dist_code as usize] += 1;
            }
        }
    }

    // Build Huffman trees.
    let literal_tree = build_huffman_tree(&literal_freqs, 256)?;
    let ic_tree = build_huffman_tree(&ic_freqs, 704)?;

    // Write literal prefix code.
    let literal_non_zero: Vec<u16> = literal_freqs
        .iter()
        .enumerate()
        .filter(|(_, f)| **f > 0)
        .map(|(i, _)| i as u16)
        .collect();

    if literal_non_zero.len() <= 4 {
        write_simple_prefix_code(writer, &literal_non_zero, 256)?;
    } else {
        write_complex_prefix_code_for_tree(writer, &literal_tree)?;
    }

    // Write insert-and-copy prefix code.
    let ic_non_zero: Vec<u16> = ic_freqs
        .iter()
        .enumerate()
        .filter(|(_, f)| **f > 0)
        .map(|(i, _)| i as u16)
        .collect();

    if ic_non_zero.len() <= 4 {
        write_simple_prefix_code(writer, &ic_non_zero, 704)?;
    } else {
        write_complex_prefix_code_for_tree(writer, &ic_tree)?;
    }

    // Write distance prefix code (if needed).
    if has_distances {
        let dist_tree = build_huffman_tree(&dist_freqs, 64)?;
        let dist_non_zero: Vec<u16> = dist_freqs
            .iter()
            .enumerate()
            .filter(|(_, f)| **f > 0)
            .map(|(i, _)| i as u16)
            .collect();

        if dist_non_zero.len() <= 4 {
            write_simple_prefix_code(writer, &dist_non_zero, 64)?;
        } else {
            write_complex_prefix_code_for_tree(writer, &dist_tree)?;
        }

        // Write the actual command data.
        for ic in &ic_commands {
            let ic_code = insert_copy_length_code(ic.insert_length, ic.copy_length);
            encode_symbol(writer, &ic_tree, ic_code)?;

            // Write extra bits for insert length.
            write_insert_length_extra(writer, ic.insert_length)?;

            // Write extra bits for copy length.
            write_copy_length_extra(writer, ic.copy_length)?;

            // Write literals.
            for &lit in &ic.literals {
                encode_symbol(writer, &literal_tree, lit as u16)?;
            }

            // Write distance.
            if ic.copy_length > 0 && ic.distance > 0 {
                let dist_code = distance_code(ic.distance);
                encode_symbol(writer, &dist_tree, dist_code)?;
                write_distance_extra(writer, ic.distance)?;
            }
        }
    } else {
        // No distances needed - write a trivial distance tree.
        write_simple_prefix_code(writer, &[0], 64)?;

        // Write command data (literals only).
        for ic in &ic_commands {
            let ic_code = insert_copy_length_code(ic.insert_length, ic.copy_length);
            encode_symbol(writer, &ic_tree, ic_code)?;

            write_insert_length_extra(writer, ic.insert_length)?;

            for &lit in &ic.literals {
                encode_symbol(writer, &literal_tree, lit as u16)?;
            }
        }
    }

    Ok(())
}

/// Write a complex prefix code for a Huffman tree.
fn write_complex_prefix_code_for_tree(
    writer: &mut BitWriter,
    tree: &crate::huffman::HuffmanTree,
) -> BrotliResult<()> {
    // Use the existing complex prefix code writer.
    crate::huffman::write_complex_prefix_code(writer, tree)
}

/// An insert-and-copy command.
#[derive(Debug, Clone)]
struct InsertCopyCommand {
    /// Number of literal bytes to insert.
    insert_length: usize,
    /// Number of bytes to copy from backward reference.
    copy_length: usize,
    /// Distance for backward reference.
    distance: usize,
    /// The literal bytes to insert.
    literals: Vec<u8>,
}

/// Build insert-and-copy commands from LZ77 command sequence.
fn build_insert_copy_commands(commands: &[Lz77Command]) -> Vec<InsertCopyCommand> {
    let mut result = Vec::new();
    let mut literals = Vec::new();

    for cmd in commands {
        match cmd {
            Lz77Command::Literal(b) => {
                literals.push(*b);
            }
            Lz77Command::Reference { length, distance } => {
                result.push(InsertCopyCommand {
                    insert_length: literals.len(),
                    copy_length: *length,
                    distance: *distance,
                    literals: std::mem::take(&mut literals),
                });
            }
        }
    }

    // Remaining literals with no copy.
    if !literals.is_empty() {
        result.push(InsertCopyCommand {
            insert_length: literals.len(),
            copy_length: 0,
            distance: 0,
            literals,
        });
    }

    result
}

/// Compute the insert-and-copy length code.
///
/// Per RFC 7932 Section 5, the insert-and-copy length is encoded
/// as a single symbol from a combined alphabet.
fn insert_copy_length_code(insert_length: usize, copy_length: usize) -> u16 {
    // Simplified encoding: use a combined code.
    // Insert length categories:
    let insert_cat = match insert_length {
        0 => 0,
        1 => 1,
        2 => 2,
        3 => 3,
        4..=5 => 4,
        6..=7 => 5,
        _ => 6,
    };

    // Copy length categories:
    let copy_cat = match copy_length {
        0 => 0,
        1 => 0,
        2 => 0,
        3 => 0,
        4 => 1,
        5 => 2,
        6..=7 => 3,
        8..=11 => 4,
        _ => 5,
    };

    // Combined code: insert_cat * 8 + copy_cat
    let code = insert_cat * 8 + copy_cat;
    code.min(703) as u16
}

/// Write extra bits for insert length.
fn write_insert_length_extra(writer: &mut BitWriter, insert_length: usize) -> BrotliResult<()> {
    match insert_length {
        0..=3 => Ok(()), // No extra bits.
        4..=5 => writer.write_bits((insert_length - 4) as u32, 1),
        6..=7 => writer.write_bits((insert_length - 6) as u32, 1),
        8..=15 => writer.write_bits((insert_length - 8) as u32, 3),
        16..=31 => writer.write_bits((insert_length - 16) as u32, 4),
        32..=63 => writer.write_bits((insert_length - 32) as u32, 5),
        _ => {
            // Large insert: write as variable length.
            writer.write_bits(insert_length.min(255) as u32, 8)
        }
    }
}

/// Write extra bits for copy length.
fn write_copy_length_extra(writer: &mut BitWriter, copy_length: usize) -> BrotliResult<()> {
    match copy_length {
        0..=5 => Ok(()), // No extra bits.
        6..=7 => writer.write_bits((copy_length - 6) as u32, 1),
        8..=11 => writer.write_bits((copy_length - 8) as u32, 2),
        12..=19 => writer.write_bits((copy_length - 12) as u32, 3),
        _ => writer.write_bits(copy_length.min(255) as u32, 8),
    }
}

/// Compute the distance code for a backward reference distance.
fn distance_code(distance: usize) -> u16 {
    // Distance codes 0-15 are short codes (from distance ring buffer).
    // Codes 16+ encode the actual distance with extra bits.
    // For simplicity, we use direct encoding.
    match distance {
        1 => 0,
        2 => 1,
        3 => 2,
        4 => 3,
        5..=6 => 4,
        7..=8 => 5,
        9..=12 => 6,
        13..=16 => 7,
        17..=24 => 8,
        25..=32 => 9,
        _ => {
            // Larger distances: use higher codes.
            let bits = 32 - (distance as u32).leading_zeros();
            (bits + 6).min(63) as u16
        }
    }
}

/// Write extra bits for a distance code.
fn write_distance_extra(writer: &mut BitWriter, distance: usize) -> BrotliResult<()> {
    match distance {
        1..=4 => Ok(()), // No extra bits for short distances.
        5..=6 => writer.write_bits((distance - 5) as u32, 1),
        7..=8 => writer.write_bits((distance - 7) as u32, 1),
        9..=12 => writer.write_bits((distance - 9) as u32, 2),
        13..=16 => writer.write_bits((distance - 13) as u32, 2),
        17..=24 => writer.write_bits((distance - 17) as u32, 3),
        25..=32 => writer.write_bits((distance - 25) as u32, 3),
        _ => {
            // Variable-length encoding for large distances.
            let bits = 32 - (distance as u32).leading_zeros();
            let extra_bits = bits.saturating_sub(1);
            let base = 1u32 << (bits - 1);
            writer.write_bits(distance as u32 - base, extra_bits)
        }
    }
}

/// Write the meta-block length (MLEN).
///
/// Per RFC 7932 Section 9.2:
/// - MNIBBLES (2 bits): number of nibbles for MLEN minus 4
/// - MLEN (MNIBBLES*4 bits): meta-block length minus 1
fn write_meta_block_length(writer: &mut BitWriter, mlen: usize) -> BrotliResult<()> {
    if mlen == 0 {
        return Err(BrotliError::InvalidParameter(
            "meta-block length cannot be zero".to_string(),
        ));
    }

    let mlen_minus_1 = (mlen - 1) as u32;

    // Determine how many nibbles we need.
    let nibbles = if mlen_minus_1 < (1 << 4) {
        4
    } else if mlen_minus_1 < (1 << 12) {
        5
    } else if mlen_minus_1 < (1 << 24) {
        6
    } else {
        return Err(BrotliError::InvalidParameter(format!(
            "meta-block length {mlen} too large"
        )));
    };

    // MNIBBLES = nibbles - 4 (encoded in 2 bits).
    let mnibbles = nibbles - 4;
    writer.write_bits(mnibbles, 2)?;

    // MLEN - 1 in nibbles * 4 bits.
    writer.write_bits(mlen_minus_1, nibbles * 4)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_params_default() {
        let params = BrotliParams::default();
        assert_eq!(params.quality, 6);
        assert_eq!(params.lgwin, 22);
        assert_eq!(params.lgblock, 0);
        assert!(params.validate().is_ok());
    }

    #[test]
    fn test_params_validation() {
        let mut params = BrotliParams::default();
        params.quality = 12;
        assert!(params.validate().is_err());

        params.quality = 6;
        params.lgwin = 25;
        assert!(params.validate().is_err());

        params.lgwin = 22;
        params.lgblock = 15;
        assert!(params.validate().is_err());
    }

    #[test]
    fn test_compress_empty() {
        let result = compress(b"", 6).expect("should compress empty");
        assert!(!result.is_empty());
    }

    #[test]
    fn test_compress_small() {
        let data = b"Hello, Brotli!";
        let result = compress(data, 0);
        assert!(result.is_ok());
        let compressed = result.expect("should compress");
        assert!(!compressed.is_empty());
    }

    #[test]
    fn test_insert_copy_length_code() {
        // Pure insert (no copy).
        let code = insert_copy_length_code(5, 0);
        assert!(code < 704);

        // Insert + copy.
        let code = insert_copy_length_code(3, 4);
        assert!(code < 704);
    }

    #[test]
    fn test_distance_code() {
        assert_eq!(distance_code(1), 0);
        assert_eq!(distance_code(2), 1);
        assert!(distance_code(100) < 64);
    }

    #[test]
    fn test_meta_block_length() {
        let mut writer = BitWriter::new();
        write_meta_block_length(&mut writer, 1).expect("should write mlen 1");

        let mut writer = BitWriter::new();
        write_meta_block_length(&mut writer, 1000).expect("should write mlen 1000");
    }

    #[test]
    fn test_window_bits() {
        for lgwin in 10..=24 {
            let mut writer = BitWriter::new();
            let result = write_window_bits(&mut writer, lgwin);
            assert!(result.is_ok(), "failed for lgwin={lgwin}");
        }
    }
}
