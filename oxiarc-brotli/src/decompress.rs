//! Brotli decompression implementation.
//!
//! Implements the Brotli decompression algorithm per RFC 7932.
//! Parses the Brotli bitstream and reconstructs the original data.
//!
//! ## Stream Structure
//!
//! A Brotli stream consists of:
//! 1. Window size header (WBITS)
//! 2. Sequence of meta-blocks, the last one marked with ISLAST=1
//!
//! Each meta-block can be:
//! - Empty (ISEMPTY=1 in last block)
//! - Uncompressed (raw bytes)
//! - Compressed (LZ77 + Huffman encoded commands)

use oxiarc_core::cancel::CancellationToken;
use oxiarc_core::progress::ProgressHandle;

use crate::bit_reader::BitReader;
use crate::context::{
    ContextMap, ContextMode, NUM_DISTANCE_CONTEXTS, distance_context_id, literal_context_id,
};
use crate::dictionary;
use crate::error::{BrotliError, BrotliResult};
use crate::huffman::read_prefix_code;

/// Maximum allowed window size (16 MB as a reasonable limit).
const MAX_WINDOW_SIZE: usize = 16 * 1024 * 1024;

/// Maximum allowed output size (256 MB limit for safety).
const MAX_OUTPUT_SIZE: usize = 256 * 1024 * 1024;

/// Decompress a Brotli-compressed byte slice.
pub fn decompress(data: &[u8]) -> BrotliResult<Vec<u8>> {
    decompress_with_hooks(data, None, None)
}

/// Decompress with optional per-meta-block progress and cancellation hooks.
///
/// Called by [`decompress`] (with `None`/`None`) and by streaming types
/// that carry a [`ProgressHandle`] or [`CancellationToken`].
///
/// Progress fires after each meta-block is decoded; `processed` is the
/// number of output bytes produced so far; `total` is `None` because
/// the uncompressed length is generally not known ahead of time.
///
/// Cancellation is checked at the start of each meta-block iteration.
pub(crate) fn decompress_with_hooks(
    data: &[u8],
    progress: Option<&ProgressHandle>,
    cancel: Option<&CancellationToken>,
) -> BrotliResult<Vec<u8>> {
    if data.is_empty() {
        return Err(BrotliError::UnexpectedEof);
    }

    let mut reader = BitReader::new(data);
    let mut output = Vec::new();

    // Read window size.
    let wbits = read_window_bits(&mut reader)?;
    let window_size = (1usize << wbits).min(MAX_WINDOW_SIZE);

    // Distance ring buffer (last 4 distances used).
    let mut dist_ring = [4usize, 11, 15, 16];
    let mut dist_ring_idx = 0usize;

    // Process meta-blocks.
    loop {
        // Check for cancellation at each meta-block boundary.
        if let Some(token) = cancel {
            token.check().map_err(BrotliError::from)?;
        }

        let is_last = reader.read_bit()?;

        if is_last {
            // Check for empty last block.
            let is_empty = reader.read_bit()?;
            if is_empty {
                break;
            }
        }

        // Read meta-block length.
        let mnibbles_raw = reader.read_bits(2)?;
        if mnibbles_raw == 3 {
            // MNIBBLES = 0 means metadata block.
            // Reserved/metadata: skip.
            let _reserved = reader.read_bit()?;
            // Read MSKIPLEN.
            let mskipbytes = reader.read_bits(2)?;
            if mskipbytes > 0 {
                // Skip padding to byte boundary.
                let bits_to_skip = (8 - (reader.bits_consumed() % 8)) % 8;
                if bits_to_skip > 0 {
                    reader.read_bits(bits_to_skip as u32)?;
                }
                // Skip mskipbytes bytes.
                let skip_bits = mskipbytes * 8;
                let skip_len = reader.read_bits(skip_bits)?;
                for _ in 0..skip_len + 1 {
                    reader.read_bits(8)?;
                }
            }
            continue;
        }

        let mnibbles = mnibbles_raw + 4;
        let mlen_minus_1 = reader.read_bits(mnibbles * 4)?;
        let mlen = mlen_minus_1 as usize + 1;

        if output.len() + mlen > MAX_OUTPUT_SIZE {
            return Err(BrotliError::OutputTooLarge(output.len() + mlen));
        }

        // Check for uncompressed block.
        if !is_last {
            let is_uncompressed = reader.read_bit()?;
            if is_uncompressed {
                // Pad to byte boundary.
                let consumed = reader.bits_consumed();
                let padding = (8 - (consumed % 8)) % 8;
                if padding > 0 {
                    reader.read_bits(padding as u32)?;
                }
                // Read raw bytes.
                for _ in 0..mlen {
                    let byte = reader.read_bits(8)? as u8;
                    output.push(byte);
                }

                // Report progress after this uncompressed meta-block.
                if let Some(handle) = progress {
                    handle.on_progress(output.len() as u64, None);
                }

                continue;
            }
        } else {
            // For last block, ISUNCOMPRESSED bit.
            let is_uncompressed = reader.read_bit()?;
            if is_uncompressed {
                let consumed = reader.bits_consumed();
                let padding = (8 - (consumed % 8)) % 8;
                if padding > 0 {
                    reader.read_bits(padding as u32)?;
                }
                for _ in 0..mlen {
                    let byte = reader.read_bits(8)? as u8;
                    output.push(byte);
                }

                // Report progress after this uncompressed last meta-block.
                if let Some(handle) = progress {
                    handle.on_progress(output.len() as u64, None);
                }

                if is_last {
                    break;
                }
                continue;
            }
        }

        // Compressed meta-block.
        decompress_compressed_block(
            &mut reader,
            &mut output,
            mlen,
            window_size,
            &mut dist_ring,
            &mut dist_ring_idx,
        )?;

        // Report progress after each compressed meta-block.
        if let Some(handle) = progress {
            handle.on_progress(output.len() as u64, None);
        }

        if is_last {
            break;
        }
    }

    Ok(output)
}

