//! `.xz` stream writer.
//!
//! Split out of `header.rs` (which owns the reader and the shared header
//! types) purely for file size; the writer needs nothing from the reader
//! but the header constants and `StreamFlags`/`CheckType`, which it
//! imports below.
//!
//! ## Progress / cancellation
//!
//! [`XzWriter`] exposes `.with_progress()` / `.with_cancel()` builders. The
//! hooks are emitted by this wrapper itself — the underlying `oxiarc-lzma`
//! LZMA2 encoder does not expose per-chunk builders — so granularity is one
//! block.

use super::header::{CheckType, FILTER_LZMA2, StreamFlags, XZ_FOOTER_MAGIC, XZ_MAGIC};
use crate::{Lzma2Encoder, LzmaLevel, props_from_dict_size};
use oxiarc_core::cancel::CancellationToken;
use oxiarc_core::crc::{Crc32, Crc64};
use oxiarc_core::error::{OxiArcError, Result};
use oxiarc_core::progress::ProgressHandle;
use std::io::Write;

/// Largest uncompressed payload [`XzWriter`] puts into a single block.
///
/// A `.xz` stream may carry any number of blocks, and the reader above
/// refuses one whose compressed size exceeds [`MAX_BLOCK_COMPRESSED_SIZE`].
/// A writer that emitted one block per call regardless of size could
/// therefore produce a file this very crate cannot read back — which is
/// what happened before 0.4.2. Splitting at 64 MiB of *input* keeps every
/// block comfortably under that cap: LZMA2's worst case is stored chunks
/// with a 3-byte header per 64 KiB of data, i.e. about 0.005 % expansion,
/// so 64 MiB in can never approach 100 MiB out.
///
/// Payloads below this size still produce exactly one block, so the bytes
/// written for ordinary inputs are unchanged.
const DEFAULT_BLOCK_SIZE: u64 = 64 * 1024 * 1024;

/// XZ writer for creating XZ compressed files.
pub struct XzWriter {
    level: LzmaLevel,
    check_type: CheckType,
    /// Largest uncompressed payload placed in one block.
    block_size: u64,
    /// Optional progress sink (wrapper-emitted, one-shot).
    progress: Option<ProgressHandle>,
    /// Optional cancellation token checked before compression.
    cancel: Option<CancellationToken>,
}

impl XzWriter {
    /// Create a new XZ writer.
    pub fn new(level: LzmaLevel) -> Self {
        Self {
            level,
            check_type: CheckType::Crc32,
            block_size: DEFAULT_BLOCK_SIZE,
            progress: None,
            cancel: None,
        }
    }

    /// Set the largest uncompressed payload placed in a single block.
    ///
    /// Input longer than this is split across several blocks, each with its
    /// own index record — a plain `.xz` stream that any decoder reads. The
    /// default (`DEFAULT_BLOCK_SIZE`, 64 MiB) is chosen so that a block
    /// can never exceed the compressed-size limit the reader enforces; the
    /// setter exists mainly so the multi-block path is testable with small
    /// payloads, but it is also the knob to reach for if a consumer wants
    /// finer-grained blocks (for example to decode them in parallel).
    ///
    /// A value of zero is treated as one byte per block.
    #[must_use]
    pub fn with_block_size(mut self, uncompressed_bytes: u64) -> Self {
        self.block_size = uncompressed_bytes;
        self
    }

    /// Set the check type.
    #[must_use]
    pub fn with_check_type(mut self, check_type: CheckType) -> Self {
        self.check_type = check_type;
        self
    }

    /// Attach a progress sink. Notified once after compression completes with
    /// the uncompressed byte count, followed by `on_finish()`.
    #[must_use]
    pub fn with_progress(mut self, handle: ProgressHandle) -> Self {
        self.progress = Some(handle);
        self
    }

    /// Attach a cancellation token. Checked before compression begins.
    #[must_use]
    pub fn with_cancel(mut self, token: CancellationToken) -> Self {
        self.cancel = Some(token);
        self
    }

