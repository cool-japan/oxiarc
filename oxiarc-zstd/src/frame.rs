//! Zstandard frame parsing and decompression.
//!
//! Handles the top-level frame format including header, blocks, and checksum.

use crate::literals::LiteralsDecoder;
use crate::sequences::{Sequence, SequencesDecoder};
use crate::xxhash::xxhash64_checksum;
use crate::{BlockType, MAX_BLOCK_SIZE, MAX_WINDOW_SIZE, ZSTD_MAGIC};
use oxiarc_core::error::{OxiArcError, Result};

/// Frame header descriptor flags.
const FHD_SINGLE_SEGMENT: u8 = 0x20;
const FHD_CONTENT_CHECKSUM: u8 = 0x04;
const FHD_DICT_ID_FLAG_MASK: u8 = 0x03;
const FHD_CONTENT_SIZE_FLAG_MASK: u8 = 0xC0;

/// Zstandard frame header.
#[derive(Debug, Clone)]
pub struct FrameHeader {
    /// Window size for decompression buffer.
    pub window_size: usize,
    /// Uncompressed content size (if known).
    pub content_size: Option<u64>,
    /// Dictionary ID (if present).
    #[allow(dead_code)]
    pub dict_id: Option<u32>,
    /// Whether content checksum is present.
    pub has_checksum: bool,
    /// Header size in bytes.
    pub header_size: usize,
}

/// Parse frame header.
pub fn parse_frame_header(data: &[u8]) -> Result<FrameHeader> {
    if data.len() < 5 {
        return Err(OxiArcError::CorruptedData {
            offset: 0,
            message: "truncated frame header".to_string(),
        });
    }

    // Check magic
    if data[0..4] != ZSTD_MAGIC {
        return Err(OxiArcError::invalid_magic(ZSTD_MAGIC, &data[0..4]));
    }

    let descriptor = data[4];
    let single_segment = (descriptor & FHD_SINGLE_SEGMENT) != 0;
    let has_checksum = (descriptor & FHD_CONTENT_CHECKSUM) != 0;
    let dict_id_flag = descriptor & FHD_DICT_ID_FLAG_MASK;
    let content_size_flag = (descriptor & FHD_CONTENT_SIZE_FLAG_MASK) >> 6;

    let mut pos = 5;

    // Window descriptor (absent if single segment)
    let window_size = if single_segment {
        0 // Will be determined from content size
    } else {
        if data.len() <= pos {
            return Err(OxiArcError::CorruptedData {
                offset: pos as u64,
                message: "missing window descriptor".to_string(),
            });
        }
        let wd = data[pos];
        pos += 1;

        let exponent = (wd >> 3) as u32;
        let mantissa = (wd & 0x07) as u32;
        let base = 1u64 << (10 + exponent);
        let window = base + (base >> 3) * mantissa as u64;
        window.min(MAX_WINDOW_SIZE as u64) as usize
    };

    // Dictionary ID
    let dict_id = match dict_id_flag {
        0 => None,
        1 => {
            if data.len() <= pos {
                return Err(OxiArcError::CorruptedData {
                    offset: pos as u64,
                    message: "missing dictionary ID".to_string(),
                });
            }
            let id = data[pos] as u32;
            pos += 1;
            Some(id)
        }
        2 => {
            if data.len() < pos + 2 {
                return Err(OxiArcError::CorruptedData {
                    offset: pos as u64,
                    message: "truncated dictionary ID".to_string(),
                });
            }
            let id = u16::from_le_bytes([data[pos], data[pos + 1]]) as u32;
            pos += 2;
            Some(id)
        }
        3 => {
            if data.len() < pos + 4 {
                return Err(OxiArcError::CorruptedData {
                    offset: pos as u64,
                    message: "truncated dictionary ID".to_string(),
                });
            }
            let id = u32::from_le_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]);
            pos += 4;
            Some(id)
        }
        _ => unreachable!(),
    };

    // Content size
    let content_size = if single_segment || content_size_flag != 0 {
        let size_bytes = match content_size_flag {
            0 => 1, // Single segment implies 1 byte
            1 => 2,
            2 => 4,
            3 => 8,
            _ => unreachable!(),
        };

        if data.len() < pos + size_bytes {
            return Err(OxiArcError::CorruptedData {
                offset: pos as u64,
                message: "truncated content size".to_string(),
            });
        }

        let size = match size_bytes {
            1 => data[pos] as u64,
            2 => {
                let s = u16::from_le_bytes([data[pos], data[pos + 1]]) as u64;
                s + 256 // Add 256 for 2-byte size
            }
            4 => {
                u32::from_le_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]) as u64
            }
            8 => u64::from_le_bytes([
                data[pos],
                data[pos + 1],
                data[pos + 2],
                data[pos + 3],
                data[pos + 4],
                data[pos + 5],
                data[pos + 6],
                data[pos + 7],
            ]),
            _ => unreachable!(),
        };
        pos += size_bytes;
        Some(size)
    } else {
        None
    };

    // Adjust window size for single segment
    let window_size = if single_segment {
        content_size
            .unwrap_or(MAX_WINDOW_SIZE as u64)
            .min(MAX_WINDOW_SIZE as u64) as usize
    } else {
        window_size
    };

    Ok(FrameHeader {
        window_size,
        content_size,
        dict_id,
        has_checksum,
        header_size: pos,
    })
}

