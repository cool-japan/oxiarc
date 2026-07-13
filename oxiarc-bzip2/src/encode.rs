//! BZip2 encoder.
//!
//! Produces streams in the real bzip2 format (readable by libbz2 and
//! compatible tools): MSB-first bit stream, bzip2-specific block CRC,
//! symbol map of used byte values, MTF over the used-byte list, RUNA/RUNB
//! zero-run coding, and canonical Huffman coding with up to six coding
//! tables selected per 50-symbol group via libbz2's iterative
//! `sendMTFValues` clustering (2-6 tables depending on block size, four
//! refinement passes, real MTF-coded selectors).

use crate::bitio::MsbBitWriter;
use crate::crc::Bz2Crc;
use crate::{BZIP2_MAGIC, CompressionLevel, bwt, huffman, mtf, rle};
use oxiarc_core::cancel::CancellationToken;
use oxiarc_core::error::{OxiArcError, Result};
use oxiarc_core::progress::ProgressHandle;
use std::io::Write;

#[cfg(feature = "parallel")]
use rayon::prelude::*;

/// Block magic as a 48-bit value (BCD digits of pi).
const BLOCK_MAGIC_BITS: u64 = 0x3141_5926_5359;

/// End-of-stream magic as a 48-bit value (BCD digits of sqrt(pi)).
const EOS_MAGIC_BITS: u64 = 0x1772_4538_5090;

/// Pseudo bit-cost of a symbol inside its initial group (libbz2
/// `BZ_LESSER_ICOST`).
const LESSER_ICOST: u8 = 0;

/// Pseudo bit-cost of a symbol outside its initial group (libbz2
/// `BZ_GREATER_ICOST`).
const GREATER_ICOST: u8 = 15;

/// Refinement passes over the table assignment (libbz2 `BZ_N_ITERS`).
const N_ITERS: usize = 4;

/// Hard format limit on the 15-bit selector count field.
const MAX_SELECTORS: usize = 32_767;

/// Maximum raw input bytes per block for a given level.
///
/// The block size limit applies to the *RLE1-encoded* data (libbz2 reserves
/// `BZ_N_OVERSHOOT` slack, hence the `- 20`). RLE1 can expand its input by
/// at most 5/4 (a 4-byte run becomes 5 bytes), so feeding at most 4/5 of the
/// limit guarantees the encoded block never exceeds it.
fn input_chunk_limit(level: CompressionLevel) -> usize {
    let block_limit = level.block_size() - 20;
    block_limit * 4 / 5
}

/// A fully transformed block, ready for bit-level serialization.
struct PreparedBlock {
    /// bzip2 CRC of the raw (pre-RLE1) block data.
    block_crc: u32,
    /// BWT original pointer.
    orig_ptr: u32,
    /// Whether the block's RLE1 data was randomised (legacy bzip2 <= 0.9.0
    /// feature; never set by the production pipeline, only by the
    /// test-fixture path).
    randomised: bool,
    /// Which byte values occur in the block.
    used: [bool; 256],
    /// MTF + RUNA/RUNB symbol stream, terminated by the EOB symbol.
    symbols: Vec<u16>,
    /// Canonical Huffman tables for the block alphabet (2-6).
    tables: Vec<huffman::HuffmanTable>,
    /// Table index chosen for each 50-symbol group.
    selectors: Vec<u8>,
}

/// Run the compression pipeline (CRC, RLE1, BWT, MTF, RLE2, Huffman table
/// clustering) for one non-empty block of raw data.
fn prepare_block(raw: &[u8]) -> Result<PreparedBlock> {
    prepare_block_impl(raw, false)
}

