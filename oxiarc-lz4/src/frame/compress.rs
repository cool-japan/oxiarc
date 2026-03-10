//! LZ4 frame compression functions.

use super::types::{FrameDescriptor, LZ4_FRAME_MAGIC};
use crate::block::compress_block;
use crate::xxhash::{XxHash32, xxhash32};
use oxiarc_core::error::Result;

#[cfg(feature = "parallel")]
use rayon::prelude::*;

/// Compress data using the official LZ4 frame format.
///
/// This produces output compatible with the lz4 reference implementation.
pub fn compress(input: &[u8]) -> Result<Vec<u8>> {
    compress_with_options(
        input,
        FrameDescriptor::new().with_content_size(input.len() as u64),
    )
}

/// Compress data using the official LZ4 frame format with custom options.
pub fn compress_with_options(input: &[u8], desc: FrameDescriptor) -> Result<Vec<u8>> {
    let mut output = Vec::with_capacity(15 + input.len());
    let mut content_hasher = if desc.content_checksum {
        Some(XxHash32::new())
    } else {
        None
    };

    // Write magic number
    output.extend_from_slice(&LZ4_FRAME_MAGIC.to_le_bytes());

    // Write frame descriptor
    let flg = desc.flg_byte();
    let bd = desc.block_max_size.to_bd();
    output.push(flg);
    output.push(bd);

    // Content size (if present)
    if let Some(size) = desc.content_size {
        output.extend_from_slice(&size.to_le_bytes());
    }

    // Dictionary ID (if present)
    if let Some(dict_id) = desc.dict_id {
        output.extend_from_slice(&dict_id.to_le_bytes());
    }

    // Header checksum (XXH32 of descriptor >> 8, masked to 1 byte)
    let header_checksum = {
        let header_end = output.len();
        let header_data = &output[4..header_end]; // FLG + BD + optional fields
        (xxhash32(header_data) >> 8) as u8
    };
    output.push(header_checksum);

    // Compress blocks
    let block_size = desc.block_max_size.size_bytes();
    let mut pos = 0;

    while pos < input.len() {
        let chunk_end = (pos + block_size).min(input.len());
        let chunk = &input[pos..chunk_end];

        // Update content hash
        if let Some(ref mut hasher) = content_hasher {
            hasher.update(chunk);
        }

        // Compress block
        let compressed = compress_block(chunk)?;

        // Decide whether to store compressed or uncompressed
        if compressed.len() < chunk.len() {
            // Store compressed
            let block_len = compressed.len() as u32;
            output.extend_from_slice(&block_len.to_le_bytes());
            output.extend_from_slice(&compressed);

            // Block checksum (if enabled)
            if desc.block_checksum {
                let checksum = xxhash32(&compressed);
                output.extend_from_slice(&checksum.to_le_bytes());
            }
        } else {
            // Store uncompressed (high bit set)
            let block_len = (chunk.len() as u32) | 0x80000000;
            output.extend_from_slice(&block_len.to_le_bytes());
            output.extend_from_slice(chunk);

            // Block checksum (if enabled)
            if desc.block_checksum {
                let checksum = xxhash32(chunk);
                output.extend_from_slice(&checksum.to_le_bytes());
            }
        }

        pos = chunk_end;
    }

    // End marker
    output.extend_from_slice(&0u32.to_le_bytes());

    // Content checksum (if enabled)
    if let Some(hasher) = content_hasher {
        let checksum = hasher.finish();
        output.extend_from_slice(&checksum.to_le_bytes());
    }

    Ok(output)
}

/// Compress data using parallel block compression (requires `parallel` feature).
///
/// This function splits the input into independent blocks and compresses them
/// in parallel using rayon. The number of threads is automatically determined
/// by rayon (typically matches the number of CPU cores).
///
/// # Arguments
///
/// * `input` - Data to compress
/// * `desc` - Frame descriptor options
///
/// # Returns
///
/// Compressed data in LZ4 frame format, identical to serial compression.
#[cfg(feature = "parallel")]
pub fn compress_with_options_parallel(input: &[u8], desc: FrameDescriptor) -> Result<Vec<u8>> {
    let mut output = Vec::with_capacity(15 + input.len());
    let mut content_hasher = if desc.content_checksum {
        Some(XxHash32::new())
    } else {
        None
    };

    // Write magic number
    output.extend_from_slice(&LZ4_FRAME_MAGIC.to_le_bytes());

    // Write frame descriptor
    let flg = desc.flg_byte();
    let bd = desc.block_max_size.to_bd();
    output.push(flg);
    output.push(bd);

    // Content size (if present)
    if let Some(size) = desc.content_size {
        output.extend_from_slice(&size.to_le_bytes());
    }

    // Dictionary ID (if present)
    if let Some(dict_id) = desc.dict_id {
        output.extend_from_slice(&dict_id.to_le_bytes());
    }

    // Header checksum (XXH32 of descriptor >> 8, masked to 1 byte)
    let header_checksum = {
        let header_end = output.len();
        let header_data = &output[4..header_end]; // FLG + BD + optional fields
        (xxhash32(header_data) >> 8) as u8
    };
    output.push(header_checksum);

    // Split input into blocks
    let block_size = desc.block_max_size.size_bytes();
    let chunks: Vec<&[u8]> = input.chunks(block_size).collect();

    // Compress blocks in parallel
    let compressed_blocks: Vec<Result<Vec<u8>>> = chunks
        .par_iter()
        .map(|chunk| compress_block(chunk))
        .collect();

    // Assemble compressed frame
    for (i, result) in compressed_blocks.into_iter().enumerate() {
        let chunk = chunks[i];
        let compressed = result?;

        // Update content hash
        if let Some(ref mut hasher) = content_hasher {
            hasher.update(chunk);
        }

        // Decide whether to store compressed or uncompressed
        if compressed.len() < chunk.len() {
            // Store compressed
            let block_len = compressed.len() as u32;
            output.extend_from_slice(&block_len.to_le_bytes());
            output.extend_from_slice(&compressed);

            // Block checksum (if enabled)
            if desc.block_checksum {
                let checksum = xxhash32(&compressed);
                output.extend_from_slice(&checksum.to_le_bytes());
            }
        } else {
            // Store uncompressed (high bit set)
            let block_len = (chunk.len() as u32) | 0x80000000;
            output.extend_from_slice(&block_len.to_le_bytes());
            output.extend_from_slice(chunk);

            // Block checksum (if enabled)
            if desc.block_checksum {
                let checksum = xxhash32(chunk);
                output.extend_from_slice(&checksum.to_le_bytes());
            }
        }
    }

    // End marker
    output.extend_from_slice(&0u32.to_le_bytes());

    // Content checksum (if enabled)
    if let Some(hasher) = content_hasher {
        let checksum = hasher.finish();
        output.extend_from_slice(&checksum.to_le_bytes());
    }

    Ok(output)
}

/// Compress data using parallel compression with default options (requires `parallel` feature).
///
/// This is a convenience wrapper around `compress_with_options_parallel` that
/// uses default frame descriptor settings.
#[cfg(feature = "parallel")]
pub fn compress_parallel(input: &[u8]) -> Result<Vec<u8>> {
    compress_with_options_parallel(
        input,
        FrameDescriptor::new().with_content_size(input.len() as u64),
    )
}