    /// Compress data to XZ format.
    pub fn compress(&self, data: &[u8]) -> Result<Vec<u8>> {
        if let Some(ref token) = self.cancel {
            token.check()?;
        }

        let mut output = Vec::new();

        // Write stream header
        let stream_flags = StreamFlags::new(self.check_type);
        self.write_stream_header(&mut output, stream_flags)?;

        // Write the blocks, keeping each Unpadded Size (header + compressed
        // data + check, excluding block padding) for the index record.
        //
        // Empty input still produces one empty block, which is what this
        // writer has always emitted; `data.chunks()` would yield nothing.
        let block_size = usize::try_from(self.block_size)
            .unwrap_or(usize::MAX)
            .max(1);
        let mut records: Vec<(usize, usize)> = Vec::new();
        let mut processed = 0u64;
        let total = data.len() as u64;
        if data.is_empty() {
            records.push((self.write_block(&mut output, data)?, 0));
        } else {
            for chunk in data.chunks(block_size) {
                if let Some(ref token) = self.cancel {
                    token.check()?;
                }
                records.push((self.write_block(&mut output, chunk)?, chunk.len()));
                processed += chunk.len() as u64;
                if let Some(ref handle) = self.progress {
                    if processed < total {
                        handle.on_progress(processed, Some(total));
                    }
                }
            }
        }

        // Write index
        let index_start = output.len();
        self.write_index(&mut output, &records)?;
        let index_end = output.len();

        // Write stream footer
        self.write_stream_footer(&mut output, stream_flags, index_end - index_start)?;

        if let Some(ref handle) = self.progress {
            handle.on_progress(total, Some(total));
            handle.on_finish();
        }

        Ok(output)
    }

    /// Write stream header.
    fn write_stream_header<W: Write>(&self, writer: &mut W, flags: StreamFlags) -> Result<()> {
        // Magic
        writer.write_all(&XZ_MAGIC)?;

        // Stream flags
        let flags_bytes = flags.encode();
        writer.write_all(&flags_bytes)?;

        // CRC32 of stream flags
        let crc = Crc32::compute(&flags_bytes);
        writer.write_all(&crc.to_le_bytes())?;

        Ok(())
    }

    /// Write a compressed block.
    ///
    /// Returns the block's Unpadded Size (block header + compressed data +
    /// check, excluding block padding) as required by the index record.
    ///
    /// The block header declares both optional size fields (flags `0xC0`),
    /// exactly as the `xz` CLI does. **Uncompressed Size is the size of the
    /// block's *original* data — before any filter chain, not after it** —
    /// and Compressed Size is the exact length of the Compressed Data
    /// field. The reader enforces both (`decompress_stream` /
    /// `decompress_block_with_size`), so if this writer ever grows a
    /// non-last filter (Delta, BCJ), it must keep declaring the original
    /// size here or produce files it cannot read back.
    fn write_block<W: Write>(&self, writer: &mut W, data: &[u8]) -> Result<usize> {
        // Compress data with LZMA2
        let encoder = Lzma2Encoder::new(self.level);
        let compressed = encoder.encode(data)?;

        // Calculate dictionary size props
        let dict_size = self.level.dict_size();
        let dict_props = props_from_dict_size(dict_size);

        // Build compressed size as multibyte int
        let mut compressed_size_bytes = Vec::new();
        Self::write_multibyte_int_static(&mut compressed_size_bytes, compressed.len() as u64);

        // Build uncompressed size as multibyte int
        let mut uncompressed_size_bytes = Vec::new();
        Self::write_multibyte_int_static(&mut uncompressed_size_bytes, data.len() as u64);

        // Build block header content (not including size byte or CRC)
        let mut block_header = Vec::new();

        // Flags: 1 filter, has compressed size, has uncompressed size
        block_header.push(0xC0); // 1 filter, has compressed size (0x40), has uncompressed size (0x80)

        // Compressed size
        block_header.extend_from_slice(&compressed_size_bytes);

        // Uncompressed size
        block_header.extend_from_slice(&uncompressed_size_bytes);

        // Filter: LZMA2
        block_header.push(FILTER_LZMA2 as u8); // Filter ID (single byte for LZMA2)
        block_header.push(0x01); // Properties size = 1
        block_header.push(dict_props); // Dictionary size properties

        // Calculate header size byte first
        // Total header size = 1 (size byte) + content + padding + 4 (CRC)
        // Must be multiple of 4, so: (size_byte + 1) * 4 = 1 + content + padding + 4
        // padding = ((size_byte + 1) * 4) - 1 - content - 4 = (size_byte + 1) * 4 - 5 - content
        // We need the smallest size_byte such that (size_byte + 1) * 4 >= 1 + content + 4
        // (size_byte + 1) * 4 >= content + 5
        // size_byte >= (content + 5) / 4 - 1
        // size_byte = ceil((content + 5) / 4) - 1 = (content + 5 + 3) / 4 - 1 = (content + 4) / 4
        let header_size_byte = ((block_header.len() + 4) / 4) as u8;
        let total_header_size = (header_size_byte as usize + 1) * 4;
        let padding = total_header_size - 1 - block_header.len() - 4;

        // Add padding
        block_header.resize(block_header.len() + padding, 0x00);

        // CRC32 of block header (size byte + padded content, per the xz
        // format spec section 3.1: everything except the CRC32 field itself)
        let mut header_crc_input = Vec::with_capacity(1 + block_header.len());
        header_crc_input.push(header_size_byte);
        header_crc_input.extend_from_slice(&block_header);
        let header_crc = Crc32::compute(&header_crc_input);

        // Write size byte
        writer.write_all(&[header_size_byte])?;

        // Write block header content
        writer.write_all(&block_header)?;

        // Write block header CRC
        writer.write_all(&header_crc.to_le_bytes())?;

        // Write compressed data
        writer.write_all(&compressed)?;

        // Pad compressed data to a 4-byte boundary (block padding is NOT
        // part of the Unpadded Size recorded in the index)
        let padding = (4 - (compressed.len() % 4)) % 4;
        for _ in 0..padding {
            writer.write_all(&[0x00])?;
        }

        // Write check
        match self.check_type {
            CheckType::None => {}
            CheckType::Crc32 => {
                let crc = Crc32::compute(data);
                writer.write_all(&crc.to_le_bytes())?;
            }
            CheckType::Crc64 => {
                let crc = Crc64::compute(data);
                writer.write_all(&crc.to_le_bytes())?;
            }
            CheckType::Sha256 => {
                let digest = oxiarc_core::sha256::Sha256::compute(data);
                writer.write_all(&digest)?;
            }
        }

        // Unpadded Size = block header + compressed data + check
        Ok(total_header_size + compressed.len() + self.check_type.size())
    }