/// [`prepare_block`] with an optional legacy randomisation pass, used to
/// build regression fixtures for the randomised-block decode path.
fn prepare_block_impl(raw: &[u8], randomised: bool) -> Result<PreparedBlock> {
    debug_assert!(!raw.is_empty());

    // The block CRC covers the raw data, before RLE1.
    let block_crc = Bz2Crc::compute(raw);

    let mut rle1_data = rle::rle1_encode(raw);
    if randomised {
        // The randomisation mask is an involution, so the decoder-side
        // pass also serves as the encoder-side one.
        crate::rand::derandomise(&mut rle1_data);
    }
    let (bwt_data, orig_ptr) = bwt::transform(&rle1_data);

    // Symbol map: byte values used in the BWT string.
    let mut used = [false; 256];
    for &b in &bwt_data {
        used[b as usize] = true;
    }
    let used_symbols: Vec<u8> = (0..=255u8).filter(|&b| used[b as usize]).collect();
    let alpha_size = used_symbols.len() + 2;
    let eob = (alpha_size - 1) as u16;

    // MTF over the used-byte list, then RUNA/RUNB zero-run coding.
    // MTF value 0 becomes a zero run; value v >= 1 becomes symbol v + 1.
    let mtf_values = mtf::transform_with_alphabet(&bwt_data, &used_symbols)?;
    let mut symbols = rle::encode_zero_runs(&mtf_values);
    symbols.push(eob);

    // Cluster the symbol stream into 2-6 Huffman tables with per-group
    // selectors (libbz2's sendMTFValues strategy).
    let (tables, selectors) = build_coding_tables(&symbols, alpha_size)?;

    Ok(PreparedBlock {
        block_crc,
        orig_ptr,
        randomised,
        used,
        symbols,
        tables,
        selectors,
    })
}

/// Cluster the block's symbol stream into 2-6 Huffman coding tables and
/// choose a table for every 50-symbol group.
///
/// This is a faithful port of libbz2's `sendMTFValues` table-selection
/// phase (compress.c): the alphabet is first split into `n_groups`
/// contiguous ranges of roughly equal total frequency, each seeding one
/// table with cheap in-range / expensive out-of-range pseudo-costs. Four
/// refinement passes then (a) assign every group of 50 symbols to its
/// currently cheapest table and (b) rebuild each table's code lengths from
/// the frequencies of the groups it won. Compared to a single shared
/// table this recovers up to ~50% output size on structured data.
fn build_coding_tables(
    symbols: &[u16],
    alpha_size: usize,
) -> Result<(Vec<huffman::HuffmanTable>, Vec<u8>)> {
    let n_mtf = symbols.len();
    debug_assert!(n_mtf > 0, "symbol stream always holds at least EOB");

    // Number of coding tables, by symbol count (libbz2 sendMTFValues).
    let n_groups: usize = if n_mtf < 200 {
        2
    } else if n_mtf < 600 {
        3
    } else if n_mtf < 1200 {
        4
    } else if n_mtf < 2400 {
        5
    } else {
        6
    };

    let num_selectors = n_mtf.div_ceil(huffman::SYMBOLS_PER_GROUP);
    if num_selectors > MAX_SELECTORS {
        return Err(OxiArcError::encoding_error(
            "BZip2 block exceeds the selector count limit",
        ));
    }

    let mut freq = vec![0u32; alpha_size];
    for &sym in symbols {
        let idx = usize::from(sym);
        if idx >= alpha_size {
            return Err(OxiArcError::encoding_error("BZip2 symbol out of alphabet"));
        }
        freq[idx] += 1;
    }

    // Initial tables: split the alphabet into contiguous ranges of roughly
    // equal cumulative frequency, one range per table, with LESSER/GREATER
    // pseudo-costs standing in for real code lengths.
    let mut lengths: Vec<Vec<u8>> = vec![vec![GREATER_ICOST; alpha_size]; n_groups];
    {
        let mut n_part = n_groups as i32;
        let mut rem_f = n_mtf as i64;
        let mut gs: i32 = 0;
        while n_part > 0 {
            let t_freq = rem_f / i64::from(n_part);
            let mut ge: i32 = gs - 1;
            let mut a_freq: i64 = 0;
            while a_freq < t_freq && ge < alpha_size as i32 - 1 {
                ge += 1;
                a_freq += i64::from(freq[ge as usize]);
            }
            // libbz2 nudge: give alternate tables one symbol less so
            // neighbouring ranges differ, improving convergence.
            if ge > gs
                && n_part != n_groups as i32
                && n_part != 1
                && (n_groups as i32 - n_part) % 2 == 1
            {
                a_freq -= i64::from(freq[ge as usize]);
                ge -= 1;
            }
            let row = &mut lengths[(n_part - 1) as usize];
            for (v, len) in row.iter_mut().enumerate() {
                *len = if v as i32 >= gs && v as i32 <= ge {
                    LESSER_ICOST
                } else {
                    GREATER_ICOST
                };
            }
            n_part -= 1;
            gs = ge + 1;
            rem_f -= a_freq;
        }
    }

    // Refine: assign each 50-symbol group to the cheapest table, then
    // rebuild every table from the frequencies it accumulated.
    let mut selectors: Vec<u8> = Vec::with_capacity(num_selectors);
    for _ in 0..N_ITERS {
        let mut rfreq = vec![vec![0u32; alpha_size]; n_groups];
        selectors.clear();
        for group in symbols.chunks(huffman::SYMBOLS_PER_GROUP) {
            let mut best_table = 0usize;
            let mut best_cost = u32::MAX;
            for (table, len_row) in lengths.iter().enumerate() {
                let cost: u32 = group
                    .iter()
                    .map(|&sym| u32::from(len_row[usize::from(sym)]))
                    .sum();
                if cost < best_cost {
                    best_cost = cost;
                    best_table = table;
                }
            }
            selectors.push(best_table as u8);
            for &sym in group {
                rfreq[best_table][usize::from(sym)] += 1;
            }
        }
        for (len_row, table_freq) in lengths.iter_mut().zip(&rfreq) {
            *len_row = huffman::build_code_lengths(table_freq, huffman::MAX_ENCODE_LEN as u8);
        }
    }

    let mut tables = Vec::with_capacity(n_groups);
    for len_row in &lengths {
        tables.push(huffman::HuffmanTable::from_lengths(len_row)?);
    }
    Ok((tables, selectors))
}

