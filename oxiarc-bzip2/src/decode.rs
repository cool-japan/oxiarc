//! BZip2 decoder.
//!
//! Implements the bzip2 stream format as produced by libbz2: MSB-first bit
//! stream, bzip2-specific block CRC (non-reflected polynomial `0x04C11DB7`),
//! symbol map of *used byte values*, MTF over the used-byte list, RUNA/RUNB
//! zero-run coding in bijective base 2, 2-6 Huffman tables with MTF-coded
//! selectors every 50 symbols, and an end-of-block symbol at
//! `alphabet_size - 1` where `alphabet_size = used_symbols + 2`.
//!
//! Multi-stream (concatenated) files — as produced by `pbzip2`, `lbzip2`,
//! or plain `cat a.bz2 b.bz2` — are decoded in full: after each
//! end-of-stream marker the decoder probes for a following `BZh<1-9>`
//! header and continues transparently, exactly like `bzip2 -d`. Trailing
//! bytes that are not a valid stream header are an error, never silently
//! dropped. Legacy randomised blocks (bzip2 <= 0.9.0) are de-randomised
//! with libbz2's `BZ2_rNums` schedule (`src/rand.rs`).

use crate::bitio::MsbBitReader;
use crate::crc::Bz2Crc;
use crate::{BZIP2_MAGIC, bwt, huffman, rand, rle};
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
    #[must_use]
    pub fn with_progress(mut self, handle: ProgressHandle) -> Self {
        self.progress = Some(handle);
        self
    }

    /// Attach a cancellation token. Checked before each block is read.
    #[must_use]
    pub fn with_cancel(mut self, token: CancellationToken) -> Self {
        self.cancel = Some(token);
        self
    }

    /// Read and decode the next block.
    ///
    /// Blocks from concatenated streams (`cat a.bz2 b.bz2`, pbzip2/lbzip2
    /// output) are delivered transparently: at each end-of-stream marker
    /// the combined CRC is verified and the decoder probes for another
    /// `BZh<1-9>` header, continuing with the next stream if one follows.
    /// `Ok(None)` therefore means the whole input is exhausted. Trailing
    /// bytes that do not start a valid stream header are an error.
    pub fn read_block(&mut self) -> Result<Option<Vec<u8>>> {
        self.read_block_inner(None)
    }

    /// Read and decode the next block with a decoded-output limit.
    ///
    /// The limit applies to the block returned by this call and is enforced
    /// during RLE1 reconstruction, before the decoded block grows beyond
    /// `max_output` bytes. Callers enforcing a cumulative output limit should
    /// pass their remaining budget on each call.
    pub fn read_block_with_limit(&mut self, max_output: usize) -> Result<Option<Vec<u8>>> {
        self.read_block_inner(Some(max_output))
    }

    fn read_block_inner(&mut self, max_output: Option<usize>) -> Result<Option<Vec<u8>>> {
        if self.finished {
            return Ok(None);
        }

        // Cooperative cancellation check before reading.
        if let Some(ref token) = self.cancel {
            token.check()?;
        }

        // Loop across stream boundaries: an end-of-stream marker followed
        // by a concatenated stream continues with that stream's blocks.
        loop {
            // Read block / end-of-stream marker (48 bits).
            let magic = self.reader.read_bits_u64(48)?;
            if magic == BLOCK_MAGIC_BITS {
                return self.decode_block_body(max_output).map(Some);
            }
            if magic != EOS_MAGIC_BITS {
                return Err(OxiArcError::invalid_header("Invalid block header"));
            }

            // Stream CRC combines all block CRCs of this stream.
            let stored_crc = self.reader.read_bits(32)?;
            if stored_crc != self.combined_crc {
                return Err(OxiArcError::crc_mismatch(stored_crc, self.combined_crc));
            }

            // Probe for a concatenated follow-up stream; on clean end of
            // input the decode is complete.
            if !self.begin_next_stream()? {
                self.finished = true;
                if let Some(ref handle) = self.progress {
                    handle.on_finish();
                }
                return Ok(None);
            }
        }
    }

    /// After an end-of-stream marker, check whether another concatenated
    /// bzip2 stream follows.
    ///
    /// Streams are zero-padded to whole bytes, so the reader is realigned
    /// first. Returns `Ok(false)` on clean end of input. If any bytes
    /// follow they must form a `BZh<1-9>` stream header; anything else is
    /// reported as corruption rather than silently ignored.
    fn begin_next_stream(&mut self) -> Result<bool> {
        self.reader.align_to_byte();
        let Some(first) = self.reader.try_read_aligned_byte()? else {
            return Ok(false);
        };
        let rest = [
            self.reader.read_bits(8)? as u8,
            self.reader.read_bits(8)? as u8,
            self.reader.read_bits(8)? as u8,
        ];
        if first != BZIP2_MAGIC[0] || rest[0] != BZIP2_MAGIC[1] || rest[1] != b'h' {
            return Err(OxiArcError::corrupted(
                0,
                "trailing data after BZip2 end-of-stream is not a concatenated stream",
            ));
        }
        let level = rest[2].wrapping_sub(b'0');
        if !(1..=9).contains(&level) {
            return Err(OxiArcError::invalid_header(
                "Invalid block size in concatenated BZip2 stream",
            ));
        }
        // Each stream carries its own block size and combined CRC.
        self.block_size = level as usize * 100_000;
        self.combined_crc = 0;
        Ok(true)
    }

    /// Decode one block after its 48-bit block magic has been consumed.
    fn decode_block_body(&mut self, max_output: Option<usize>) -> Result<Vec<u8>> {
        let block_crc = self.reader.read_bits(32)?;

        // Randomised blocks (deprecated since bzip2 0.9.5, produced only by
        // bzip2 <= 0.9.0) need a de-randomisation pass after the inverse
        // BWT; libbz2 still decodes them and so do we.
        let randomised = self.reader.read_bits(1)? != 0;

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

        // Inverse BWT, undo the legacy randomisation if the block used it,
        // then undo the initial run-length encoding.
        let mut rle1_data = bwt::inverse_transform(&bwt_data, orig_ptr as u32);
        if randomised {
            rand::derandomise(&mut rle1_data);
        }
        let data = if let Some(limit) = max_output {
            rle::rle1_decode_with_limit(&rle1_data, limit)?
        } else {
            rle::rle1_decode(&rle1_data)?
        };

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

        Ok(data)
    }

    /// Get the block size of the stream currently being decoded.
    ///
    /// Concatenated streams each carry their own level digit, so this
    /// value can change after a stream boundary is crossed.
    pub fn block_size(&self) -> usize {
        self.block_size
    }
}