/// Decompress a compressed meta-block.
#[allow(clippy::too_many_arguments)]
fn decompress_compressed_block(
    reader: &mut BitReader<'_>,
    output: &mut Vec<u8>,
    mlen: usize,
    _window_size: usize,
    dist_ring: &mut [usize; 4],
    dist_ring_idx: &mut usize,
) -> BrotliResult<()> {
    let output_start = output.len();
    let target_len = output_start + mlen;

    // Read number of literal block types (NBLTYPESL).
    let nbltypesl = read_block_type_count(reader)?;

    // Read number of insert-and-copy block types (NBLTYPESI).
    let nbltypesi = read_block_type_count(reader)?;

    // Read number of distance block types (NBLTYPESD).
    let nbltypesd = read_block_type_count(reader)?;

    // Read NPOSTFIX and NDIRECT.
    let npostfix = reader.read_bits(2)?;
    let ndirect_raw = reader.read_bits(4)?;
    let ndirect = ndirect_raw << npostfix;

    // Read context modes for each literal block type.
    let mut context_modes = Vec::with_capacity(nbltypesl as usize);
    for _ in 0..nbltypesl {
        let mode_bits = reader.read_bits(2)? as u8;
        let mode = ContextMode::from_bits(mode_bits).ok_or_else(|| {
            BrotliError::InvalidContextMap(format!("invalid context mode: {mode_bits}"))
        })?;
        context_modes.push(mode);
    }

    // Read literal context map.
    let num_literal_contexts = nbltypesl as usize * 64;
    let literal_context_map = if num_literal_contexts > 64 || nbltypesl > 1 {
        read_context_map(reader, num_literal_contexts)?
    } else {
        ContextMap::trivial(nbltypesl as usize, 64)
    };

    // Read distance context map.
    let num_distance_contexts = nbltypesd as usize * NUM_DISTANCE_CONTEXTS;
    let distance_context_map = if num_distance_contexts > NUM_DISTANCE_CONTEXTS || nbltypesd > 1 {
        read_context_map(reader, num_distance_contexts)?
    } else {
        ContextMap::trivial(nbltypesd as usize, NUM_DISTANCE_CONTEXTS)
    };

    // Determine alphabet sizes.
    let literal_alphabet_size = 256u32;
    let ic_alphabet_size = 704u32; // Insert-and-copy length alphabet.
    let distance_alphabet_size = compute_distance_alphabet_size(ndirect, npostfix);

    // Read prefix codes for literals.
    let num_literal_trees = literal_context_map.num_trees.max(1);
    let mut literal_trees = Vec::with_capacity(num_literal_trees);
    for _ in 0..num_literal_trees {
        literal_trees.push(read_prefix_code(reader, literal_alphabet_size)?);
    }

    // Read prefix code for insert-and-copy lengths.
    let num_ic_trees = nbltypesi as usize;
    let mut ic_trees = Vec::with_capacity(num_ic_trees.max(1));
    for _ in 0..num_ic_trees.max(1) {
        ic_trees.push(read_prefix_code(reader, ic_alphabet_size)?);
    }

    // Read prefix codes for distances.
    let num_distance_trees = distance_context_map.num_trees.max(1);
    let mut distance_trees = Vec::with_capacity(num_distance_trees);
    for _ in 0..num_distance_trees {
        distance_trees.push(read_prefix_code(reader, distance_alphabet_size)?);
    }

    // Block type/count state for each category.
    let literal_block_type = 0usize;
    let ic_block_type = 0usize;
    let distance_block_type = 0usize;
    let mut _literal_block_remaining = if nbltypesl > 1 {
        read_block_count(reader)?
    } else {
        usize::MAX
    };
    let mut _ic_block_remaining = if nbltypesi > 1 {
        read_block_count(reader)?
    } else {
        usize::MAX
    };
    let mut _distance_block_remaining = if nbltypesd > 1 {
        read_block_count(reader)?
    } else {
        usize::MAX
    };

    // Decode commands.
    while output.len() < target_len {
        // Read insert-and-copy length symbol.
        let ic_tree_idx = ic_block_type.min(ic_trees.len() - 1);
        let ic_symbol = ic_trees[ic_tree_idx].decode_symbol(reader)?;
        let (insert_length, copy_length) = decode_insert_copy_lengths(reader, ic_symbol, npostfix)?;

        // Read and output literals.
        for _ in 0..insert_length {
            if output.len() >= target_len {
                break;
            }

            // Get context for literal.
            let p1 = if output.is_empty() {
                0u8
            } else {
                output[output.len() - 1]
            };
            let p2 = if output.len() < 2 {
                0u8
            } else {
                output[output.len() - 2]
            };

            let mode = context_modes
                .get(literal_block_type)
                .copied()
                .unwrap_or(ContextMode::Lsb6);
            let ctx_id = literal_context_id(mode, p1, p2);
            let tree_idx = literal_context_map.tree_index(literal_block_type, ctx_id);
            let tree = &literal_trees[tree_idx.min(literal_trees.len() - 1)];

            let literal = tree.decode_symbol(reader)? as u8;
            output.push(literal);
        }

        if copy_length == 0 || output.len() >= target_len {
            continue;
        }

        // Read distance.
        let dist_ctx = distance_context_id(copy_length);
        let dist_tree_idx = distance_context_map.tree_index(distance_block_type, dist_ctx);
        let dist_tree = &distance_trees[dist_tree_idx.min(distance_trees.len() - 1)];

        let dist_symbol = dist_tree.decode_symbol(reader)?;
        let distance = decode_distance(
            reader,
            dist_symbol,
            dist_ring,
            *dist_ring_idx,
            ndirect,
            npostfix,
        )?;

        // Validate distance.
        if distance == 0 {
            return Err(BrotliError::InvalidDistance {
                distance: 0,
                max_distance: output.len(),
            });
        }

        // Update distance ring buffer.
        dist_ring[*dist_ring_idx & 3] = distance;
        *dist_ring_idx = (*dist_ring_idx + 1) & 3;

        // Check for dictionary reference.
        if distance > output.len() {
            // This might be a static dictionary reference.
            let dict_distance = distance - output.len() - 1;
            let word_length = copy_length;
            let transform_id = dict_distance / dictionary::num_transforms().max(1);
            let word_index = dict_distance % dictionary::num_transforms().max(1);

            match dictionary::lookup_word(word_length, word_index as u32) {
                Ok(word) => {
                    let transformed = dictionary::apply_transform(word, transform_id)?;
                    output.extend_from_slice(&transformed);
                }
                Err(_) => {
                    return Err(BrotliError::InvalidDistance {
                        distance,
                        max_distance: output.len(),
                    });
                }
            }
        } else {
            // Normal backward reference.
            let start = output.len() - distance;
            for i in 0..copy_length {
                if output.len() >= target_len {
                    break;
                }
                let src = start + (i % distance);
                let byte = output[src];
                output.push(byte);
            }
        }

        // Update block type state (simplified).
        let _ = literal_block_type;
        let _ = ic_block_type;
        let _ = distance_block_type;
    }

    // Truncate to exact target length if we overshot.
    output.truncate(target_len);

    Ok(())
}