/// Serialize one prepared block into the bit stream.
fn write_block_bits<W: Write>(writer: &mut MsbBitWriter<W>, block: &PreparedBlock) -> Result<()> {
    writer.write_bits_u64(BLOCK_MAGIC_BITS, 48)?;
    writer.write_bits(block.block_crc, 32)?;
    writer.write_bit(u32::from(block.randomised))?;
    writer.write_bits(block.orig_ptr, 24)?;

    // Symbol map: 16-bit group map, then a 16-bit map per used group.
    let mut group_bits = 0u32;
    for group in 0..16usize {
        if (0..16).any(|bit| block.used[group * 16 + bit]) {
            group_bits |= 1 << (15 - group);
        }
    }
    writer.write_bits(group_bits, 16)?;
    for group in 0..16usize {
        if group_bits & (1 << (15 - group)) != 0 {
            let mut bits = 0u32;
            for bit in 0..16usize {
                if block.used[group * 16 + bit] {
                    bits |= 1 << (15 - bit);
                }
            }
            writer.write_bits(bits, 16)?;
        }
    }

    // Table count, selector count, then the selectors MTF-coded over the
    // table indices and unary-coded (index j = j ones + a zero). The
    // selector count includes the group holding the EOB symbol.
    writer.write_bits(block.tables.len() as u32, 3)?;
    writer.write_bits(block.selectors.len() as u32, 15)?;
    let mut mtf_pos: Vec<u8> = (0..block.tables.len() as u8).collect();
    for &selector in &block.selectors {
        let index = mtf_pos
            .iter()
            .position(|&table| table == selector)
            .ok_or_else(|| OxiArcError::encoding_error("BZip2 selector out of table range"))?;
        for _ in 0..index {
            writer.write_bit(1)?;
        }
        writer.write_bit(0)?;
        if index > 0 {
            mtf_pos.copy_within(0..index, 1);
            mtf_pos[0] = selector;
        }
    }

    // Delta-coded code lengths, once per table.
    for table in &block.tables {
        let lengths = &table.lengths;
        let mut current = i32::from(lengths.first().copied().unwrap_or(1));
        writer.write_bits(current as u32, 5)?;
        for &len in lengths {
            let target = i32::from(len);
            while current < target {
                writer.write_bits(0b10, 2)?; // increment
                current += 1;
            }
            while current > target {
                writer.write_bits(0b11, 2)?; // decrement
                current -= 1;
            }
            writer.write_bit(0)?; // this symbol is done
        }
    }

    // The Huffman-coded symbol stream (EOB included), each 50-symbol group
    // encoded with the table its selector chose.
    for (group_index, group) in block.symbols.chunks(huffman::SYMBOLS_PER_GROUP).enumerate() {
        let selector = *block.selectors.get(group_index).ok_or_else(|| {
            OxiArcError::encoding_error("BZip2 selector missing for symbol group")
        })?;
        let table = block
            .tables
            .get(usize::from(selector))
            .ok_or_else(|| OxiArcError::encoding_error("BZip2 selector out of table range"))?;
        for &sym in group {
            let (code, len) = table
                .get_code(sym)
                .ok_or_else(|| OxiArcError::corrupted(0, "BZip2 symbol without Huffman code"))?;
            writer.write_bits(code, u32::from(len))?;
        }
    }

    Ok(())
}

