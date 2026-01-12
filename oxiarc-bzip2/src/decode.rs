//! BZip2 decoder.

use crate::{BLOCK_MAGIC, BZIP2_MAGIC, EOS_MAGIC, bwt, huffman, mtf, rle};
use oxiarc_core::error::{OxiArcError, Result};
use oxiarc_core::{BitReader, Crc32};
use std::io::Read;

/// BZip2 decoder.
pub struct BzDecoder<R: Read> {
    reader: BitReader<R>,
    block_size: usize,
    combined_crc: u32,
    finished: bool,
}

impl<R: Read> BzDecoder<R> {
    /// Create a new decoder.
    pub fn new(mut reader: R) -> Result<Self> {
        let mut header = [0u8; 4];
        reader.read_exact(&mut header)?;

        // Check magic
        if header[0] != BZIP2_MAGIC[0] || header[1] != BZIP2_MAGIC[1] {
            return Err(OxiArcError::invalid_magic(
                BZIP2_MAGIC.to_vec(),
                header[0..2].to_vec(),
            ));
        }

        // Check 'h' marker
        if header[2] != b'h' {
            return Err(OxiArcError::invalid_header("Invalid BZip2 version marker"));
        }

        // Get block size (1-9)
        let level = header[3].saturating_sub(b'0');
        if !(1..=9).contains(&level) {
            return Err(OxiArcError::invalid_header("Invalid block size"));
        }

        let block_size = level as usize * 100_000;

        Ok(Self {
            reader: BitReader::new(reader),
            block_size,
            combined_crc: 0,
            finished: false,
        })
    }

    /// Read and decode the next block.
    pub fn read_block(&mut self) -> Result<Option<Vec<u8>>> {
        if self.finished {
            return Ok(None);
        }

        // Read block/stream marker (6 bytes as bits)
        let mut marker = [0u8; 6];
        for byte in &mut marker {
            *byte = self.reader.read_bits(8)? as u8;
        }

        // Check for end of stream
        if marker == EOS_MAGIC {
            // Read combined CRC
            let stored_crc = self.reader.read_bits(32)?;
            if stored_crc != self.combined_crc {
                return Err(OxiArcError::crc_mismatch(stored_crc, self.combined_crc));
            }
            self.finished = true;
            return Ok(None);
        }

        // Check for block magic
        if marker != BLOCK_MAGIC {
            return Err(OxiArcError::invalid_header("Invalid block header"));
        }

        // Read block CRC
        let block_crc = self.reader.read_bits(32)?;

        // Read randomised flag
        let _randomised = self.reader.read_bits(1)?;

        // Read original pointer
        let orig_ptr = self.reader.read_bits(24)?;

        // Read symbol bitmap
        let in_use_16 = self.reader.read_bits(16)? as u16;

        let mut used = [false; 256];
        for i in 0..16 {
            if (in_use_16 >> (15 - i)) & 1 == 1 {
                let group_map = self.reader.read_bits(16)? as u16;
                for j in 0..16 {
                    if (group_map >> (15 - j)) & 1 == 1 {
                        used[i * 16 + j] = true;
                    }
                }
            }
        }

        let num_symbols = used.iter().filter(|&&u| u).count() + 2; // +2 for RUNA, RUNB

        // Read number of Huffman tables
        let num_tables = self.reader.read_bits(3)? as usize;
        if !(1..=6).contains(&num_tables) {
            return Err(OxiArcError::invalid_header(
                "Invalid number of Huffman tables",
            ));
        }

        // Read number of selectors
        let num_selectors = self.reader.read_bits(15)? as usize;

        // Read selectors (MTF encoded)
        let mut selectors = Vec::with_capacity(num_selectors);
        let mut selector_mtf: Vec<u8> = (0..num_tables as u8).collect();

        for _ in 0..num_selectors {
            // Read unary-coded selector index
            let mut idx = 0;
            while self.reader.read_bits(1)? == 1 {
                idx += 1;
                if idx >= num_tables {
                    return Err(OxiArcError::corrupted(0, "Invalid selector"));
                }
            }

            // MTF decode selector
            let selected = selector_mtf[idx];
            if idx > 0 {
                selector_mtf.remove(idx);
                selector_mtf.insert(0, selected);
            }
            selectors.push(selected);
        }

        // Read Huffman tables
        let mut tables = Vec::with_capacity(num_tables);

        for _ in 0..num_tables {
            let mut lengths = Vec::with_capacity(num_symbols + 1);
            let mut current_len = self.reader.read_bits(5)? as u8;

            for _ in 0..=num_symbols {
                loop {
                    let bit = self.reader.read_bits(1)?;
                    if bit == 0 {
                        break;
                    }
                    let inc = self.reader.read_bits(1)?;
                    if inc == 0 {
                        current_len += 1;
                    } else {
                        current_len = current_len.saturating_sub(1);
                    }
                }
                lengths.push(current_len);
            }

            tables.push(huffman::HuffmanTable::from_lengths(&lengths)?);
        }

        // Decode symbols
        let mut zrle_data = Vec::new();
        let mut group_idx = 0;
        let mut symbols_in_group = 0;

        loop {
            if symbols_in_group >= huffman::SYMBOLS_PER_GROUP && group_idx < selectors.len() - 1 {
                group_idx += 1;
                symbols_in_group = 0;
            }

            let table = &tables[selectors[group_idx.min(selectors.len() - 1)] as usize];
            let sym = table.decode(&mut self.reader)?;

            if sym as usize == num_symbols {
                // End of block
                break;
            }

            zrle_data.push(sym);
            symbols_in_group += 1;
        }

        // Step 4: Decode zero-run encoding with compact symbol mapping
        let mtf_data = rle::decode_zero_runs_compact(&zrle_data, &used);

        // Step 3: Inverse MTF
        let bwt_data = mtf::inverse_transform(&mtf_data);

        // Step 2: Inverse BWT
        let rle1_data = bwt::inverse_transform(&bwt_data, orig_ptr);

        // Step 1: Decode RLE1
        let data = rle::rle1_decode(&rle1_data)?;

        // Verify CRC
        let computed_crc = Crc32::compute(&data);
        if computed_crc != block_crc {
            return Err(OxiArcError::crc_mismatch(block_crc, computed_crc));
        }

        // Update combined CRC
        self.combined_crc = self.combined_crc.rotate_left(1) ^ block_crc;

        Ok(Some(data))
    }

    /// Get the block size.
    pub fn block_size(&self) -> usize {
        self.block_size
    }
}

/// Decompress BZip2 data.
pub fn decompress<R: Read>(reader: R) -> Result<Vec<u8>> {
    let mut decoder = BzDecoder::new(reader)?;
    let mut output = Vec::new();

    while let Some(block) = decoder.read_block()? {
        output.extend_from_slice(&block);
    }

    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn test_decoder_invalid_magic() {
        let data = b"XXXX";
        let result = BzDecoder::new(Cursor::new(data));
        assert!(result.is_err());
    }

    #[test]
    fn test_decoder_header_parsing() {
        // Valid BZip2 header followed by EOS
        let mut data = Vec::new();
        data.extend_from_slice(&BZIP2_MAGIC);
        data.push(b'h');
        data.push(b'9'); // Block size 9
        data.extend_from_slice(&EOS_MAGIC);
        data.extend_from_slice(&[0, 0, 0, 0]); // Combined CRC

        let decoder = BzDecoder::new(Cursor::new(data));
        assert!(decoder.is_ok());
        let decoder = decoder.unwrap();
        assert_eq!(decoder.block_size(), 900_000);
    }
}
