//! Brotli compression implementation.
//!
//! Implements the Brotli compression algorithm per RFC 7932, including:
//! - LZ77 matching with backward references
//! - Context-dependent Huffman coding
//! - Insert-and-copy length encoding
//! - Distance short codes
//! - Meta-block formatting

use oxiarc_core::cancel::CancellationToken;
use oxiarc_core::progress::ProgressHandle;

use crate::bit_writer::BitWriter;
use crate::context::ContextMode;
use crate::error::{BrotliError, BrotliResult};
use crate::huffman::{
    build_huffman_tree, encode_symbol, write_prefix_code_and_build_tree, write_simple_prefix_code,
};
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
        if self.lgwin < 16 || self.lgwin > 24 {
            return Err(BrotliError::InvalidParameter(format!(
                "lgwin {} out of range [16, 24]",
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
    compress_with_hooks(data, params, None, None)
}

/// Compress data with optional per-meta-block progress and cancellation hooks.
///
/// Called by [`compress_with_params`] (with `None`/`None`) and by streaming
/// types that carry a [`ProgressHandle`] or [`CancellationToken`].
///
/// Progress fires after each meta-block is written; `processed` is the
/// approximate number of compressed bytes emitted so far, `total` is `None`
/// because the final size is not known ahead of time.
///
/// Cancellation is checked at the start of each meta-block iteration.
pub(crate) fn compress_with_hooks(
    data: &[u8],
    params: &BrotliParams,
    progress: Option<&ProgressHandle>,
    cancel: Option<&CancellationToken>,
) -> BrotliResult<Vec<u8>> {
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
        // Check for cancellation at each meta-block boundary.
        if let Some(token) = cancel {
            token.check().map_err(BrotliError::from)?;
        }

        let end = (offset + block_size).min(data.len());
        let block = &data[offset..end];
        let is_last = end == data.len();

        encode_meta_block(&mut writer, block, data, offset, params, is_last)?;
        offset = end;

        // Report progress: approximate compressed bytes produced so far.
        if let Some(handle) = progress {
            let bytes_out = writer.output().len() as u64;
            handle.on_progress(bytes_out, None);
        }
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
        // Single 0 bit.
        writer.write_bit(false)?;
    } else if (17..=24).contains(&lgwin) {
        // RFC 7932: flag bit = 1, then 3 bits for (lgwin - 17).
        // Encoding: flag(1) | ((lgwin-17) << 1) packed into 4 bits LSB-first.
        let n = lgwin - 17;
        writer.write_bits(n << 1 | 1, 4)?;
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
        writer.write_bit(false)?;
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

    // Build Huffman trees from frequencies (used for complex prefix codes).
    let literal_tree_freq = build_huffman_tree(&literal_freqs, 256)?;
    let ic_tree_freq = build_huffman_tree(&ic_freqs, 704)?;

    // Write literal prefix code and get the actual tree used for encoding.
    let literal_non_zero: Vec<u16> = literal_freqs
        .iter()
        .enumerate()
        .filter(|(_, f)| **f > 0)
        .map(|(i, _)| i as u16)
        .collect();

    let literal_tree =
        write_prefix_code_and_build_tree(writer, &literal_non_zero, &literal_tree_freq, 256)?;

    // Write insert-and-copy prefix code and get the actual tree used for encoding.
    let ic_non_zero: Vec<u16> = ic_freqs
        .iter()
        .enumerate()
        .filter(|(_, f)| **f > 0)
        .map(|(i, _)| i as u16)
        .collect();

    let ic_tree = write_prefix_code_and_build_tree(writer, &ic_non_zero, &ic_tree_freq, 704)?;

    // Write distance prefix code and get the actual tree used for encoding.
    if has_distances {
        let dist_tree_freq = build_huffman_tree(&dist_freqs, 64)?;
        let dist_non_zero: Vec<u16> = dist_freqs
            .iter()
            .enumerate()
            .filter(|(_, f)| **f > 0)
            .map(|(i, _)| i as u16)
            .collect();

        let dist_tree =
            write_prefix_code_and_build_tree(writer, &dist_non_zero, &dist_tree_freq, 64)?;

        // Write the actual command data.
        for ic in ic_commands.iter() {
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

/// Maximum copy length encodable in a single IC command's copy-cat field.
/// Per the copy length table (cat 0-7), maximum is 17.
const MAX_IC_COPY_LENGTH: usize = 17;

/// Build insert-and-copy commands from LZ77 command sequence.
///
/// Long backward references (copy_length > MAX_IC_COPY_LENGTH) are split into
/// multiple IC commands so that each copy fits within the encodable range.
fn build_insert_copy_commands(commands: &[Lz77Command]) -> Vec<InsertCopyCommand> {
    let mut result = Vec::new();
    let mut literals = Vec::new();

    for cmd in commands {
        match cmd {
            Lz77Command::Literal(b) => {
                literals.push(*b);
            }
            Lz77Command::Reference { length, distance } => {
                let mut remaining_copy = *length;
                let mut first = true;
                while remaining_copy > 0 {
                    let chunk = remaining_copy.min(MAX_IC_COPY_LENGTH);
                    remaining_copy -= chunk;

                    if first {
                        // First chunk: emit all accumulated literals.
                        result.push(InsertCopyCommand {
                            insert_length: literals.len(),
                            copy_length: chunk,
                            distance: *distance,
                            literals: std::mem::take(&mut literals),
                        });
                        first = false;
                    } else {
                        // Continuation chunks: no literals (insert_length=0).
                        result.push(InsertCopyCommand {
                            insert_length: 0,
                            copy_length: chunk,
                            distance: *distance,
                            literals: Vec::new(),
                        });
                    }
                }
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

/// Compute the insert length category and extra-bit count for the given insert length.
/// Returns (category, extra_bits, base_value) matching decode_insert_length_short.
fn insert_length_cat(insert_length: usize) -> (usize, u32, usize) {
    match insert_length {
        0 => (0, 0, 0),
        1 => (1, 0, 1),
        2 => (2, 0, 2),
        3 => (3, 0, 3),
        4..=5 => (4, 1, 4),
        6..=7 => (5, 1, 6),
        8..=11 => (6, 2, 8),
        12..=15 => (7, 2, 12),
        16..=23 => (8, 3, 16),
        24..=31 => (9, 3, 24),
        32..=47 => (10, 4, 32),
        48..=63 => (11, 4, 48),
        64..=95 => (12, 5, 64),
        96..=127 => (13, 5, 96),
        128..=191 => (14, 6, 128),
        _ => (15, 7, 192),
    }
}

/// Compute the copy length category and extra-bit count for the given copy length.
/// Returns (category, extra_bits, base_value) matching decode_copy_length_short.
fn copy_length_cat(copy_length: usize) -> (usize, u32, usize) {
    match copy_length {
        0..=2 => (0, 0, 2),
        3 => (1, 0, 3),
        4 => (2, 0, 4),
        5 => (3, 0, 5),
        6..=7 => (4, 1, 6),
        8..=9 => (5, 1, 8),
        10..=13 => (6, 2, 10),
        _ => (7, 2, 14),
    }
}

/// Compute the insert-and-copy length code.
///
/// Per RFC 7932 Section 5, the insert-and-copy length is encoded
/// as a single symbol from a combined alphabet. The symbol encodes
/// category indices for both insert and copy lengths, with extra bits
/// appended afterward. This matches decode_insert_copy_lengths exactly.
fn insert_copy_length_code(insert_length: usize, copy_length: usize) -> u16 {
    let (insert_cat, _, _) = insert_length_cat(insert_length);
    let (copy_cat, _, _) = copy_length_cat(copy_length);

    // Symbols 0-127: short codes (insert_cat 0-15, copy_cat 0-7).
    let code = insert_cat * 8 + copy_cat;
    code.min(703) as u16
}

/// Write extra bits for insert length.
/// Must match decode_insert_length_short/extended exactly.
fn write_insert_length_extra(writer: &mut BitWriter, insert_length: usize) -> BrotliResult<()> {
    let (_, extra_bits, base) = insert_length_cat(insert_length);
    if extra_bits == 0 {
        Ok(())
    } else {
        writer.write_bits((insert_length - base) as u32, extra_bits)
    }
}

/// Write extra bits for copy length.
/// Must match decode_copy_length_short exactly.
fn write_copy_length_extra(writer: &mut BitWriter, copy_length: usize) -> BrotliResult<()> {
    let (_, extra_bits, base) = copy_length_cat(copy_length);
    if extra_bits == 0 {
        Ok(())
    } else {
        writer.write_bits((copy_length - base) as u32, extra_bits)
    }
}

/// Compute the distance code and extra bits for encoding a distance value.
///
/// Uses the RFC 7932 Section 4 format with npostfix=0, ndirect=0:
/// - Codes 16+: distance is encoded with a hcode and extra bits.
/// - The distance is NOT encoded using ring buffer references (codes 0-15).
///
/// Returns (dist_code, extra_bits_value, extra_bits_count).
fn distance_encoding(distance: usize) -> (u16, u32, u32) {
    // With ndirect=0 and npostfix=0, direct distance encoding starts at code 16.
    // For hcode in 0..:
    //   nbits = 1 + (hcode >> 1)
    //   offset = ((2 + (hcode & 1)) << nbits) - 4
    //   distance range: [offset+1, offset+(1<<nbits)]
    //   dist_code = 16 + hcode
    //   extra = distance - (offset + 1)

    let d = distance as u64;
    // Find the hcode:
    let mut hcode = 0u32;
    loop {
        let nbits = 1 + (hcode >> 1);
        let offset = ((2u64 + ((hcode & 1) as u64)) << nbits) - 4;
        let low = offset + 1;
        let high = offset + (1u64 << nbits);
        if d >= low && d <= high {
            let extra = (d - low) as u32;
            let dist_code = (16 + hcode) as u16;
            return (dist_code, extra, nbits);
        }
        hcode += 1;
        if hcode > 60 {
            // Safety: return a large code with many bits.
            return (63u16, distance as u32, 24);
        }
    }
}

/// Compute the distance code for a backward reference distance.
/// Returns the dist_code to use in the prefix code tree lookup.
fn distance_code(distance: usize) -> u16 {
    distance_encoding(distance).0
}

/// Write extra bits for a distance code.
fn write_distance_extra(writer: &mut BitWriter, distance: usize) -> BrotliResult<()> {
    let (_, extra_val, extra_bits) = distance_encoding(distance);
    if extra_bits == 0 {
        Ok(())
    } else {
        writer.write_bits(extra_val, extra_bits)
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
        let mut params = BrotliParams {
            quality: 12,
            ..Default::default()
        };
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
        // With ndirect=0, npostfix=0: all distances use codes >=16.
        // hcode=0: nbits=1, offset=0, range [1,2], code=16
        // hcode=1: nbits=1, offset=2, range [3,4], code=17
        assert_eq!(distance_code(1), 16);
        assert_eq!(distance_code(2), 16);
        assert_eq!(distance_code(3), 17);
        assert!(distance_code(100) < 64);

        // Verify round-trip: encode and check distance recovery.
        for dist in [1, 2, 3, 4, 5, 10, 51, 100, 306] {
            let (code, extra_val, extra_bits) = distance_encoding(dist);
            // Verify the decode formula: distance = (offset + extra) + 1
            let hcode = (code as u32).saturating_sub(16);
            let nbits = 1 + (hcode >> 1);
            assert_eq!(nbits, extra_bits, "dist={dist}");
            let offset = ((2u64 + ((hcode & 1) as u64)) << nbits) - 4;
            let decoded = offset + extra_val as u64 + 1;
            assert_eq!(decoded as usize, dist, "dist={dist} code={code}");
        }
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
        for lgwin in 16..=24 {
            let mut writer = BitWriter::new();
            let result = write_window_bits(&mut writer, lgwin);
            assert!(result.is_ok(), "failed for lgwin={lgwin}");
        }
    }
}
