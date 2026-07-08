//! BZip2 decoder.
//!
//! Implements the bzip2 stream format as produced by libbz2: MSB-first bit
//! stream, bzip2-specific block CRC (non-reflected polynomial `0x04C11DB7`),
//! symbol map of *used byte values*, MTF over the used-byte list, RUNA/RUNB
//! zero-run coding in bijective base 2, 2-6 Huffman tables with MTF-coded
//! selectors every 50 symbols, and an end-of-block symbol at
//! `alphabet_size - 1` where `alphabet_size = used_symbols + 2`.

use crate::bitio::MsbBitReader;
use crate::crc::Bz2Crc;
use crate::{BZIP2_MAGIC, bwt, huffman, rle};
use oxiarc_core::cancel::CancellationToken;
use oxiarc_core::error::{OxiArcError, Result};
use oxiarc_core::progress::ProgressHandle;
use std::io::Read;

/// Block magic as a 48-bit value (BCD digits of pi).
const BLOCK_MAGIC_BITS: u64 = 0x3141_5926_5359;

/// End-of-stream magic as a 48-bit value (BCD digits of sqrt(pi)).
const EOS_MAGIC_BITS: u64 = 0x1772_4538_5090;

/// BZip2 decoder.
///
/// Supports optional progress reporting via [`ProgressHandle`] and
/// cooperative cancellation via [`CancellationToken`] using the
/// [`BzDecoder::with_progress`] / [`BzDecoder::with_cancel`] builders.
pub struct BzDecoder<R: Read> {
    reader: MsbBitReader<R>,
    block_size: usize,
    combined_crc: u32,
    finished: bool,
    /// Optional progress sink. Notified with cumulative decompressed bytes
    /// after each block is produced.
    progress: Option<ProgressHandle>,
    /// Optional cancellation token. Checked before each block is read.
    cancel: Option<CancellationToken>,
    /// Cumulative decompressed bytes produced so far.
    bytes_processed: u64,
    /// Reusable block CRC calculator.
    crc: Bz2Crc,
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
            reader: MsbBitReader::new(reader),
            block_size,
            combined_crc: 0,
            finished: false,
            progress: None,
            cancel: None,
            bytes_processed: 0,
            crc: Bz2Crc::new(),
        })
    }

    /// Attach a progress sink.
    ///
    /// The sink's `on_progress(cumulative_decompressed_bytes, None)` is
    /// called once per decoded block. `on_finish()` is invoked when the
    /// end-of-stream marker is processed.
    pub fn with_progress(mut self, handle: ProgressHandle) -> Self {
        self.progress = Some(handle);
        self
    }

    /// Attach a cancellation token. Checked before each block is read.
    pub fn with_cancel(mut self, token: CancellationToken) -> Self {
        self.cancel = Some(token);
        self
    }

    /// Read and decode the next block.
    pub fn read_block(&mut self) -> Result<Option<Vec<u8>>> {
        if self.finished {
            return Ok(None);
        }

        // Cooperative cancellation check before reading.
        if let Some(ref token) = self.cancel {
            token.check()?;
        }

        // Read block / end-of-stream marker (48 bits).
        let magic = self.reader.read_bits_u64(48)?;
        if magic == EOS_MAGIC_BITS {
            // Stream CRC combines all block CRCs.
            let stored_crc = self.reader.read_bits(32)?;
            if stored_crc != self.combined_crc {
                return Err(OxiArcError::crc_mismatch(stored_crc, self.combined_crc));
            }
            self.finished = true;
            if let Some(ref handle) = self.progress {
                handle.on_finish();
            }
            return Ok(None);
        }
        if magic != BLOCK_MAGIC_BITS {
            return Err(OxiArcError::invalid_header("Invalid block header"));
        }

        let block_crc = self.reader.read_bits(32)?;

        // Randomised blocks (deprecated since bzip2 0.9.5) are not produced
        // by any modern encoder; reject them explicitly.
        if self.reader.read_bits(1)? != 0 {
            return Err(OxiArcError::unsupported_method(
                "randomised BZip2 block (deprecated format)",
            ));
        }

        let orig_ptr = self.reader.read_bits(24)? as usize;

        // Symbol map: 16-bit group map, then one 16-bit map per used group.
        // The bits describe which *byte values* occur in the BWT string.
        let used_groups = self.reader.read_bits(16)?;
        let mut used_symbols: Vec<u8> = Vec::new();
        for group in 0..16u32 {
            if (used_groups >> (15 - group)) & 1 == 1 {
                let bits = self.reader.read_bits(16)?;
                for bit in 0..16u32 {
                    if (bits >> (15 - bit)) & 1 == 1 {
                        used_symbols.push((group * 16 + bit) as u8);
                    }
                }
            }
        }
        if used_symbols.is_empty() {
            return Err(OxiArcError::corrupted(0, "BZip2 block uses no symbols"));
        }
        // Alphabet: RUNA, RUNB, one symbol per used byte except the first
        // (MTF indices 1..=used-1 map to symbols 2..=used), EOB.
        let alpha_size = used_symbols.len() + 2;
        let eob = (alpha_size - 1) as u16;

        // Number of Huffman tables (2-6) and selectors.
        let num_tables = self.reader.read_bits(3)? as usize;
        if !(huffman::MIN_TABLES..=huffman::MAX_TABLES).contains(&num_tables) {
            return Err(OxiArcError::invalid_header(
                "Invalid number of Huffman tables",
            ));
        }
        let num_selectors = self.reader.read_bits(15)? as usize;
        if num_selectors == 0 {
            return Err(OxiArcError::corrupted(0, "BZip2 block has no selectors"));
        }

        // Selectors are MTF-coded over the table indices, unary-coded.
        let mut selector_mtf: Vec<u8> = (0..num_tables as u8).collect();
        let mut selectors = Vec::with_capacity(num_selectors);
        for _ in 0..num_selectors {
            let mut index = 0usize;
            while self.reader.read_bits(1)? == 1 {
                index += 1;
                if index >= num_tables {
                    return Err(OxiArcError::corrupted(0, "Invalid selector"));
                }
            }
            let selected = selector_mtf[index];
            selector_mtf.copy_within(0..index, 1);
            selector_mtf[0] = selected;
            selectors.push(selected as usize);
        }

        // Delta-coded code lengths: `alpha_size` lengths per table.
        let mut tables = Vec::with_capacity(num_tables);
        for _ in 0..num_tables {
            let mut current = self.reader.read_bits(5)? as i32;
            let mut lengths = Vec::with_capacity(alpha_size);
            for _ in 0..alpha_size {
                loop {
                    if !(1..=huffman::MAX_CODE_LEN as i32).contains(&current) {
                        return Err(OxiArcError::corrupted(
                            0,
                            "BZip2 Huffman code length out of range",
                        ));
                    }
                    if self.reader.read_bits(1)? == 0 {
                        break;
                    }
                    if self.reader.read_bits(1)? == 0 {
                        current += 1;
                    } else {
                        current -= 1;
                    }
                }
                lengths.push(current as u8);
            }
            tables.push(huffman::HuffmanTable::from_lengths(&lengths)?);
        }

        // Decode the symbol stream: undo RUNA/RUNB zero runs and MTF in one
        // pass, producing the BWT string. Every decoded symbol (including
        // EOB) consumes one slot of the current 50-symbol selector group.
        let max_block = self.block_size + 10;
        let mut mtf_list = used_symbols.clone();
        let mut bwt_data: Vec<u8> = Vec::new();
        let mut group_pos = 0usize;
        let mut group_index = 0usize;
        let mut run_length: u64 = 0;
        let mut run_bit: u32 = 0;

        loop {
            if group_pos == 0 {
                let selector = *selectors
                    .get(group_index)
                    .ok_or_else(|| OxiArcError::corrupted(0, "BZip2 selectors exhausted"))?;
                if selector >= tables.len() {
                    return Err(OxiArcError::corrupted(0, "BZip2 selector out of range"));
                }
                group_index += 1;
                group_pos = huffman::SYMBOLS_PER_GROUP;
            }
            group_pos -= 1;

            let symbol = tables[selectors[group_index - 1]].decode(&mut self.reader)?;

            if symbol <= 1 {
                // RUNA (0) / RUNB (1): accumulate the zero-run length in
                // bijective base 2.
                if run_bit >= 25 {
                    return Err(OxiArcError::corrupted(0, "BZip2 zero run too long"));
                }
                run_length += u64::from(symbol + 1) << run_bit;
                run_bit += 1;
                continue;
            }

            if run_length > 0 {
                if bwt_data.len() as u64 + run_length > max_block as u64 {
                    return Err(OxiArcError::corrupted(0, "BZip2 block overflows"));
                }
                let byte = mtf_list[0];
                bwt_data.resize(bwt_data.len() + run_length as usize, byte);
                run_length = 0;
                run_bit = 0;
            }

            if symbol == eob {
                break;
            }

            // MTF decode: symbol - 1 is the move-to-front index.
            let index = (symbol - 1) as usize;
            if index >= mtf_list.len() {
                return Err(OxiArcError::corrupted(0, "BZip2 MTF index out of range"));
            }
            if bwt_data.len() >= max_block {
                return Err(OxiArcError::corrupted(0, "BZip2 block overflows"));
            }
            let byte = mtf_list[index];
            mtf_list.copy_within(0..index, 1);
            mtf_list[0] = byte;
            bwt_data.push(byte);
        }

        if orig_ptr >= bwt_data.len() {
            return Err(OxiArcError::corrupted(
                0,
                "BZip2 original pointer out of range",
            ));
        }

        // Inverse BWT, then undo the initial run-length encoding.
        let rle1_data = bwt::inverse_transform(&bwt_data, orig_ptr as u32);
        let data = rle::rle1_decode(&rle1_data)?;

        // Verify the block CRC (bzip2-specific CRC-32).
        self.crc.reset();
        self.crc.update(&data);
        let computed_crc = self.crc.finish();
        if computed_crc != block_crc {
            return Err(OxiArcError::crc_mismatch(block_crc, computed_crc));
        }

        // Update combined CRC
        self.combined_crc = self.combined_crc.rotate_left(1) ^ block_crc;

        // Update cumulative decompressed byte count and notify progress.
        self.bytes_processed = self.bytes_processed.saturating_add(data.len() as u64);
        if let Some(ref handle) = self.progress {
            handle.on_progress(self.bytes_processed, None);
        }

        Ok(Some(data))
    }

    /// Get the block size.
    pub fn block_size(&self) -> usize {
        self.block_size
    }
}