/// Zstandard decoder.
pub struct ZstdDecoder {
    /// Literals decoder.
    literals_decoder: LiteralsDecoder,
    /// Sequences decoder.
    sequences_decoder: SequencesDecoder,
    /// Output buffer (sliding window).
    output: Vec<u8>,
    /// Window size.
    window_size: usize,
}

impl ZstdDecoder {
    /// Create a new decoder.
    pub fn new() -> Self {
        Self {
            literals_decoder: LiteralsDecoder::new(),
            sequences_decoder: SequencesDecoder::new(),
            output: Vec::new(),
            window_size: MAX_WINDOW_SIZE,
        }
    }

    /// Decode a complete Zstandard frame.
    pub fn decode_frame(&mut self, data: &[u8]) -> Result<Vec<u8>> {
        let header = parse_frame_header(data)?;
        self.window_size = header.window_size;

        // Reserve space for output
        if let Some(size) = header.content_size {
            self.output.reserve(size as usize);
        }

        let mut pos = header.header_size;

        // Decode blocks
        loop {
            if data.len() < pos + 3 {
                return Err(OxiArcError::CorruptedData {
                    offset: pos as u64,
                    message: "truncated block header".to_string(),
                });
            }

            // Read block header (3 bytes, little-endian)
            let block_header = u32::from_le_bytes([data[pos], data[pos + 1], data[pos + 2], 0]);
            pos += 3;

            let last_block = (block_header & 1) != 0;
            let block_type = BlockType::from_bits(((block_header >> 1) & 0x03) as u8)?;
            let block_size = ((block_header >> 3) & 0x1FFFFF) as usize;

            if block_size > MAX_BLOCK_SIZE {
                return Err(OxiArcError::CorruptedData {
                    offset: pos as u64,
                    message: format!("block size {} exceeds maximum", block_size),
                });
            }

            // For RLE blocks, block_size is the regenerated size and only 1 byte of data follows
            let compressed_size = match block_type {
                BlockType::Rle => 1,
                _ => block_size,
            };

            if data.len() < pos + compressed_size {
                return Err(OxiArcError::CorruptedData {
                    offset: pos as u64,
                    message: "truncated block data".to_string(),
                });
            }

            let block_data = &data[pos..pos + compressed_size];
            pos += compressed_size;

            match block_type {
                BlockType::Raw => {
                    self.output.extend_from_slice(block_data);
                }
                BlockType::Rle => {
                    // block_size is the regenerated size for RLE
                    // The actual data is just 1 byte
                    self.output
                        .extend(std::iter::repeat_n(block_data[0], block_size));
                }
                BlockType::Compressed => {
                    self.decode_compressed_block(block_data)?;
                }
                BlockType::Reserved => {
                    return Err(OxiArcError::CorruptedData {
                        offset: pos as u64,
                        message: "reserved block type".to_string(),
                    });
                }
            }

            if last_block {
                break;
            }
        }

        // Verify checksum if present
        if header.has_checksum {
            if data.len() < pos + 4 {
                return Err(OxiArcError::CorruptedData {
                    offset: pos as u64,
                    message: "missing content checksum".to_string(),
                });
            }

            let expected =
                u32::from_le_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]);
            let computed = xxhash64_checksum(&self.output);