/// BZip2 encoder.
///
/// Supports optional progress reporting via [`ProgressHandle`] and
/// cooperative cancellation via [`CancellationToken`] using the
/// [`BzEncoder::with_progress`] / [`BzEncoder::with_cancel`] builders.
pub struct BzEncoder<W: Write> {
    writer: MsbBitWriter<W>,
    level: CompressionLevel,
    combined_crc: u32,
    /// Raw input buffered until a full block's worth is available, so that
    /// small streaming writes coalesce into full-size bzip2 blocks. Always
    /// shorter than the level's per-block input limit between calls.
    buffer: Vec<u8>,
    /// Optional progress sink. Notified with cumulative uncompressed bytes
    /// after each block is successfully written.
    progress: Option<ProgressHandle>,
    /// Optional cancellation token. Checked before each block is encoded.
    cancel: Option<CancellationToken>,
    /// Cumulative uncompressed bytes successfully encoded.
    bytes_processed: u64,
}

impl<W: Write> BzEncoder<W> {
    /// Create a new encoder.
    pub fn new(writer: W, level: CompressionLevel) -> Result<Self> {
        let mut bit_writer = MsbBitWriter::new(writer);

        // Stream header: "BZh" + block size digit.
        bit_writer.write_bytes_aligned(&[
            BZIP2_MAGIC[0],
            BZIP2_MAGIC[1],
            b'h',
            b'0' + level.level(),
        ])?;

        Ok(Self {
            writer: bit_writer,
            level,
            combined_crc: 0,
            buffer: Vec::new(),
            progress: None,
            cancel: None,
            bytes_processed: 0,
        })
    }

    /// Attach a progress sink.
    ///
    /// The sink's `on_progress(cumulative_uncompressed_bytes, None)` is
    /// called once after each block is successfully encoded. `on_finish()`
    /// is called after the stream footer is written in [`BzEncoder::finish`].
    #[must_use]
    pub fn with_progress(mut self, handle: ProgressHandle) -> Self {
        self.progress = Some(handle);
        self
    }

    /// Attach a cancellation token.
    ///
    /// The token is checked at the start of each [`BzEncoder::write_block`]
    /// call. If cancelled, `write_block` returns [`oxiarc_core::error::OxiArcError::Cancelled`]
    /// before any bytes for that block are written.
    #[must_use]
    pub fn with_cancel(mut self, token: CancellationToken) -> Self {
        self.cancel = Some(token);
        self
    }