/// Decompress BZip2 data.
///
/// # Example
///
/// ```rust
/// use oxiarc_bzip2::{compress, decompress, CompressionLevel};
///
/// let data = b"Hello, World! Hello, World!";
/// let compressed = compress(data, CompressionLevel::new(9)).expect("compress");
/// let decompressed = decompress(&compressed[..]).expect("decompress");
/// assert_eq!(decompressed, data);
/// ```
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
    use crate::{BLOCK_MAGIC, EOS_MAGIC};
    use std::io::Cursor;

    #[test]
    fn test_decoder_invalid_magic() {
        let data = b"XXXX";
        let result = BzDecoder::new(Cursor::new(data));
        assert!(result.is_err());
    }

    #[test]
    fn test_magic_constants_match_bit_values() {
        // The public byte constants and the internal 48-bit values must agree.
        let mut block = 0u64;
        for &b in &BLOCK_MAGIC {
            block = (block << 8) | u64::from(b);
        }
        let mut eos = 0u64;
        for &b in &EOS_MAGIC {
            eos = (eos << 8) | u64::from(b);
        }
        assert_eq!(block, BLOCK_MAGIC_BITS);
        assert_eq!(eos, EOS_MAGIC_BITS);
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
        let decoder = decoder.expect("decoder should construct with valid header");
        assert_eq!(decoder.block_size(), 900_000);
    }

    #[test]
    fn test_decoder_empty_stream() {
        // Header + EOS + zero combined CRC decodes to empty output.
        let mut data = Vec::new();
        data.extend_from_slice(&BZIP2_MAGIC);
        data.push(b'h');
        data.push(b'1');
        data.extend_from_slice(&EOS_MAGIC);
        data.extend_from_slice(&[0, 0, 0, 0]);

        let decoded = decompress(Cursor::new(data)).expect("decode empty stream");
        assert!(decoded.is_empty());
    }

    #[test]
    fn test_decoder_with_progress_builder() {
        use crate::{CompressionLevel, compress};
        use oxiarc_core::progress::{ProgressHandle, ProgressSink};
        use std::sync::Arc;
        use std::sync::atomic::{AtomicU64, Ordering};

        struct CountingSink {
            progress_count: AtomicU64,
            finish_count: AtomicU64,
        }
        impl ProgressSink for CountingSink {
            fn on_progress(&self, _processed: u64, _total: Option<u64>) {
                self.progress_count.fetch_add(1, Ordering::SeqCst);
            }
            fn on_finish(&self) {
                self.finish_count.fetch_add(1, Ordering::SeqCst);
            }
        }

        let sink = Arc::new(CountingSink {
            progress_count: AtomicU64::new(0),
            finish_count: AtomicU64::new(0),
        });
        let handle: ProgressHandle = sink.clone();

        let original = b"progress tracking through the decoder";
        let compressed =
            compress(original, CompressionLevel::new(1)).expect("compress should succeed");
        let mut decoder = BzDecoder::new(Cursor::new(&compressed))
            .expect("decoder should construct")
            .with_progress(handle);

        let mut output = Vec::new();
        while let Some(block) = decoder.read_block().expect("read_block should succeed") {
            output.extend_from_slice(&block);
        }
        assert_eq!(output, original);
        assert!(sink.progress_count.load(Ordering::SeqCst) >= 1);
        assert_eq!(sink.finish_count.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn test_decoder_with_cancel_builder() {
        use crate::{CompressionLevel, compress};
        use oxiarc_core::cancel::CancellationToken;
        use oxiarc_core::error::OxiArcError;

        let original = b"cancel before the first block";
        let compressed =
            compress(original, CompressionLevel::new(1)).expect("compress should succeed");

        let token = CancellationToken::new();
        let mut decoder = BzDecoder::new(Cursor::new(&compressed))
            .expect("decoder should construct")
            .with_cancel(token.clone());

        token.cancel();
        let result = decoder.read_block();
        assert!(matches!(result, Err(OxiArcError::Cancelled)));
    }
}