    /// Write a multibyte integer (static version).
    fn write_multibyte_int_static(output: &mut Vec<u8>, mut value: u64) {
        loop {
            let byte = (value & 0x7F) as u8;
            value >>= 7;
            if value == 0 {
                output.push(byte);
                break;
            } else {
                output.push(byte | 0x80);
            }
        }
    }

    /// Write index.
    ///
    /// One record per block, in the order the blocks were written. Each
    /// record's first field is the Unpadded Size — the block WITHOUT its
    /// trailing padding (header + compressed data + check), per the xz
    /// format spec — and the second is that block's uncompressed size.
    fn write_index<W: Write>(&self, writer: &mut W, records: &[(usize, usize)]) -> Result<()> {
        let mut index = Vec::new();

        // Index indicator
        index.push(0x00);

        // Number of records (a multibyte integer: a stream may hold more
        // than 127 blocks).
        self.write_multibyte_int(&mut index, records.len() as u64);

        // Records: unpadded size, uncompressed size
        for &(unpadded_size, uncompressed_size) in records {
            self.write_multibyte_int(&mut index, unpadded_size as u64);
            self.write_multibyte_int(&mut index, uncompressed_size as u64);
        }

        // Pad to 4 bytes
        while (index.len() + 4) % 4 != 0 {
            index.push(0x00);
        }

        // CRC32
        let crc = Crc32::compute(&index);
        index.extend_from_slice(&crc.to_le_bytes());

        writer.write_all(&index)?;

        Ok(())
    }

    /// Write a multibyte integer.
    fn write_multibyte_int(&self, output: &mut Vec<u8>, mut value: u64) {
        loop {
            let byte = (value & 0x7F) as u8;
            value >>= 7;
            if value == 0 {
                output.push(byte);
                break;
            } else {
                output.push(byte | 0x80);
            }
        }
    }