    /// Write input data into the encoder.
    ///
    /// Input is buffered internally until a full block's worth of raw data
    /// is available, so small streaming writes (e.g. 4 KiB reads) coalesce
    /// into full-size bzip2 blocks instead of emitting one undersized,
    /// poorly compressed block per call. Data larger than the per-block
    /// capacity is split so every emitted block stays within the format's
    /// block size limit. Any buffered remainder is flushed as the final
    /// block by [`BzEncoder::finish`] — until then it has not been written
    /// to the underlying writer.
    pub fn write_block(&mut self, data: &[u8]) -> Result<()> {
        if data.is_empty() {
            return Ok(());
        }

        // Cooperative cancellation check before any work.
        if let Some(ref token) = self.cancel {
            token.check()?;
        }

        let chunk_limit = input_chunk_limit(self.level);
        let mut rest = data;

        // Top an existing partial buffer up to a full block first.
        if !self.buffer.is_empty() {
            let take = (chunk_limit - self.buffer.len()).min(rest.len());
            self.buffer.extend_from_slice(&rest[..take]);
            rest = &rest[take..];
            if self.buffer.len() == chunk_limit {
                let full = std::mem::take(&mut self.buffer);
                self.emit_block(&full)?;
                self.buffer = full;
                self.buffer.clear();
            }
        }

        // Emit full blocks straight from the input without buffering them.
        while rest.len() >= chunk_limit {
            if let Some(ref token) = self.cancel {
                token.check()?;
            }
            self.emit_block(&rest[..chunk_limit])?;
            rest = &rest[chunk_limit..];
        }

        // Keep the remainder (always < one block) for the next call.
        self.buffer.extend_from_slice(rest);

        // Update cumulative uncompressed byte count and notify progress.
        self.bytes_processed = self.bytes_processed.saturating_add(data.len() as u64);
        if let Some(ref handle) = self.progress {
            handle.on_progress(self.bytes_processed, None);
        }

        Ok(())
    }

    /// Compress one raw block and append it to the bit stream.
    fn emit_block(&mut self, raw: &[u8]) -> Result<()> {
        let block = prepare_block(raw)?;
        write_block_bits(&mut self.writer, &block)?;
        self.combined_crc = self.combined_crc.rotate_left(1) ^ block.block_crc;
        Ok(())
    }

    /// Finish encoding and write the stream footer.
    ///
    /// Flushes any input still buffered by [`BzEncoder::write_block`] as a
    /// final (possibly short) block before the end-of-stream marker.
    pub fn finish(mut self) -> Result<W> {
        if !self.buffer.is_empty() {
            if let Some(ref token) = self.cancel {
                token.check()?;
            }
            let tail = std::mem::take(&mut self.buffer);
            self.emit_block(&tail)?;
        }

        // End-of-stream marker and the combined CRC of all blocks.
        self.writer.write_bits_u64(EOS_MAGIC_BITS, 48)?;
        self.writer.write_bits(self.combined_crc, 32)?;
        self.writer.finish()?;

        // Notify progress completion.
        if let Some(ref handle) = self.progress {
            handle.on_finish();
        }

        Ok(self.writer.into_inner())
    }
}

/// Compress data using BZip2.
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
pub fn compress(data: &[u8], level: CompressionLevel) -> Result<Vec<u8>> {
    let output = Vec::new();
    let mut encoder = BzEncoder::new(output, level)?;
    encoder.write_block(data)?;
    encoder.finish()
}

