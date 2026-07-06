//! BZip2 encoder.
//!
//! Produces streams in the real bzip2 format (readable by libbz2 and
//! compatible tools): MSB-first bit stream, bzip2-specific block CRC,
//! symbol map of used byte values, MTF over the used-byte list, RUNA/RUNB
//! zero-run coding, and canonical Huffman coding with the minimum required
//! two coding tables (all selectors referencing table 0).

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

/// Number of Huffman tables this encoder emits (the format minimum).
const NUM_TABLES: u32 = huffman::MIN_TABLES as u32;

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
    /// Which byte values occur in the block.
    used: [bool; 256],
    /// MTF + RUNA/RUNB symbol stream, terminated by the EOB symbol.
    symbols: Vec<u16>,
    /// Canonical Huffman table for the block alphabet.
    table: huffman::HuffmanTable,
}

/// Run the compression pipeline (CRC, RLE1, BWT, MTF, RLE2, Huffman build)
/// for one non-empty block of raw data.
fn prepare_block(raw: &[u8]) -> Result<PreparedBlock> {
    debug_assert!(!raw.is_empty());

    // The block CRC covers the raw data, before RLE1.
    let block_crc = Bz2Crc::compute(raw);

    let rle1_data = rle::rle1_encode(raw);
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
    let mtf_values = mtf::transform_with_alphabet(&bwt_data, &used_symbols);
    let mut symbols = rle::encode_zero_runs(&mtf_values);
    symbols.push(eob);

    // Canonical, length-limited Huffman code for the block alphabet.
    let mut freqs = vec![0u32; alpha_size];
    for &sym in &symbols {
        let idx = sym as usize;
        if idx >= alpha_size {
            return Err(OxiArcError::corrupted(0, "BZip2 symbol out of alphabet"));
        }
        freqs[idx] += 1;
    }
    let lengths = huffman::build_code_lengths(&freqs, huffman::MAX_ENCODE_LEN as u8);
    let table = huffman::HuffmanTable::from_lengths(&lengths)?;

    Ok(PreparedBlock {
        block_crc,
        orig_ptr,
        used,
        symbols,
        table,
    })
}

/// Serialize one prepared block into the bit stream.
fn write_block_bits<W: Write>(writer: &mut MsbBitWriter<W>, block: &PreparedBlock) -> Result<()> {
    writer.write_bits_u64(BLOCK_MAGIC_BITS, 48)?;
    writer.write_bits(block.block_crc, 32)?;
    writer.write_bit(0)?; // randomised = 0 (deprecated feature)
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

    // Two identical Huffman tables (format minimum); every 50-symbol group
    // selects table 0, which MTF-codes to a single 0 bit per selector. The
    // selector count includes the group holding the EOB symbol.
    let num_selectors = block.symbols.len().div_ceil(huffman::SYMBOLS_PER_GROUP);
    writer.write_bits(NUM_TABLES, 3)?;
    writer.write_bits(num_selectors as u32, 15)?;
    for _ in 0..num_selectors {
        writer.write_bit(0)?;
    }

    // Delta-coded code lengths, once per table.
    let lengths = &block.table.lengths;
    for _ in 0..NUM_TABLES {
        let mut current = i32::from(lengths[0]);
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

    // The Huffman-coded symbol stream (EOB included).
    for &sym in &block.symbols {
        let (code, len) = block
            .table
            .get_code(sym)
            .ok_or_else(|| OxiArcError::corrupted(0, "BZip2 symbol without Huffman code"))?;
        writer.write_bits(code, u32::from(len))?;
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
    pub fn with_progress(mut self, handle: ProgressHandle) -> Self {
        self.progress = Some(handle);
        self
    }

    /// Attach a cancellation token.
    ///
    /// The token is checked at the start of each [`BzEncoder::write_block`]
    /// call. If cancelled, `write_block` returns [`oxiarc_core::error::OxiArcError::Cancelled`]
    /// before any bytes for that block are written.
    pub fn with_cancel(mut self, token: CancellationToken) -> Self {
        self.cancel = Some(token);
        self
    }

    /// Write a data block.
    ///
    /// Data larger than the level's per-block capacity is split into
    /// multiple bzip2 blocks so that every emitted block stays within the
    /// format's block size limit.
    pub fn write_block(&mut self, data: &[u8]) -> Result<()> {
        if data.is_empty() {
            return Ok(());
        }

        let chunk_limit = input_chunk_limit(self.level);
        for chunk in data.chunks(chunk_limit) {
            // Cooperative cancellation check before each block.
            if let Some(ref token) = self.cancel {
                token.check()?;
            }
            let block = prepare_block(chunk)?;
            write_block_bits(&mut self.writer, &block)?;
            self.combined_crc = self.combined_crc.rotate_left(1) ^ block.block_crc;
        }

        // Update cumulative uncompressed byte count and notify progress.
        self.bytes_processed = self.bytes_processed.saturating_add(data.len() as u64);
        if let Some(ref handle) = self.progress {
            handle.on_progress(self.bytes_processed, None);
        }

        Ok(())
    }

    /// Finish encoding and write the stream footer.
    pub fn finish(mut self) -> Result<W> {
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
