//! Sequences section decoding for Zstandard.
//!
//! Sequences describe LZ77-style back-references using literal lengths,
//! match lengths, and offsets.

use crate::fse::{FseBitReader, FseDecoder, FseTable, FseTableEntry, read_fse_table_description};
use oxiarc_core::error::{OxiArcError, Result};

/// A decoded sequence.
#[derive(Debug, Clone, Copy)]
pub struct Sequence {
    /// Number of literal bytes to copy.
    pub literal_length: usize,
    /// Number of bytes to copy from back-reference.
    pub match_length: usize,
    /// Offset for back-reference (or repeat offset index).
    pub offset: usize,
}

/// Sequences section header.
#[derive(Debug)]
pub struct SequencesHeader {
    /// Number of sequences.
    pub num_sequences: usize,
    /// Compression mode for literal lengths.
    pub ll_mode: CompressionMode,
    /// Compression mode for offsets.
    pub of_mode: CompressionMode,
    /// Compression mode for match lengths.
    pub ml_mode: CompressionMode,
    /// Header size in bytes.
    pub header_size: usize,
}

/// Compression mode for sequence symbols.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompressionMode {
    /// Predefined FSE table.
    Predefined,
    /// RLE (single symbol).
    Rle,
    /// FSE table in compressed form.
    Fse,
    /// Repeat previous FSE table.
    Repeat,
}

impl CompressionMode {
    /// Create from 2-bit value.
    pub fn from_bits(bits: u8) -> Self {
        match bits & 0x03 {
            0 => CompressionMode::Predefined,
            1 => CompressionMode::Rle,
            2 => CompressionMode::Fse,
            3 => CompressionMode::Repeat,
            _ => unreachable!(),
        }
    }
}

/// Parse sequences header.
pub fn parse_sequences_header(data: &[u8]) -> Result<SequencesHeader> {
    if data.is_empty() {
        return Err(OxiArcError::CorruptedData {
            offset: 0,
            message: "empty sequences section".to_string(),
        });
    }

    let byte0 = data[0];
    let (num_sequences, header_start) = if byte0 == 0 {
        (0, 1)
    } else if byte0 < 128 {
        (byte0 as usize, 1)
    } else if byte0 < 255 {
        if data.len() < 2 {
            return Err(OxiArcError::CorruptedData {
                offset: 0,
                message: "truncated sequences header".to_string(),
            });
        }
        let count = ((byte0 as usize - 128) << 8) + data[1] as usize;
        (count, 2)
    } else {
        if data.len() < 3 {
            return Err(OxiArcError::CorruptedData {
                offset: 0,
                message: "truncated sequences header".to_string(),
            });
        }
        let count = (data[1] as usize) + ((data[2] as usize) << 8) + 0x7F00;
        (count, 3)
    };

    if num_sequences == 0 {
        return Ok(SequencesHeader {
            num_sequences: 0,
            ll_mode: CompressionMode::Predefined,
            of_mode: CompressionMode::Predefined,
            ml_mode: CompressionMode::Predefined,
            header_size: header_start,
        });
    }

    if data.len() <= header_start {
        return Err(OxiArcError::CorruptedData {
            offset: 0,
            message: "missing compression modes".to_string(),
        });
    }

    let modes_byte = data[header_start];
    let ll_mode = CompressionMode::from_bits((modes_byte >> 6) & 0x03);
    let of_mode = CompressionMode::from_bits((modes_byte >> 4) & 0x03);
    let ml_mode = CompressionMode::from_bits((modes_byte >> 2) & 0x03);

    Ok(SequencesHeader {
        num_sequences,
        ll_mode,
        of_mode,
        ml_mode,
        header_size: header_start + 1,
    })
}

/// Sequences decoder.
pub struct SequencesDecoder {
    /// Literal length FSE table.
    ll_table: Option<FseTable>,
    /// Offset FSE table.
    of_table: Option<FseTable>,
    /// Match length FSE table.
    ml_table: Option<FseTable>,
    /// Repeat offsets.
    repeat_offsets: [usize; 3],
}

impl SequencesDecoder {
    /// Create a new sequences decoder.
    pub fn new() -> Self {
        Self {
            ll_table: None,
            of_table: None,
            ml_table: None,
            repeat_offsets: [1, 4, 8], // Default repeat offsets
        }
    }