    /// Write stream footer.
    fn write_stream_footer<W: Write>(
        &self,
        writer: &mut W,
        flags: StreamFlags,
        index_size: usize,
    ) -> Result<()> {
        // Backward size (index size / 4 - 1). `index_size` is a `usize` we
        // computed ourselves while writing the Index field, but a plain
        // `as u32` would still silently wrap once it exceeds `u32::MAX`
        // (an index that large implies an implausibly large archive, but
        // "implausible" is not "impossible" on a 64-bit target) and emit a
        // footer whose Backward Size does not describe the Index we just
        // wrote -- a self-corrupting archive our own reader would then
        // reject. Mirrors the `try_from` guard `read_footer` applies to the
        // same field on the decode side.
        let backward_size = u32::try_from((index_size / 4).saturating_sub(1)).map_err(|_| {
            OxiArcError::encoding_error(format!(
                "XZ index size {index_size} does not fit the 32-bit Backward Size field"
            ))
        })?;

        // CRC32 of backward size and stream flags
        let mut footer_data = Vec::new();
        footer_data.extend_from_slice(&backward_size.to_le_bytes());
        footer_data.extend_from_slice(&flags.encode());
        let crc = Crc32::compute(&footer_data);

        // Write footer
        writer.write_all(&crc.to_le_bytes())?;
        writer.write_all(&backward_size.to_le_bytes())?;
        writer.write_all(&flags.encode())?;
        writer.write_all(&XZ_FOOTER_MAGIC)?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::header::{decompress_slice, xorshift_bytes};
    use super::*;

    #[test]
    fn test_xz_roundtrip_large_multi_block() {
        // Hand-assemble a two-block XZ stream to exercise the reader's
        // multi-block loop together with the index CRC-32 and footer
        // Backward Size validation against a genuine, format-compliant
        // multi-record index, independently of how `XzWriter::compress`
        // happens to split its input (see
        // `xz_writer_splits_large_input_into_blocks`).
        let writer = XzWriter::new(LzmaLevel::new(6));
        let stream_flags = StreamFlags::new(writer.check_type);

        let block_a = xorshift_bytes(0x1234_5678_9ABC_DEF0, 48 * 1024);
        let block_b: Vec<u8> = (0..96 * 1024).map(|i| (i % 251) as u8).collect();

        let mut output = Vec::new();
        writer
            .write_stream_header(&mut output, stream_flags)
            .expect("write stream header");
        let unpadded_a = writer
            .write_block(&mut output, &block_a)
            .expect("write block a");
        let unpadded_b = writer
            .write_block(&mut output, &block_b)
            .expect("write block b");

        // Build a genuine 2-record index (Index Indicator + Number of
        // Records + records + padding + CRC32), matching the on-disk layout
        // `write_index` produces for a single record.
        let mut index = vec![0x00u8];
        index.push(0x02); // number of records
        writer.write_multibyte_int(&mut index, unpadded_a as u64);
        writer.write_multibyte_int(&mut index, block_a.len() as u64);
        writer.write_multibyte_int(&mut index, unpadded_b as u64);
        writer.write_multibyte_int(&mut index, block_b.len() as u64);
        while (index.len() + 4) % 4 != 0 {
            index.push(0x00);
        }
        let index_crc = Crc32::compute(&index);
        index.extend_from_slice(&index_crc.to_le_bytes());
        output.extend_from_slice(&index);

        writer
            .write_stream_footer(&mut output, stream_flags, index.len())
            .expect("write stream footer");

        let mut expected = block_a.clone();
        expected.extend_from_slice(&block_b);

        let decompressed =
            decompress_slice(&output).expect("decompress hand-assembled multi-block stream");
        assert_eq!(decompressed, expected);
    }
    #[test]
    fn test_xz_progress_forwarding() {
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
            fn on_entry(&self, _name: &str, _index: u64) {}
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

        // Use a small repeating payload so the underlying LZMA encoder
        // handles it correctly (see module notes on complex data patterns).
        let data: Vec<u8> = (0..1_000).map(|_| b'A').collect();
        let writer = XzWriter::new(LzmaLevel::new(6)).with_progress(handle);
        let _compressed = writer
            .compress(&data)
            .expect("xz compression with progress should succeed");

        assert!(sink.progress_count.load(Ordering::SeqCst) >= 1);
        assert_eq!(sink.finish_count.load(Ordering::SeqCst), 1);
        assert_eq!(
            sink.last_processed.load(Ordering::SeqCst),
            data.len() as u64
        );
    }
    #[test]
    fn test_xz_cancel_forwarding() {
        use oxiarc_core::cancel::CancellationToken;
        use oxiarc_core::error::OxiArcError;

        let token = CancellationToken::new();
        token.cancel();
        let writer = XzWriter::new(LzmaLevel::new(6)).with_cancel(token);
        let data: Vec<u8> = (0..100).map(|_| b'A').collect();
        let result = writer.compress(&data);
        assert!(matches!(result, Err(OxiArcError::Cancelled)));
    }
}
