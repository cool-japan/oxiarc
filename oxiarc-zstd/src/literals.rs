//! Literals section decoding for Zstandard.
//!
//! The literals section contains literal bytes that are copied directly
//! to the output, either uncompressed or Huffman-encoded.

use crate::LiteralsBlockType;
use crate::huffman::{HuffmanBitReader, HuffmanTable, read_huffman_table};
use oxiarc_core::error::{OxiArcError, Result};

/// Decoded literals section header.
#[derive(Debug)]
pub struct LiteralsHeader {
    /// Block type.
    pub block_type: LiteralsBlockType,
    /// Regenerated (uncompressed) size.
    pub regenerated_size: usize,
    /// Compressed size (for compressed types).
    pub compressed_size: usize,
    /// Number of streams (1 or 4).
    pub num_streams: usize,
    /// Header size in bytes.
    pub header_size: usize,
}

/// Parse literals section header.
pub fn parse_literals_header(data: &[u8]) -> Result<LiteralsHeader> {
    if data.is_empty() {
        return Err(OxiArcError::CorruptedData {
            offset: 0,
            message: "empty literals section".to_string(),
        });
    }

    let byte0 = data[0];
    let block_type = LiteralsBlockType::from_bits(byte0 & 0x03);
    let size_format = (byte0 >> 2) & 0x03;

    match block_type {
        LiteralsBlockType::Raw | LiteralsBlockType::Rle => {
            // Raw and RLE: only regenerated size, no compressed size
            let (regenerated_size, header_size) = match size_format {
                0 | 2 => {
                    // 5 bits, 1 byte header
                    if data.is_empty() {
                        return Err(OxiArcError::CorruptedData {
                            offset: 0,
                            message: "truncated literals header".to_string(),
                        });
                    }
                    ((byte0 >> 3) as usize, 1)
                }
                1 => {
                    // 12 bits, 2 byte header
                    if data.len() < 2 {
                        return Err(OxiArcError::CorruptedData {
                            offset: 0,
                            message: "truncated literals header".to_string(),
                        });
                    }
                    let size = ((byte0 >> 4) as usize) | ((data[1] as usize) << 4);
                    (size, 2)
                }
                3 => {
                    // 20 bits, 3 byte header
                    if data.len() < 3 {
                        return Err(OxiArcError::CorruptedData {
                            offset: 0,
                            message: "truncated literals header".to_string(),
                        });
                    }
                    let size = ((byte0 >> 4) as usize)
                        | ((data[1] as usize) << 4)
                        | ((data[2] as usize) << 12);
                    (size, 3)
                }
                _ => unreachable!(),
            };

            Ok(LiteralsHeader {
                block_type,
                regenerated_size,
                compressed_size: if block_type == LiteralsBlockType::Rle {
                    1
                } else {
                    regenerated_size
                },
                num_streams: 1,
                header_size,
            })
        }
        LiteralsBlockType::Compressed | LiteralsBlockType::Treeless => {
            // Compressed: both sizes, possibly 4 streams
            let (regenerated_size, compressed_size, num_streams, header_size) = match size_format {
                0 => {
                    // Single stream, 10 bits each, 3 byte header
                    if data.len() < 3 {
                        return Err(OxiArcError::CorruptedData {
                            offset: 0,
                            message: "truncated compressed literals header".to_string(),
                        });
                    }
                    let combined =
                        ((byte0 >> 4) as u32) | ((data[1] as u32) << 4) | ((data[2] as u32) << 12);
                    let regen = (combined & 0x3FF) as usize;
                    let comp = ((combined >> 10) & 0x3FF) as usize;
                    (regen, comp, 1, 3)
                }
                1 => {
                    // 4 streams, 10 bits each, 3 byte header
                    if data.len() < 3 {
                        return Err(OxiArcError::CorruptedData {
                            offset: 0,
                            message: "truncated compressed literals header".to_string(),
                        });
                    }
                    let combined =
                        ((byte0 >> 4) as u32) | ((data[1] as u32) << 4) | ((data[2] as u32) << 12);
                    let regen = (combined & 0x3FF) as usize;
                    let comp = ((combined >> 10) & 0x3FF) as usize;
                    (regen, comp, 4, 3)
                }
                2 => {
                    // 4 streams, 14 bits each, 4 byte header
                    if data.len() < 4 {
                        return Err(OxiArcError::CorruptedData {
                            offset: 0,
                            message: "truncated compressed literals header".to_string(),
                        });
                    }
                    let combined = ((byte0 >> 4) as u32)
                        | ((data[1] as u32) << 4)
                        | ((data[2] as u32) << 12)
                        | ((data[3] as u32) << 20);
                    let regen = (combined & 0x3FFF) as usize;
                    let comp = ((combined >> 14) & 0x3FFF) as usize;
                    (regen, comp, 4, 4)
                }
                3 => {
                    // 4 streams, 18 bits each, 5 byte header
                    if data.len() < 5 {
                        return Err(OxiArcError::CorruptedData {
                            offset: 0,
                            message: "truncated compressed literals header".to_string(),
                        });
                    }
                    let combined = ((byte0 >> 4) as u64)
                        | ((data[1] as u64) << 4)
                        | ((data[2] as u64) << 12)
                        | ((data[3] as u64) << 20)
                        | ((data[4] as u64) << 28);
                    let regen = (combined & 0x3FFFF) as usize;
                    let comp = ((combined >> 18) & 0x3FFFF) as usize;
                    (regen, comp, 4, 5)
                }
                _ => unreachable!(),
            };

            Ok(LiteralsHeader {
                block_type,
                regenerated_size,
                compressed_size,
                num_streams,
                header_size,
            })
        }
    }
}