    /// Decode sequences section.
    pub fn decode(&mut self, data: &[u8]) -> Result<(Vec<Sequence>, usize)> {
        let header = parse_sequences_header(data)?;

        if header.num_sequences == 0 {
            return Ok((Vec::new(), header.header_size));
        }

        let mut pos = header.header_size;

        // Read/setup FSE tables
        pos += self.setup_ll_table(&data[pos..], header.ll_mode)?;
        pos += self.setup_of_table(&data[pos..], header.of_mode)?;
        pos += self.setup_ml_table(&data[pos..], header.ml_mode)?;

        // Decode sequences from bitstream
        let bitstream = &data[pos..];
        let sequences = self.decode_sequences(bitstream, header.num_sequences)?;

        Ok((sequences, data.len()))
    }

    /// Setup literal length table.
    fn setup_ll_table(&mut self, data: &[u8], mode: CompressionMode) -> Result<usize> {
        match mode {
            CompressionMode::Predefined => {
                self.ll_table = Some(predefined_ll_table());
                Ok(0)
            }
            CompressionMode::Rle => {
                if data.is_empty() {
                    return Err(OxiArcError::CorruptedData {
                        offset: 0,
                        message: "missing RLE symbol for literal lengths".to_string(),
                    });
                }
                self.ll_table = Some(rle_table(data[0]));
                Ok(1)
            }
            CompressionMode::Fse => {
                let (table, consumed) = read_fse_table_description(data, 35)?;
                self.ll_table = Some(table);
                Ok(consumed)
            }
            CompressionMode::Repeat => {
                if self.ll_table.is_none() {
                    return Err(OxiArcError::CorruptedData {
                        offset: 0,
                        message: "repeat mode without previous table".to_string(),
                    });
                }
                Ok(0)
            }
        }
    }

    /// Setup offset table.
    fn setup_of_table(&mut self, data: &[u8], mode: CompressionMode) -> Result<usize> {
        match mode {
            CompressionMode::Predefined => {
                self.of_table = Some(predefined_of_table());
                Ok(0)
            }
            CompressionMode::Rle => {
                if data.is_empty() {
                    return Err(OxiArcError::CorruptedData {
                        offset: 0,
                        message: "missing RLE symbol for offsets".to_string(),
                    });
                }
                self.of_table = Some(rle_table(data[0]));
                Ok(1)
            }
            CompressionMode::Fse => {
                let (table, consumed) = read_fse_table_description(data, 31)?;
                self.of_table = Some(table);
                Ok(consumed)
            }
            CompressionMode::Repeat => {
                if self.of_table.is_none() {
                    return Err(OxiArcError::CorruptedData {
                        offset: 0,
                        message: "repeat mode without previous table".to_string(),
                    });
                }
                Ok(0)
            }
        }
    }

    /// Setup match length table.
    fn setup_ml_table(&mut self, data: &[u8], mode: CompressionMode) -> Result<usize> {
        match mode {
            CompressionMode::Predefined => {
                self.ml_table = Some(predefined_ml_table());
                Ok(0)
            }
            CompressionMode::Rle => {
                if data.is_empty() {
                    return Err(OxiArcError::CorruptedData {
                        offset: 0,
                        message: "missing RLE symbol for match lengths".to_string(),
                    });
                }
                self.ml_table = Some(rle_table(data[0]));
                Ok(1)
            }
            CompressionMode::Fse => {
                let (table, consumed) = read_fse_table_description(data, 52)?;
                self.ml_table = Some(table);
                Ok(consumed)
            }
            CompressionMode::Repeat => {
                if self.ml_table.is_none() {
                    return Err(OxiArcError::CorruptedData {
                        offset: 0,
                        message: "repeat mode without previous table".to_string(),
                    });
                }
                Ok(0)
            }
        }
    }