/// Read the window size from the stream header.
fn read_window_bits(reader: &mut BitReader<'_>) -> BrotliResult<u32> {
    let first_bit = reader.read_bit()?;
    if !first_bit {
        return Ok(16);
    }

    // RFC 7932: read 3 more bits; WBITS = next_three + 17.
    let next_three = reader.read_bits(3)?;
    if next_three > 7 {
        return Err(BrotliError::InvalidWindowSize(next_three));
    }
    Ok(next_three + 17)
}

/// Read a block type count.
fn read_block_type_count(reader: &mut BitReader<'_>) -> BrotliResult<u32> {
    let first_bit = reader.read_bit()?;
    if !first_bit {
        return Ok(1);
    }

    // Read block type count - 1 using a variable-length encoding.
    let prefix = reader.read_bits(3)?;
    match prefix {
        0 => Ok(2),
        1 => Ok(3),
        2 => Ok(4),
        3 => {
            let extra = reader.read_bits(2)?;
            Ok(5 + extra)
        }
        4 => {
            let extra = reader.read_bits(3)?;
            Ok(9 + extra)
        }
        5 => {
            let extra = reader.read_bits(5)?;
            Ok(17 + extra)
        }
        6 => {
            let extra = reader.read_bits(6)?;
            Ok(49 + extra)
        }
        7 => {
            let extra = reader.read_bits(8)?;
            Ok(113 + extra).map(|v: u32| v.min(256))
        }
        _ => Ok(1),
    }
}