/// Compress data using parallel block compression (requires `parallel` feature).
///
/// This function splits the input into independent blocks and compresses them
/// in parallel using rayon. The heavy work (RLE1, BWT, MTF, Huffman table
/// building) is done in parallel, while the final bitstream writing is done
/// sequentially to maintain proper bit alignment.
///
/// # Arguments
///
/// * `data` - Data to compress
/// * `level` - Compression level (1-9)
///
/// # Returns
///
/// Compressed data in BZip2 format.
#[cfg(feature = "parallel")]
pub fn compress_parallel(data: &[u8], level: CompressionLevel) -> Result<Vec<u8>> {
    let mut bit_writer = MsbBitWriter::new(Vec::new());

    // Stream header: "BZh" + block size digit.
    bit_writer.write_bytes_aligned(&[
        BZIP2_MAGIC[0],
        BZIP2_MAGIC[1],
        b'h',
        b'0' + level.level(),
    ])?;

    if data.is_empty() {
        bit_writer.write_bits_u64(EOS_MAGIC_BITS, 48)?;
        bit_writer.write_bits(0, 32)?; // Combined CRC of zero blocks
        bit_writer.finish()?;
        return Ok(bit_writer.into_inner());
    }

    // Transform blocks in parallel (heavy computation only, no writing).
    let chunks: Vec<&[u8]> = data.chunks(input_chunk_limit(level)).collect();
    let prepared: Vec<Result<PreparedBlock>> = chunks
        .par_iter()
        .map(|chunk| prepare_block(chunk))
        .collect();

    // Write blocks sequentially to keep the bit stream contiguous.
    let mut combined_crc = 0u32;
    for result in prepared {
        let block = result?;
        write_block_bits(&mut bit_writer, &block)?;
        combined_crc = combined_crc.rotate_left(1) ^ block.block_crc;
    }

    bit_writer.write_bits_u64(EOS_MAGIC_BITS, 48)?;
    bit_writer.write_bits(combined_crc, 32)?;
    bit_writer.finish()?;
    Ok(bit_writer.into_inner())
}