/// Literals decoder state.
pub struct LiteralsDecoder {
    /// Huffman table for compressed literals.
    huffman_table: Option<HuffmanTable>,
}

impl LiteralsDecoder {
    /// Create a new literals decoder.
    pub fn new() -> Self {
        Self {
            huffman_table: None,
        }
    }

    /// Decode literals section.
    pub fn decode(&mut self, data: &[u8]) -> Result<(Vec<u8>, usize)> {
        let header = parse_literals_header(data)?;
        let content = &data[header.header_size..];

        match header.block_type {
            LiteralsBlockType::Raw => {
                // Copy bytes directly
                if content.len() < header.regenerated_size {
                    return Err(OxiArcError::CorruptedData {
                        offset: 0,
                        message: "truncated raw literals".to_string(),
                    });
                }
                let literals = content[..header.regenerated_size].to_vec();
                Ok((literals, header.header_size + header.regenerated_size))
            }
            LiteralsBlockType::Rle => {
                // Repeat single byte
                if content.is_empty() {
                    return Err(OxiArcError::CorruptedData {
                        offset: 0,
                        message: "missing RLE byte".to_string(),
                    });
                }
                let literals = vec![content[0]; header.regenerated_size];
                Ok((literals, header.header_size + 1))
            }
            LiteralsBlockType::Compressed => {
                // Decode Huffman table then decompress
                if content.len() < header.compressed_size {
                    return Err(OxiArcError::CorruptedData {
                        offset: 0,
                        message: "truncated compressed literals".to_string(),
                    });
                }

                let (table, table_size) = read_huffman_table(content)?;
                self.huffman_table = Some(table);

                let stream_data = &content[table_size..header.compressed_size];
                let literals = self.decode_huffman_streams(
                    stream_data,
                    header.regenerated_size,
                    header.num_streams,
                )?;

                Ok((literals, header.header_size + header.compressed_size))
            }
            LiteralsBlockType::Treeless => {
                // Use previous Huffman table
                if self.huffman_table.is_none() {
                    return Err(OxiArcError::CorruptedData {
                        offset: 0,
                        message: "treeless literals without previous table".to_string(),
                    });
                }

                if content.len() < header.compressed_size {
                    return Err(OxiArcError::CorruptedData {
                        offset: 0,
                        message: "truncated treeless literals".to_string(),
                    });
                }

                let stream_data = &content[..header.compressed_size];
                let literals = self.decode_huffman_streams(
                    stream_data,
                    header.regenerated_size,
                    header.num_streams,
                )?;

                Ok((literals, header.header_size + header.compressed_size))
            }
        }
    }

    /// Decode Huffman-compressed streams.
    fn decode_huffman_streams(
        &self,
        data: &[u8],
        total_size: usize,
        num_streams: usize,
    ) -> Result<Vec<u8>> {
        let table = self
            .huffman_table
            .as_ref()
            .ok_or_else(|| OxiArcError::CorruptedData {
                offset: 0,
                message: "no Huffman table".to_string(),
            })?;

        if num_streams == 1 {
            // Single stream
            self.decode_single_stream(data, total_size, table)
        } else {
            // 4 streams with jump table
            self.decode_four_streams(data, total_size, table)
        }
    }