    /// Decode sequences from bitstream.
    fn decode_sequences(&mut self, data: &[u8], count: usize) -> Result<Vec<Sequence>> {
        // Check tables exist first
        if self.ll_table.is_none() {
            return Err(OxiArcError::CorruptedData {
                offset: 0,
                message: "missing literal length table".to_string(),
            });
        }
        if self.of_table.is_none() {
            return Err(OxiArcError::CorruptedData {
                offset: 0,
                message: "missing offset table".to_string(),
            });
        }
        if self.ml_table.is_none() {
            return Err(OxiArcError::CorruptedData {
                offset: 0,
                message: "missing match length table".to_string(),
            });
        }

        let mut reader = FseBitReader::new(data)?;

        // Initialize FSE decoders - tables checked above
        let ll_table = self
            .ll_table
            .as_ref()
            .ok_or_else(|| OxiArcError::corrupted(0, "missing literal length table"))?;
        let of_table = self
            .of_table
            .as_ref()
            .ok_or_else(|| OxiArcError::corrupted(0, "missing offset table"))?;
        let ml_table = self
            .ml_table
            .as_ref()
            .ok_or_else(|| OxiArcError::corrupted(0, "missing match length table"))?;

        let mut ll_decoder = FseDecoder::new(ll_table, &mut reader);
        let mut of_decoder = FseDecoder::new(of_table, &mut reader);
        let mut ml_decoder = FseDecoder::new(ml_table, &mut reader);

        let mut sequences = Vec::with_capacity(count);

        for _ in 0..count {
            // Decode in order: offset, match length, literal length
            let of_code = of_decoder.decode(&mut reader);
            let ml_code = ml_decoder.decode(&mut reader);
            let ll_code = ll_decoder.decode(&mut reader);

            // Convert codes to values
            let ll_value = decode_ll_value(ll_code, &mut reader)?;
            let ml_value = decode_ml_value(ml_code, &mut reader)?;
            let offset = decode_offset(of_code, ll_value, &mut self.repeat_offsets, &mut reader)?;

            sequences.push(Sequence {
                literal_length: ll_value,
                match_length: ml_value,
                offset,
            });
        }

        Ok(sequences)
    }

    /// Reset repeat offsets (for new frame).
    pub fn reset(&mut self) {
        self.repeat_offsets = [1, 4, 8];
    }
}

/// Decode offset with repeat offset handling.
fn decode_offset(
    code: u8,
    literal_length: usize,
    repeat_offsets: &mut [usize; 3],
    reader: &mut FseBitReader,
) -> Result<usize> {
    if code == 0 {
        // Use repeat offset
        let offset = if literal_length == 0 {
            // Special case: offset = repeat_offset[1]
            repeat_offsets.swap(0, 1);
            repeat_offsets[0]
        } else {
            repeat_offsets[0]
        };
        Ok(offset)
    } else if code <= 3 && literal_length == 0 {
        // Repeat offset with adjustment
        let idx = code as usize;
        let offset = if idx <= 2 {
            repeat_offsets[idx]
        } else {
            repeat_offsets[0] - 1
        };

        // Update repeat offsets
        if idx != 0 {
            let temp = repeat_offsets[idx];
            for i in (1..=idx).rev() {
                repeat_offsets[i] = repeat_offsets[i - 1];
            }
            repeat_offsets[0] = temp;
        }

        Ok(offset)
    } else {
        // Regular offset
        let extra_bits = code.saturating_sub(1);
        let extra = reader.read_bits(extra_bits);
        let offset = (1 << code) + extra as usize;

        // Update repeat offsets
        repeat_offsets[2] = repeat_offsets[1];
        repeat_offsets[1] = repeat_offsets[0];
        repeat_offsets[0] = offset;

        Ok(offset)
    }
}

impl Default for SequencesDecoder {
    fn default() -> Self {
        Self::new()
    }
}

/// Decode literal length value from code and extra bits.
fn decode_ll_value(code: u8, reader: &mut FseBitReader) -> Result<usize> {
    // Literal length baseline and extra bits table
    const LL_BASELINE: [u32; 36] = [
        0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 18, 20, 22, 24, 28, 32, 40, 48,
        64, 128, 256, 512, 1024, 2048, 4096, 8192, 16384, 32768, 65536,
    ];
    const LL_EXTRA: [u8; 36] = [
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 3, 3, 4, 6, 7, 8, 9, 10,
        11, 12, 13, 14, 15, 16,
    ];

    let idx = code as usize;
    if idx >= LL_BASELINE.len() {
        return Err(OxiArcError::CorruptedData {
            offset: 0,
            message: format!("invalid literal length code: {}", code),
        });
    }

    let extra = reader.read_bits(LL_EXTRA[idx]);
    Ok(LL_BASELINE[idx] as usize + extra as usize)
}