/// Read a block count value.
fn read_block_count(reader: &mut BitReader<'_>) -> BrotliResult<usize> {
    let code = reader.read_bits(2)?;
    match code {
        0 => {
            let extra = reader.read_bits(2)?;
            Ok((extra + 1) as usize)
        }
        1 => {
            let extra = reader.read_bits(4)?;
            Ok((extra + 5) as usize)
        }
        2 => {
            let extra = reader.read_bits(8)?;
            Ok((extra + 21) as usize)
        }
        3 => {
            let extra = reader.read_bits(16)?;
            Ok((extra + 277) as usize)
        }
        _ => Ok(1),
    }
}

/// Read a context map from the bitstream.
fn read_context_map(reader: &mut BitReader<'_>, num_entries: usize) -> BrotliResult<ContextMap> {
    // NTREES
    let ntrees = read_block_type_count(reader)? as usize;

    if ntrees == 1 {
        return Ok(ContextMap {
            map: vec![0u8; num_entries],
            num_contexts: num_entries,
            num_trees: 1,
        });
    }

    // Read RLEMAX.
    let use_rle = reader.read_bit()?;
    let rlemax = if use_rle { reader.read_bits(4)? + 1 } else { 0 };

    // Read prefix code for the context map.
    let alphabet_size = ntrees as u32 + rlemax;
    let cm_tree = read_prefix_code(reader, alphabet_size)?;

    // Decode context map entries.
    let mut map = Vec::with_capacity(num_entries);
    while map.len() < num_entries {
        let sym = cm_tree.decode_symbol(reader)?;
        if sym == 0 {
            map.push(0);
        } else if sym <= rlemax as u16 {
            // Run-length encoding: repeat zero.
            let run_length = 1usize << sym;
            for _ in 0..run_length {
                if map.len() < num_entries {
                    map.push(0);
                }
            }
        } else {
            map.push((sym - rlemax as u16) as u8);
        }
    }

    map.truncate(num_entries);

    // IMTF (inverse move-to-front transform).
    let imtf = reader.read_bit()?;
    if imtf {
        inverse_move_to_front(&mut map);
    }

    Ok(ContextMap {
        map,
        num_contexts: num_entries,
        num_trees: ntrees,
    })
}

/// Inverse move-to-front transform.
fn inverse_move_to_front(data: &mut [u8]) {
    let mut mtf = [0u8; 256];
    for (i, val) in mtf.iter_mut().enumerate() {
        *val = i as u8;
    }

    for v in data.iter_mut() {
        let idx = *v as usize;
        *v = mtf[idx];
        let value = mtf[idx];
        // Move to front.
        for j in (1..=idx).rev() {
            mtf[j] = mtf[j - 1];
        }
        mtf[0] = value;
    }
}

