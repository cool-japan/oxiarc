//! LZ4 frame decompression functions.

use super::types::LZ4_LEGACY_MAGIC;
use super::types::{FrameDescriptor, LZ4_FRAME_MAGIC};
use crate::block::decompress_block;
use crate::xxhash::{XxHash32, xxhash32};
use oxiarc_core::error::{OxiArcError, Result};

/// Decompress LZ4 framed data.
///
/// Supports both official frame format and legacy/simple frames.
pub fn decompress(input: &[u8], max_output: usize) -> Result<Vec<u8>> {
    if input.len() < 4 {
        return Err(OxiArcError::invalid_header("LZ4 frame too short"));
    }

    let magic = u32::from_le_bytes([input[0], input[1], input[2], input[3]]);

    match magic {
        LZ4_FRAME_MAGIC => decompress_frame(input, max_output),
        LZ4_LEGACY_MAGIC => decompress_legacy(input, max_output),
        _ => {
            // Try simple format (our own magic)
            if magic == 0x184D2204 {
                decompress_frame(input, max_output)
            } else {
                Err(OxiArcError::invalid_magic(
                    LZ4_FRAME_MAGIC.to_le_bytes(),
                    &input[..4],
                ))
            }
        }
    }
}

/// Decompress official LZ4 frame format.
pub(super) fn decompress_frame(input: &[u8], max_output: usize) -> Result<Vec<u8>> {
    if input.len() < 7 {
        return Err(OxiArcError::invalid_header("LZ4 frame too short"));
    }

    let mut pos = 4; // Skip magic

    // Parse frame descriptor
    let flg = input[pos];
    pos += 1;
    let bd = input[pos];
    pos += 1;

    let mut desc = FrameDescriptor::parse(flg, bd)?;

    // Read content size if present
    if desc.content_size.is_some() {
        if pos + 8 > input.len() {
            return Err(OxiArcError::invalid_header("missing content size"));
        }
        let size = u64::from_le_bytes([
            input[pos],
            input[pos + 1],
            input[pos + 2],
            input[pos + 3],
            input[pos + 4],
            input[pos + 5],
            input[pos + 6],
            input[pos + 7],
        ]);
        desc.content_size = Some(size);
        pos += 8;
    }

    // Read dictionary ID if present
    if desc.dict_id.is_some() {
        if pos + 4 > input.len() {
            return Err(OxiArcError::invalid_header("missing dictionary ID"));
        }
        let dict_id =
            u32::from_le_bytes([input[pos], input[pos + 1], input[pos + 2], input[pos + 3]]);
        desc.dict_id = Some(dict_id);
        pos += 4;
    }

    // Verify header checksum
    if pos >= input.len() {
        return Err(OxiArcError::invalid_header("missing header checksum"));
    }
    let stored_hc = input[pos];
    pos += 1;

    let header_data = &input[4..pos - 1];
    let computed_hc = (xxhash32(header_data) >> 8) as u8;
    if stored_hc != computed_hc {
        return Err(OxiArcError::crc_mismatch(
            computed_hc as u32,
            stored_hc as u32,
        ));
    }

    // Decompress blocks
    let mut output = Vec::with_capacity(
        desc.content_size
            .map(|s| s as usize)
            .unwrap_or(max_output)
            .min(max_output),
    );
    let mut content_hasher = if desc.content_checksum {
        Some(XxHash32::new())
    } else {
        None
    };

    let block_max = desc.block_max_size.size_bytes();

    loop {
        if pos + 4 > input.len() {
            return Err(OxiArcError::corrupted(pos as u64, "truncated block header"));
        }

        let block_len =
            u32::from_le_bytes([input[pos], input[pos + 1], input[pos + 2], input[pos + 3]]);
        pos += 4;

        // End marker
        if block_len == 0 {
            break;
        }

        let uncompressed = (block_len & 0x80000000) != 0;
        let block_size = (block_len & 0x7FFFFFFF) as usize;

        if block_size > block_max {
            return Err(OxiArcError::corrupted(
                pos as u64,
                "block size exceeds maximum",
            ));
        }

        if pos + block_size > input.len() {
            return Err(OxiArcError::corrupted(pos as u64, "truncated block data"));
        }

        let block_data = &input[pos..pos + block_size];
        pos += block_size;

        // Verify block checksum if present
        if desc.block_checksum {
            if pos + 4 > input.len() {
                return Err(OxiArcError::corrupted(pos as u64, "missing block checksum"));
            }
            let stored_checksum =
                u32::from_le_bytes([input[pos], input[pos + 1], input[pos + 2], input[pos + 3]]);
            pos += 4;

            let computed_checksum = xxhash32(block_data);
            if stored_checksum != computed_checksum {
                return Err(OxiArcError::crc_mismatch(
                    computed_checksum,
                    stored_checksum,
                ));
            }
        }

        // Decompress block
        let decompressed = if uncompressed {
            block_data.to_vec()
        } else {
            decompress_block(block_data, block_max)?
        };

        // Update content hash
        if let Some(ref mut hasher) = content_hasher {
            hasher.update(&decompressed);
        }

        output.extend_from_slice(&decompressed);

        if output.len() > max_output {
            return Err(OxiArcError::corrupted(
                pos as u64,
                "output exceeds maximum size",
            ));
        }
    }

    // Verify content checksum if present
    if desc.content_checksum {
        if pos + 4 > input.len() {
            return Err(OxiArcError::corrupted(
                pos as u64,
                "missing content checksum",
            ));
        }
        let stored_checksum =
            u32::from_le_bytes([input[pos], input[pos + 1], input[pos + 2], input[pos + 3]]);

        if let Some(hasher) = content_hasher {
            let computed_checksum = hasher.finish();
            if stored_checksum != computed_checksum {
                return Err(OxiArcError::crc_mismatch(
                    computed_checksum,
                    stored_checksum,
                ));
            }
        }
    }

    Ok(output)
}

/// Decompress legacy LZ4 format.
pub(super) fn decompress_legacy(input: &[u8], max_output: usize) -> Result<Vec<u8>> {
    if input.len() < 8 {
        return Err(OxiArcError::invalid_header("legacy LZ4 frame too short"));
    }

    let mut output = Vec::new();
    let mut pos = 4; // Skip magic

    while pos + 4 <= input.len() {
        let block_size =
            u32::from_le_bytes([input[pos], input[pos + 1], input[pos + 2], input[pos + 3]])
                as usize;
        pos += 4;

        if block_size == 0 {
            break;
        }

        if pos + block_size > input.len() {
            return Err(OxiArcError::corrupted(pos as u64, "truncated block"));
        }

        let block_data = &input[pos..pos + block_size];
        pos += block_size;

        let decompressed = decompress_block(block_data, max_output - output.len())?;
        output.extend_from_slice(&decompressed);

        if output.len() > max_output {
            break;
        }
    }

    Ok(output)
}