/// Decode match length value from code and extra bits.
fn decode_ml_value(code: u8, reader: &mut FseBitReader) -> Result<usize> {
    // Match length baseline and extra bits table
    const ML_BASELINE: [u32; 53] = [
        3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26,
        27, 28, 29, 30, 31, 32, 33, 34, 35, 37, 39, 41, 43, 47, 51, 59, 67, 83, 99, 131, 259, 515,
        1027, 2051, 4099, 8195, 16387, 32771, 65539,
    ];
    const ML_EXTRA: [u8; 53] = [
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 1, 1, 1, 1, 2, 2, 3, 3, 4, 4, 5, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16,
    ];

    let idx = code as usize;
    if idx >= ML_BASELINE.len() {
        return Err(OxiArcError::CorruptedData {
            offset: 0,
            message: format!("invalid match length code: {}", code),
        });
    }

    let extra = reader.read_bits(ML_EXTRA[idx]);
    Ok(ML_BASELINE[idx] as usize + extra as usize)
}

/// Create RLE FSE table (single symbol).
fn rle_table(symbol: u8) -> FseTable {
    let entries = vec![FseTableEntry {
        symbol,
        num_bits: 0,
        baseline: 0,
    }];
    FseTable::from_entries(0, entries)
}

/// Create predefined literal length FSE table.
fn predefined_ll_table() -> FseTable {
    // Predefined distribution for literal lengths (accuracy log 6)
    let probs = [
        4i16, 3, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 1, 1, 1, 2, 2, 2, 2, 2, 2, 2, 2, 2, 3, 2, 1, 1,
        1, 1, 1, -1, -1, -1, -1,
    ];
    FseTable::new(6, &probs).expect("Predefined literal length FSE table should always be valid")
}

/// Create predefined offset FSE table.
fn predefined_of_table() -> FseTable {
    // Predefined distribution for offsets (accuracy log 5)
    let probs = [
        1i16, 1, 1, 1, 1, 1, 2, 2, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, -1, -1, -1, -1,
        -1, -1, -1, -1,
    ];
    FseTable::new(5, &probs).expect("Predefined offset FSE table should always be valid")
}

/// Create predefined match length FSE table.
fn predefined_ml_table() -> FseTable {
    // Predefined distribution for match lengths (accuracy log 6)
    let probs = [
        1i16, 4, 3, 2, 2, 2, 2, 2, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
        1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, -1, -1, -1, -1, -1, -1, -1,
    ];
    FseTable::new(6, &probs).expect("Predefined match length FSE table should always be valid")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compression_mode_from_bits() {
        assert_eq!(CompressionMode::from_bits(0), CompressionMode::Predefined);
        assert_eq!(CompressionMode::from_bits(1), CompressionMode::Rle);
        assert_eq!(CompressionMode::from_bits(2), CompressionMode::Fse);
        assert_eq!(CompressionMode::from_bits(3), CompressionMode::Repeat);
    }

    #[test]
    fn test_parse_zero_sequences() {
        let data = [0];
        let header = parse_sequences_header(&data).unwrap();
        assert_eq!(header.num_sequences, 0);
    }

    #[test]
    fn test_parse_small_sequence_count() {
        let data = [10, 0b00000000]; // 10 sequences, all predefined
        let header = parse_sequences_header(&data).unwrap();
        assert_eq!(header.num_sequences, 10);
        assert_eq!(header.ll_mode, CompressionMode::Predefined);
        assert_eq!(header.of_mode, CompressionMode::Predefined);
        assert_eq!(header.ml_mode, CompressionMode::Predefined);
    }

    #[test]
    fn test_predefined_tables() {
        let ll = predefined_ll_table();
        let of = predefined_of_table();
        let ml = predefined_ml_table();

        assert_eq!(ll.accuracy_log(), 6);
        assert_eq!(of.accuracy_log(), 5);
        assert_eq!(ml.accuracy_log(), 6);
    }

    #[test]
    fn test_rle_table() {
        let table = rle_table(42);
        assert_eq!(table.get(0).symbol, 42);
        assert_eq!(table.get(0).num_bits, 0);
    }
}