/// Decode insert and copy lengths from an insert-and-copy symbol.
fn decode_insert_copy_lengths(
    reader: &mut BitReader<'_>,
    symbol: u16,
    _npostfix: u32,
) -> BrotliResult<(usize, usize)> {
    // RFC 7932 Section 5: Insert-and-copy length decoding.
    // The symbol encodes both insert length and copy length.

    let sym = symbol as u32;

    // Table from RFC 7932 Section 5.
    // Symbols 0-127 encode various insert/copy combinations.
    // Symbols 128-703 encode larger values with extra bits.

    if sym < 128 {
        // Short codes: insert categories 0–15.
        let insert_cat = sym / 8;
        let copy_cat = sym % 8;

        let insert_length = decode_insert_length(reader, insert_cat)?;
        let copy_length = decode_copy_length_short(reader, copy_cat)?;

        Ok((insert_length, copy_length))
    } else {
        // Extended codes: insert categories ≥ 16.
        let adjusted = sym - 128;
        let insert_cat = adjusted / 8 + 16;
        let copy_cat = adjusted % 8;

        let insert_length = decode_insert_length(reader, insert_cat)?;
        let copy_length = decode_copy_length_short(reader, copy_cat)?;

        Ok((insert_length, copy_length))
    }
}

/// Decode an insert length for any category using the canonical insert-length
/// table shared with the encoder ([`crate::compress::insert_length_code_info`]).
///
/// This single function replaces the former split short/extended decoders, whose
/// extended branch did not invert the encoder, and guarantees encoder/decoder
/// agreement across the full insert-length range (including the large inserts
/// produced for incompressible meta-blocks).
fn decode_insert_length(reader: &mut BitReader<'_>, cat: u32) -> BrotliResult<usize> {
    let (base, extra_bits) = crate::compress::insert_length_code_info(cat);
    let extra = if extra_bits == 0 {
        0
    } else {
        reader.read_bits(extra_bits)? as usize
    };
    Ok(base + extra)
}

/// Decode a short copy length category.
fn decode_copy_length_short(reader: &mut BitReader<'_>, cat: u32) -> BrotliResult<usize> {
    match cat {
        0 => Ok(2),
        1 => Ok(3),
        2 => Ok(4),
        3 => Ok(5),
        4 => {
            let extra = reader.read_bits(1)?;
            Ok(6 + extra as usize)
        }
        5 => {
            let extra = reader.read_bits(1)?;
            Ok(8 + extra as usize)
        }
        6 => {
            let extra = reader.read_bits(2)?;
            Ok(10 + extra as usize)
        }
        7 => {
            let extra = reader.read_bits(2)?;
            Ok(14 + extra as usize)
        }
        _ => Ok(2),
    }
}

/// Decode a distance value from a distance symbol.
fn decode_distance(
    reader: &mut BitReader<'_>,
    symbol: u16,
    dist_ring: &[usize; 4],
    dist_ring_idx: usize,
    ndirect: u32,
    npostfix: u32,
) -> BrotliResult<usize> {
    let sym = symbol as u32;

    // Distance codes 0-3: ring buffer references.
    // 0: last distance
    // 1: second-to-last distance
    // 2: third-to-last distance
    // 3: fourth-to-last distance
    if sym < 4 {
        let ring_pos = (dist_ring_idx + 4 - sym as usize - 1) & 3;
        return Ok(dist_ring[ring_pos]);
    }

    // Distance codes 4-9: ring buffer +/- 1.
    if sym < 10 {
        let (ring_offset, delta) = match sym {
            4 => (0, 1i32),
            5 => (0, -1),
            6 => (0, 2),
            7 => (0, -2),
            8 => (0, 3),
            9 => (0, -3),
            _ => (0, 0),
        };
        let base = dist_ring[(dist_ring_idx + 3 - ring_offset) & 3];
        let distance = (base as i64 + delta as i64).max(1) as usize;
        return Ok(distance);
    }

    // Distance codes 10-15: ring buffer with larger offsets.
    if sym < 16 {
        let idx = ((sym - 10) / 2) as usize;
        let sign = if sym % 2 == 0 { 1i32 } else { -1 };
        let base_dist = dist_ring[(dist_ring_idx + 3 - idx) & 3];
        let offset = ((sym - 10) / 2 + 1) as i32;
        let distance = (base_dist as i64 + (sign * offset) as i64).max(1) as usize;
        return Ok(distance);
    }

    // Direct distances.
    if sym < 16 + ndirect {
        return Ok((sym - 16 + 1) as usize);
    }

    // Distance codes with extra bits.
    let adjusted = sym - 16 - ndirect;
    let postfix_mask = (1u32 << npostfix) - 1;
    let postfix = adjusted & postfix_mask;
    let hcode = adjusted >> npostfix;
    let nbits = 1 + (hcode >> 1);
    let offset = ((2 + (hcode & 1)) << nbits) - 4;
    let extra = reader.read_bits(nbits)?;
    let distance = ((offset + extra) << npostfix) + postfix + ndirect + 1;

    Ok(distance as usize)
}