            if expected != computed {
                return Err(OxiArcError::CrcMismatch { expected, computed });
            }
        }

        // Verify content size if known
        if let Some(expected_size) = header.content_size {
            if self.output.len() as u64 != expected_size {
                return Err(OxiArcError::CorruptedData {
                    offset: 0,
                    message: format!(
                        "content size mismatch: expected {}, got {}",
                        expected_size,
                        self.output.len()
                    ),
                });
            }
        }

        Ok(std::mem::take(&mut self.output))
    }

    /// Decode a compressed block.
    fn decode_compressed_block(&mut self, data: &[u8]) -> Result<()> {
        // Decode literals
        let (literals, literals_size) = self.literals_decoder.decode(data)?;

        // Decode sequences
        let sequences_data = &data[literals_size..];
        let (sequences, _) = self.sequences_decoder.decode(sequences_data)?;

        // Execute sequences
        self.execute_sequences(&literals, &sequences)?;

        Ok(())
    }

    /// Execute sequences to produce output.
    fn execute_sequences(&mut self, literals: &[u8], sequences: &[Sequence]) -> Result<()> {
        let mut lit_pos = 0;

        for seq in sequences {
            // Copy literals
            if seq.literal_length > 0 {
                if lit_pos + seq.literal_length > literals.len() {
                    return Err(OxiArcError::CorruptedData {
                        offset: 0,
                        message: "literal length exceeds available literals".to_string(),
                    });
                }
                self.output
                    .extend_from_slice(&literals[lit_pos..lit_pos + seq.literal_length]);
                lit_pos += seq.literal_length;
            }

            // Copy match
            if seq.match_length > 0 {
                if seq.offset == 0 || seq.offset > self.output.len() {
                    return Err(OxiArcError::CorruptedData {
                        offset: 0,
                        message: format!(
                            "invalid offset {} (output length {})",
                            seq.offset,
                            self.output.len()
                        ),
                    });
                }

                let start = self.output.len() - seq.offset;

                // Handle overlapping copies
                for i in 0..seq.match_length {
                    let byte = self.output[start + (i % seq.offset)];
                    self.output.push(byte);
                }
            }
        }

        // Copy remaining literals
        if lit_pos < literals.len() {
            self.output.extend_from_slice(&literals[lit_pos..]);
        }

        Ok(())
    }

    /// Reset decoder state for a new frame.
    pub fn reset(&mut self) {
        self.output.clear();
        self.sequences_decoder.reset();
    }
}

impl Default for ZstdDecoder {
    fn default() -> Self {
        Self::new()
    }
}

/// Decompress Zstandard data.
pub fn decompress(data: &[u8]) -> Result<Vec<u8>> {
    let mut decoder = ZstdDecoder::new();
    decoder.decode_frame(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_frame_header_minimal() {
        // Minimal frame: magic + descriptor (single segment, 1 byte content size)
        let mut data = Vec::new();
        data.extend_from_slice(&ZSTD_MAGIC);
        data.push(0x20); // Single segment flag
        data.push(5); // Content size = 5

        let header = parse_frame_header(&data).unwrap();

        assert_eq!(header.content_size, Some(5));
        assert!(!header.has_checksum);
        assert!(header.dict_id.is_none());
    }

    #[test]
    fn test_parse_frame_header_with_checksum() {
        let mut data = Vec::new();
        data.extend_from_slice(&ZSTD_MAGIC);
        data.push(0x24); // Single segment + checksum
        data.push(10); // Content size = 10

        let header = parse_frame_header(&data).unwrap();

        assert!(header.has_checksum);
        assert_eq!(header.content_size, Some(10));
    }

    #[test]
    fn test_invalid_magic() {
        let data = [0x00, 0x00, 0x00, 0x00, 0x00];
        let result = parse_frame_header(&data);
        assert!(result.is_err());
    }

    #[test]
    fn test_decoder_creation() {
        let decoder = ZstdDecoder::new();
        assert_eq!(decoder.window_size, MAX_WINDOW_SIZE);
    }

    #[test]
    fn test_block_type_parsing() {
        assert_eq!(BlockType::from_bits(0).unwrap(), BlockType::Raw);
        assert_eq!(BlockType::from_bits(1).unwrap(), BlockType::Rle);
        assert_eq!(BlockType::from_bits(2).unwrap(), BlockType::Compressed);
        assert!(BlockType::from_bits(3).is_err());
    }
}