/// Decompress BZip2 data.
///
/// Concatenated multi-stream input (pbzip2/lbzip2 output, `cat a.bz2
/// b.bz2`) is decoded in full, matching `bzip2 -d`. Trailing bytes that do
/// not form another bzip2 stream are an error.
///
/// # Memory characteristics
///
/// The whole decompressed payload is accumulated in one `Vec<u8>`. A
/// single block expands to at most ~46 MiB (900 kB of RLE1 data at the
/// maximum 255x run expansion), but a crafted file may contain arbitrarily
/// many blocks, so the total output — and therefore the allocation — is
/// unbounded. When decoding untrusted input, use
/// [`decompress_with_limit`] to cap cumulative output size, or drive
/// [`BzDecoder::read_block_with_limit`] directly for bounded block-at-a-time
/// consumption. [`BzDecoder::read_block`] remains available when no decoded
/// output limit is required.
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

/// Decompress BZip2 data with an output-size limit.
///
/// Behaves like [`decompress`] (including multi-stream support) but returns
/// [`OxiArcError::MemoryBudgetExceeded`] as soon as the accumulated output
/// would exceed `max_out` bytes — the decompression-bomb guard for
/// untrusted input.
///
/// # Memory characteristics
///
/// The remaining cumulative output budget is passed into each block decode
/// and enforced during RLE1 reconstruction, before the returned decoded block
/// grows beyond that remaining budget. Reaching exactly `max_out` bytes does
/// not skip end-of-stream, CRC, concatenated-stream, or trailing-data checks.
///
/// This is a decoded-output size limit, not a total process-memory or RSS
/// limit. Other decoder working structures, including BWT and Huffman state,
/// are governed separately by BZip2's format-level block-size bounds.
///
/// # Example
///
/// ```rust
/// use oxiarc_bzip2::{compress, decompress_with_limit, CompressionLevel};
///
/// let data = vec![0u8; 100_000];
/// let compressed = compress(&data, CompressionLevel::new(9)).expect("compress");
/// // Generous limit: succeeds.
/// let ok = decompress_with_limit(&compressed[..], 1 << 20).expect("decompress");
/// assert_eq!(ok, data);
/// // Tight limit: rejected before materialising a 100 kB decoded block.
/// assert!(decompress_with_limit(&compressed[..], 1024).is_err());
/// ```
pub fn decompress_with_limit<R: Read>(reader: R, max_out: usize) -> Result<Vec<u8>> {
    let mut decoder = BzDecoder::new(reader)?;
    let mut output = Vec::new();

    loop {
        let remaining = max_out
            .checked_sub(output.len())
            .ok_or_else(|| OxiArcError::memory_budget_exceeded(max_out, output.len()))?;

        let Some(block) = decoder.read_block_with_limit(remaining)? else {
            break;
        };

        let projected = output.len().saturating_add(block.len());
        if projected > max_out {
            return Err(OxiArcError::memory_budget_exceeded(max_out, projected));
        }

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
    fn decodes_randomised_blocks_roundtrip() {
        // The randomised test encoder applies libbz2's legacy XOR schedule
        // and sets the block's randomised bit; the decoder must undo it.
        use crate::CompressionLevel;
        let data = b"repetitive repetitive repetitive!\n".repeat(64);
        let stream = crate::encode::compress_randomised_for_tests(&data, CompressionLevel::new(1))
            .expect("build randomised stream");
        let decoded = decompress(&stream[..]).expect("decode randomised stream");
        assert_eq!(decoded, data);
    }

    #[test]
    fn randomised_fixture_matches_generator() {
        // Pins the committed fixture (verified byte-identical via
        // `bzip2 -d` at development time, re-checked by the bzip2-oracle
        // suite) to the in-tree generator so its provenance stays
        // auditable. If this drifts, re-verify against `bzip2 -d` before
        // re-blessing tests/data/randomised_rep_l1.bz2.
        use crate::CompressionLevel;
        let data = b"repetitive repetitive repetitive!\n".repeat(64);
        let stream = crate::encode::compress_randomised_for_tests(&data, CompressionLevel::new(1))
            .expect("build randomised stream");
        assert_eq!(
            stream,
            include_bytes!("../tests/data/randomised_rep_l1.bz2"),
            "randomised fixture drifted from its generator"
        );
    }

    #[test]
    fn decodes_concatenated_own_streams() {
        use crate::{CompressionLevel, compress};
        let first = b"stream one payload".repeat(20);
        let second = b"stream TWO payload, different level".repeat(15);
        let mut cat = compress(&first, CompressionLevel::new(9)).expect("compress first");
        cat.extend_from_slice(
            &compress(&second, CompressionLevel::new(1)).expect("compress second"),
        );

        let mut expected = first.clone();
        expected.extend_from_slice(&second);
        let decoded = decompress(&cat[..]).expect("decode concatenated streams");
        assert_eq!(decoded, expected);
    }

    #[test]
    fn trailing_garbage_after_stream_is_error() {
        use crate::{CompressionLevel, compress};
        let mut stream =
            compress(b"payload before garbage", CompressionLevel::new(1)).expect("compress");
        stream.extend_from_slice(b"NOT A BZIP2 STREAM");
        assert!(
            decompress(&stream[..]).is_err(),
            "trailing non-stream bytes must not be silently dropped"
        );
    }

    #[test]
    fn decompress_with_limit_enforces_cap() {
        use crate::{CompressionLevel, compress};
        let data = vec![0x5Au8; 50_000];
        let compressed = compress(&data, CompressionLevel::new(1)).expect("compress");

        let ok = decompress_with_limit(&compressed[..], data.len()).expect("within limit");
        assert_eq!(ok, data);

        let err = decompress_with_limit(&compressed[..], data.len() - 1);
        assert!(matches!(err, Err(OxiArcError::MemoryBudgetExceeded { .. })));
    }

    #[test]
    fn decompress_with_limit_is_cumulative_across_concatenated_streams() {
        use crate::{CompressionLevel, compress};

        let first = vec![b'A'; 700];
        let second = vec![b'B'; 700];

        let mut stream = compress(&first, CompressionLevel::new(1)).expect("compress first stream");
        stream.extend_from_slice(
            &compress(&second, CompressionLevel::new(1)).expect("compress second stream"),
        );

        let mut expected = first;
        expected.extend_from_slice(&second);

        let decoded =
            decompress_with_limit(&stream[..], expected.len()).expect("exact limit should succeed");
        assert_eq!(decoded, expected);

        let result = decompress_with_limit(&stream[..], expected.len() - 1);
        assert!(matches!(
            result,
            Err(OxiArcError::MemoryBudgetExceeded { .. })
        ));
    }

    #[test]
    fn decompress_with_limit_exact_limit_still_requires_complete_stream() {
        use crate::{CompressionLevel, compress};

        let data = vec![b'A'; 50_000];
        let mut stream = compress(&data, CompressionLevel::new(1)).expect("compress test payload");

        let decoded =
            decompress_with_limit(&stream[..], data.len()).expect("complete stream should succeed");
        assert_eq!(decoded, data);

        // Reaching the output limit must not bypass the trailing EOS and CRC checks.
        stream.pop().expect("compressed stream should not be empty");
        assert!(
            decompress_with_limit(&stream[..], data.len()).is_err(),
            "truncated framing must still be rejected at the exact output limit"
        );
    }

    #[test]
    fn decoder_output_limit_reaches_block_reconstruction() {
        use crate::{CompressionLevel, compress};

        // Repeated data exercises RLE1 expansion inside a normal, valid BZip2
        // block. The stream itself is intentionally valid; the rejection here
        // must come from the caller's resource policy rather than format errors.
        let original = vec![b'A'; 50_000];
        let compressed =
            compress(&original, CompressionLevel::new(1)).expect("compress test payload");

        // Give the streaming decoder a budget smaller than the decoded block.
        // The budget needs to travel through read_block_with_limit() into RLE1
        // reconstruction so expansion stops before the complete block is materialized.
        let mut decoder =
            BzDecoder::new(Cursor::new(&compressed)).expect("decoder should construct");

        let result = decoder.read_block_with_limit(1024);

        // Resource-policy rejection is distinct from malformed input. Preserve
        // that distinction so streaming callers can make an explicit policy
        // decision instead of treating a valid but oversized stream as corrupt.
        assert!(matches!(
            result,
            Err(OxiArcError::MemoryBudgetExceeded { .. })
        ));
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