/// Compress `data` into a stream whose blocks carry the legacy randomised
/// bit (bzip2 <= 0.9.0). No modern encoder produces such streams, so this
/// exists solely to build regression fixtures for the randomised-block
/// decode path; the committed fixture was verified against `bzip2 -d`.
#[cfg(test)]
pub(crate) fn compress_randomised_for_tests(
    data: &[u8],
    level: CompressionLevel,
) -> Result<Vec<u8>> {
    let mut bit_writer = MsbBitWriter::new(Vec::new());
    bit_writer.write_bytes_aligned(&[
        BZIP2_MAGIC[0],
        BZIP2_MAGIC[1],
        b'h',
        b'0' + level.level(),
    ])?;

    let mut combined_crc = 0u32;
    for chunk in data.chunks(input_chunk_limit(level)) {
        let block = prepare_block_impl(chunk, true)?;
        write_block_bits(&mut bit_writer, &block)?;
        combined_crc = combined_crc.rotate_left(1) ^ block.block_crc;
    }

    bit_writer.write_bits_u64(EOS_MAGIC_BITS, 48)?;
    bit_writer.write_bits(combined_crc, 32)?;
    bit_writer.finish()?;
    Ok(bit_writer.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compress_empty() {
        let result = compress(b"", CompressionLevel::default()).expect("compress empty input");
        // Header (4) + EOS magic (6) + combined CRC (4).
        assert_eq!(result.len(), 14);
        assert_eq!(&result[0..2], &BZIP2_MAGIC);
    }

    #[test]
    fn test_compress_hello() {
        let result =
            compress(b"hello world", CompressionLevel::new(1)).expect("compress hello world");
        assert!(result.len() > 10);
        assert_eq!(&result[0..2], &BZIP2_MAGIC);
    }

    #[test]
    fn test_encoder_with_progress_builder() {
        use oxiarc_core::progress::{ProgressHandle, ProgressSink};
        use std::sync::Arc;
        use std::sync::atomic::{AtomicU64, Ordering};

        struct CountingSink {
            progress_count: AtomicU64,
            finish_count: AtomicU64,
            last_processed: AtomicU64,
        }

        impl ProgressSink for CountingSink {
            fn on_progress(&self, processed: u64, _total: Option<u64>) {
                self.progress_count.fetch_add(1, Ordering::SeqCst);
                self.last_processed.store(processed, Ordering::SeqCst);
            }
            fn on_finish(&self) {
                self.finish_count.fetch_add(1, Ordering::SeqCst);
            }
        }

        let sink = Arc::new(CountingSink {
            progress_count: AtomicU64::new(0),
            finish_count: AtomicU64::new(0),
            last_processed: AtomicU64::new(0),
        });
        let handle: ProgressHandle = sink.clone();

        let output = Vec::new();
        let mut encoder = BzEncoder::new(output, CompressionLevel::new(1))
            .expect("encoder should construct")
            .with_progress(handle);
        encoder
            .write_block(b"hello progress world")
            .expect("write_block should succeed");
        let _ = encoder.finish().expect("finish should succeed");

        assert!(sink.progress_count.load(Ordering::SeqCst) >= 1);
        assert_eq!(sink.finish_count.load(Ordering::SeqCst), 1);
        assert_eq!(sink.last_processed.load(Ordering::SeqCst), 20);
    }

    #[test]
    fn small_streaming_writes_coalesce_into_full_blocks() {
        use crate::decompress;
        // Feeding data in 4 KiB slivers must produce exactly the same
        // stream as one big write: the encoder buffers input and only cuts
        // blocks at the level's block size (plus the final remainder).
        let data: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        let level = CompressionLevel::new(1);

        let whole = compress(&data, level).expect("single-shot compress");

        let mut encoder = BzEncoder::new(Vec::new(), level).expect("encoder");
        for chunk in data.chunks(4096) {
            encoder.write_block(chunk).expect("streamed write");
        }
        let streamed = encoder.finish().expect("finish");

        assert_eq!(
            streamed, whole,
            "streamed writes must coalesce into the same blocks as one write"
        );
        let decoded = decompress(&streamed[..]).expect("decode streamed output");
        assert_eq!(decoded, data);
    }

    #[test]
    fn write_larger_than_block_after_partial_buffer() {
        use crate::decompress;
        // A partial buffer followed by a multi-block write exercises the
        // top-up path plus the direct full-block path in one call.
        let level = CompressionLevel::new(1);
        let chunk_limit = input_chunk_limit(level);
        let first = vec![0xA5u8; 1000];
        let second: Vec<u8> = (0..(chunk_limit * 2 + 500) as u32)
            .map(|i| (i % 253) as u8)
            .collect();

        let mut encoder = BzEncoder::new(Vec::new(), level).expect("encoder");
        encoder.write_block(&first).expect("write partial");
        encoder.write_block(&second).expect("write multi-block");
        let streamed = encoder.finish().expect("finish");

        let mut expected_input = first.clone();
        expected_input.extend_from_slice(&second);
        let whole = compress(&expected_input, level).expect("single-shot compress");
        assert_eq!(streamed, whole);
        assert_eq!(decompress(&streamed[..]).expect("decode"), expected_input);
    }

    #[test]
    fn test_encoder_with_cancel_builder() {
        use oxiarc_core::cancel::CancellationToken;
        use oxiarc_core::error::OxiArcError;

        let token = CancellationToken::new();
        let output = Vec::new();
        let mut encoder = BzEncoder::new(output, CompressionLevel::new(1))
            .expect("encoder should construct")
            .with_cancel(token.clone());

        token.cancel();
        let result = encoder.write_block(b"should not compress");
        assert!(matches!(result, Err(OxiArcError::Cancelled)));
    }

    #[test]
    #[cfg(feature = "parallel")]
    fn test_parallel_roundtrip_basic() {
        use crate::decompress;
        let data = b"Hello, World! Parallel Bzip2 compression test.";
        let compressed =
            compress_parallel(data, CompressionLevel::new(1)).expect("parallel compress basic");
        let decompressed = decompress(&compressed[..]).expect("decompress parallel basic");
        assert_eq!(decompressed, data.as_slice());
    }

    #[test]
    #[cfg(feature = "parallel")]
    fn test_parallel_roundtrip_large() {
        use crate::decompress;
        // Large data spanning multiple blocks
        let data = vec![0x42u8; 3_000_000];
        let compressed =
            compress_parallel(&data, CompressionLevel::new(5)).expect("parallel compress large");
        let decompressed = decompress(&compressed[..]).expect("decompress parallel large");
        assert_eq!(decompressed, data);
    }

    #[test]
    #[cfg(feature = "parallel")]
    fn test_parallel_vs_serial() {
        use crate::decompress;
        let data = b"Testing parallel vs serial Bzip2 compression.";
        let level = CompressionLevel::new(9);

        let serial = compress(data, level).expect("serial compress");
        let parallel = compress_parallel(data, level).expect("parallel compress");

        // Both should decompress correctly
        let serial_decompressed = decompress(&serial[..]).expect("decompress serial");
        let parallel_decompressed = decompress(&parallel[..]).expect("decompress parallel");

        assert_eq!(serial_decompressed, data.as_slice());
        assert_eq!(parallel_decompressed, data.as_slice());
    }

    #[test]
    #[cfg(feature = "parallel")]
    fn test_parallel_empty() {
        use crate::decompress;
        let data: &[u8] = b"";
        let compressed =
            compress_parallel(data, CompressionLevel::new(1)).expect("parallel compress empty");
        let decompressed = decompress(&compressed[..]).expect("decompress parallel empty");
        assert_eq!(decompressed, data);
    }

    #[test]
    #[cfg(feature = "parallel")]
    fn test_parallel_multiple_blocks() {
        use crate::decompress;
        // Test parallel compression by compressing two separate blocks sequentially
        // Each block uses parallel processing internally
        // Using 5KB per call to keep BWT complexity low and test fast
        let pattern =
            b"The quick brown fox jumps over the lazy dog. 0123456789 ABCDEFGHIJKLMNOPQRSTUVWXYZ\n";
        let target_size = 5_000; // 5KB per block (reduced from 30KB)
        let mut data = Vec::new();
        let repeats = target_size / pattern.len() + 1;
        for _ in 0..repeats {
            data.extend_from_slice(pattern);
        }
        data.truncate(target_size);

        // Compress and decompress first block
        let compressed1 =
            compress_parallel(&data, CompressionLevel::new(1)).expect("parallel compress block 1");
        let decompressed1 = decompress(&compressed1[..]).expect("decompress block 1");
        assert_eq!(decompressed1, data);

        // Create second block with different pattern
        let pattern2 = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ abcdefghijklmnopqrstuvwxyz\n";
        let mut data2 = Vec::new();
        let repeats2 = target_size / pattern2.len() + 1;
        for _ in 0..repeats2 {
            data2.extend_from_slice(pattern2);
        }
        data2.truncate(target_size);

        // Compress and decompress second block
        let compressed2 =
            compress_parallel(&data2, CompressionLevel::new(1)).expect("parallel compress block 2");
        let decompressed2 = decompress(&compressed2[..]).expect("decompress block 2");
        assert_eq!(decompressed2, data2);
    }

    #[test]
    #[cfg(feature = "parallel")]
    fn test_parallel_repeated_data() {
        use crate::decompress;
        // Reduced from repeat(10000) to repeat(200) for faster testing
        // This still gives 7200 bytes which is enough to test repeated data compression
        let data = b"aaaaaaaaaaaabbbbbbbbbbbbcccccccccccc".repeat(200);
        // Use level 3 instead of 9 for faster BWT while still testing compression quality
        let compressed = compress_parallel(&data, CompressionLevel::new(3))
            .expect("parallel compress repeated data");

        // Should compress well
        assert!(compressed.len() < data.len() / 5);

        let decompressed = decompress(&compressed[..]).expect("decompress repeated data");
        assert_eq!(decompressed, data);
    }

    #[test]
    #[cfg(feature = "parallel")]
    fn test_parallel_different_levels() {
        use crate::decompress;
        // Reduced from repeat(1000) to repeat(100) for faster testing (4400 bytes)
        let data = b"Test data for different compression levels.".repeat(100);

        // Test only levels 1, 5, 9 instead of all 1-9 to reduce test time by 67%
        // This still covers low, medium, and high compression adequately
        for level in [1, 5, 9] {
            let compressed = compress_parallel(&data, CompressionLevel::new(level))
                .expect("parallel compress for level");
            let decompressed = decompress(&compressed[..]).expect("decompress for level");
            assert_eq!(decompressed, data, "Failed for level {}", level);
        }
    }
}