/// Compute the distance alphabet size.
fn compute_distance_alphabet_size(ndirect: u32, npostfix: u32) -> u32 {
    16 + ndirect + (48 << npostfix)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_inverse_move_to_front() {
        let mut data = vec![0, 0, 0, 0];
        inverse_move_to_front(&mut data);
        assert_eq!(data, vec![0, 0, 0, 0]);

        let mut data = vec![0, 1, 2, 3];
        inverse_move_to_front(&mut data);
        assert_eq!(data, vec![0, 1, 2, 3]);
    }

    #[test]
    fn test_distance_alphabet_size() {
        assert_eq!(compute_distance_alphabet_size(0, 0), 64);
        assert_eq!(compute_distance_alphabet_size(0, 1), 112);
        assert_eq!(compute_distance_alphabet_size(4, 0), 68);
    }

    #[test]
    fn test_decode_insert_length_short() {
        // Categories 0–3: lengths 0–3, no extra bits needed.
        let data = [0xFF; 4];
        let mut reader = BitReader::new(&data);
        assert_eq!(decode_insert_length(&mut reader, 0).ok(), Some(0));
        assert_eq!(decode_insert_length(&mut reader, 1).ok(), Some(1));
        assert_eq!(decode_insert_length(&mut reader, 2).ok(), Some(2));
        assert_eq!(decode_insert_length(&mut reader, 3).ok(), Some(3));
    }

    #[test]
    fn test_decode_insert_length_extended_matches_encoder() {
        // The decoder's insert-length table must invert the encoder's across the
        // full category range. For categories with extra bits, feed all-zero
        // extra bits so the decoded value equals the category base.
        for cat in 0u32..=40 {
            let (base, extra_bits) = crate::compress::insert_length_code_info(cat);
            // Zero extra bits → decoded == base.
            let data = [0u8; 8];
            let mut reader = BitReader::new(&data);
            let decoded = decode_insert_length(&mut reader, cat).expect("decode");
            assert_eq!(decoded, base, "cat={cat} base mismatch");
            // Max extra bits → decoded == base + (1<<extra_bits) - 1.
            if extra_bits > 0 {
                let ones = [0xFFu8; 8];
                let mut reader = BitReader::new(&ones);
                let decoded = decode_insert_length(&mut reader, cat).expect("decode");
                let expected = base + (1usize << extra_bits) - 1;
                assert_eq!(decoded, expected, "cat={cat} max mismatch");
            }
        }
    }

    #[test]
    fn test_decode_copy_length_short() {
        let data = [0xFF; 4];
        let mut reader = BitReader::new(&data);
        assert_eq!(decode_copy_length_short(&mut reader, 0).ok(), Some(2));
        assert_eq!(decode_copy_length_short(&mut reader, 1).ok(), Some(3));
        assert_eq!(decode_copy_length_short(&mut reader, 2).ok(), Some(4));
        assert_eq!(decode_copy_length_short(&mut reader, 3).ok(), Some(5));
    }

    #[test]
    fn test_distance_ring_buffer() {
        let dist_ring = [1usize, 2, 3, 4];
        let data = [0x00; 4];
        let mut reader = BitReader::new(&data);

        // Symbol 0: last distance (ring_idx - 1).
        let d = decode_distance(&mut reader, 0, &dist_ring, 0, 0, 0);
        assert!(d.is_ok());
        // ring_idx=0, sym=0 => ring_pos = (0+4-0-1)&3 = 3 => dist_ring[3] = 4
        assert_eq!(d.ok(), Some(4));
    }
}