    /// Decode a single Huffman stream.
    fn decode_single_stream(
        &self,
        data: &[u8],
        size: usize,
        table: &HuffmanTable,
    ) -> Result<Vec<u8>> {
        let mut reader = HuffmanBitReader::new(data)?;
        let mut output = Vec::with_capacity(size);

        while output.len() < size {
            let bits = reader.peek_bits(table.max_bits());
            let entry = table.decode(bits);
            output.push(entry.symbol);
            reader.consume(entry.num_bits);
        }

        Ok(output)
    }

    /// Decode four interleaved Huffman streams.
    fn decode_four_streams(
        &self,
        data: &[u8],
        total_size: usize,
        table: &HuffmanTable,
    ) -> Result<Vec<u8>> {
        // Read jump table (6 bytes: 3 x 2-byte offsets)
        if data.len() < 6 {
            return Err(OxiArcError::CorruptedData {
                offset: 0,
                message: "truncated 4-stream jump table".to_string(),
            });
        }

        let jump1 = u16::from_le_bytes([data[0], data[1]]) as usize;
        let jump2 = u16::from_le_bytes([data[2], data[3]]) as usize;
        let jump3 = u16::from_le_bytes([data[4], data[5]]) as usize;

        let stream_data = &data[6..];

        // Validate jumps
        if jump1 > stream_data.len() || jump2 > stream_data.len() || jump3 > stream_data.len() {
            return Err(OxiArcError::CorruptedData {
                offset: 0,
                message: "invalid stream jump offsets".to_string(),
            });
        }

        // Split into 4 streams
        let stream1 = &stream_data[..jump1];
        let stream2 = &stream_data[jump1..jump2];
        let stream3 = &stream_data[jump2..jump3];
        let stream4 = &stream_data[jump3..];

        // Each stream produces roughly 1/4 of the output
        let quarter = total_size.div_ceil(4);
        let size1 = quarter;
        let size2 = quarter;
        let size3 = quarter;
        let size4 = total_size - size1 - size2 - size3;

        // Decode each stream
        let mut output = Vec::with_capacity(total_size);
        output.extend(self.decode_single_stream(stream1, size1, table)?);
        output.extend(self.decode_single_stream(stream2, size2, table)?);
        output.extend(self.decode_single_stream(stream3, size3, table)?);
        output.extend(self.decode_single_stream(stream4, size4, table)?);

        Ok(output)
    }
}

impl Default for LiteralsDecoder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_raw_literals_small() {
        // Raw literals, size format 0/2, 5 bits size
        let data = [0b00001000]; // type=0 (raw), size_format=0, size=1
        let header = parse_literals_header(&data).unwrap();

        assert_eq!(header.block_type, LiteralsBlockType::Raw);
        assert_eq!(header.regenerated_size, 1);
        assert_eq!(header.header_size, 1);
    }

    #[test]
    fn test_parse_rle_literals() {
        // RLE literals
        let data = [0b00001001]; // type=1 (RLE), size_format=0, size=1
        let header = parse_literals_header(&data).unwrap();

        assert_eq!(header.block_type, LiteralsBlockType::Rle);
        assert_eq!(header.regenerated_size, 1);
        assert_eq!(header.compressed_size, 1);
    }

    #[test]
    fn test_decode_raw_literals() {
        // Raw literals with actual data
        let mut data = vec![0b00101000]; // type=0, size_format=0, size=5
        data.extend_from_slice(b"Hello");

        let mut decoder = LiteralsDecoder::new();
        let (literals, consumed) = decoder.decode(&data).unwrap();

        assert_eq!(literals, b"Hello");
        assert_eq!(consumed, 6);
    }

    #[test]
    fn test_decode_rle_literals() {
        // RLE: repeat 'A' 5 times
        let data = [0b00101001, b'A']; // type=1, size=5, byte='A'

        let mut decoder = LiteralsDecoder::new();
        let (literals, consumed) = decoder.decode(&data).unwrap();

        assert_eq!(literals, vec![b'A'; 5]);
        assert_eq!(consumed, 2);
    }
}
