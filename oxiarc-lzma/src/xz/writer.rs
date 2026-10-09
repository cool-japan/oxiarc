//! `.xz` stream writer.
//!
//! Split out of `header.rs` (which owns the reader and the shared header
//! types) purely for file size; the writer needs nothing from the reader
//! but the header constants and `StreamFlags`/`CheckType`, which it
//! imports below.
//!
//! ## Progress / cancellation
//!
//! [`XzWriter`] and [`XzStreamWriter`] expose `.with_progress()` /
//! `.with_cancel()` builders. The hooks are emitted by this wrapper itself —
//! the underlying `oxiarc-lzma` LZMA2 encoder does not expose per-chunk
//! builders — so granularity is one block.
//!
//! ## Two writers
//!
//! [`XzWriter`] is the one-shot writer: [`compress`](XzWriter::compress) takes
//! the whole payload and returns a `Vec<u8>`. [`XzStreamWriter`] is its
//! streaming counterpart and implements [`std::io::Write`], so a payload of
//! any size can be piped through it with a bounded buffer (one block) instead
//! of being materialised first. Both share the same framing helpers
//! ([`write_stream_header_to`], [`write_block_to`], [`build_index`],
//! [`write_stream_footer_to`]) and, at the same block size, produce
//! byte-identical output.

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
        write_stream_header_to(&mut output, stream_flags)?;

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
            records.push((
                write_block_to(&mut output, self.level, self.check_type, data)?,
                0,
            ));
        } else {
            for chunk in data.chunks(block_size) {
                if let Some(ref token) = self.cancel {
                    token.check()?;
                }
                records.push((
                    write_block_to(&mut output, self.level, self.check_type, chunk)?,
                    chunk.len(),
                ));
                processed += chunk.len() as u64;
                if let Some(ref handle) = self.progress {
                    if processed < total {
                        handle.on_progress(processed, Some(total));
                    }
                }
            }
        }

        // Write index
        let index = build_index(&records);
        output.write_all(&index)?;

        // Write stream footer
        write_stream_footer_to(&mut output, stream_flags, index.len())?;

        if let Some(ref handle) = self.progress {
            handle.on_progress(total, Some(total));
            handle.on_finish();
        }

        Ok(output)
    }
}

/// Write stream header.
fn write_stream_header_to<W: Write>(writer: &mut W, flags: StreamFlags) -> Result<()> {
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
///
/// Every block is compressed by its own freshly built [`Lzma2Encoder`], so a
/// block never back-references the previous one. That is what makes
/// [`XzStreamWriter`]'s output byte-identical to [`XzWriter::compress`] at the
/// same block size, whatever sizes the caller happens to write in.
fn write_block_to<W: Write>(
    writer: &mut W,
    level: LzmaLevel,
    check_type: CheckType,
    data: &[u8],
) -> Result<usize> {
    // Compress data with LZMA2
    let encoder = Lzma2Encoder::new(level);
    let compressed = encoder.encode(data)?;

    // Calculate dictionary size props
    let dict_size = level.dict_size();
    let dict_props = props_from_dict_size(dict_size);

    // Build compressed size as multibyte int
    let mut compressed_size_bytes = Vec::new();
    write_multibyte_int(&mut compressed_size_bytes, compressed.len() as u64);

    // Build uncompressed size as multibyte int
    let mut uncompressed_size_bytes = Vec::new();
    write_multibyte_int(&mut uncompressed_size_bytes, data.len() as u64);

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
    match check_type {
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
    Ok(total_header_size + compressed.len() + check_type.size())
}

/// Write a multibyte integer: 7 bits per byte, least significant group
/// first, high bit set on every byte but the last.
fn write_multibyte_int(output: &mut Vec<u8>, mut value: u64) {
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

/// Build the Index field: Index Indicator + Number of Records + one record
/// per block + padding + CRC32.
///
/// Each record's first field is the Unpadded Size — the block WITHOUT its
/// trailing padding (header + compressed data + check), per the xz format
/// spec — and the second is that block's uncompressed size. Records are in
/// the order the blocks were written.
fn build_index(records: &[(usize, usize)]) -> Vec<u8> {
    let mut index = Vec::new();

    // Index indicator
    index.push(0x00);

    // Number of records (a multibyte integer: a stream may hold more
    // than 127 blocks).
    write_multibyte_int(&mut index, records.len() as u64);

    // Records: unpadded size, uncompressed size
    for &(unpadded_size, uncompressed_size) in records {
        write_multibyte_int(&mut index, unpadded_size as u64);
        write_multibyte_int(&mut index, uncompressed_size as u64);
    }

    // Pad to 4 bytes
    while (index.len() + 4) % 4 != 0 {
        index.push(0x00);
    }

    // CRC32
    let crc = Crc32::compute(&index);
    index.extend_from_slice(&crc.to_le_bytes());

    index
}

/// Write stream footer.
fn write_stream_footer_to<W: Write>(
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
    // reject. Mirrors the `try_from` guard `read_footer` applies to
    // the same field on the decode side.
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

// =============================================================================
// Streaming writer
// =============================================================================

/// Streaming `.xz` writer implementing [`std::io::Write`].
///
/// [`XzWriter`] is one-shot: [`compress`](XzWriter::compress) needs the whole
/// payload up front and returns a `Vec<u8>`, so nothing can be layered on top
/// of it — a `tar.xz` archive, for instance, has to be materialised in full
/// before it can be compressed. This type closes that gap: it writes the
/// 12-byte Stream Header on construction, buffers input into one fixed-size
/// block, compresses and emits each block through
/// [`write_block_to`] as soon as the block is full, and writes the Index and
/// Stream Footer in [`finish`](XzStreamWriter::finish).
///
/// Memory use is bounded by the block size (`DEFAULT_BLOCK_SIZE`, 64 MiB, by
/// default) regardless of how much is written, and blocks are compressed
/// independently, so at the same block size the output is **byte-identical**
/// to [`XzWriter::compress`] for the same payload — no matter how the caller
/// chops up its `write()` calls.
///
/// # Example
///
/// ```
/// use oxiarc_lzma::xz::{XzStreamWriter, decompress};
/// use oxiarc_lzma::LzmaLevel;
/// use std::io::Write;
///
/// let mut out = Vec::new();
/// {
///     let mut writer = XzStreamWriter::new(&mut out, LzmaLevel::FAST)?;
///     writer.write_all(b"Hello, ")?;
///     writer.write_all(b"streaming XZ!")?;
///
///     writer.into_inner()?;
/// }
/// assert_eq!(decompress(&mut &out[..])?, b"Hello, streaming XZ!");
/// # Ok::<(), oxiarc_core::error::OxiArcError>(())
/// ```
///
/// # Drop
///
/// Dropping the writer calls [`finish`](XzStreamWriter::finish)
/// best-effort, exactly as [`ZipWriter`](crate::xz::XzWriter)'s archive
/// siblings do, so a forgotten `finish()` still yields a complete, readable
/// `.xz` stream (errors are discarded). Call `finish()`/`into_inner()`
/// yourself when the error matters.
pub struct XzStreamWriter<W: Write> {
    /// Underlying writer. `None` only between `into_inner` taking it and the
    /// enclosing `self` finishing its (now-skipped) drop — the type system
    /// rules out any further use afterwards, since `into_inner` consumes
    /// `self`.
    writer: Option<W>,
    /// Compression level handed to every block encoder.
    level: LzmaLevel,
    /// Check type used for the stream header and every block's check.
    check_type: CheckType,
    /// Stream flags derived from `check_type`, kept for the footer.
    stream_flags: StreamFlags,
    /// Largest uncompressed payload buffered before a block is emitted.
    block_size: usize,
    /// Input not yet emitted as a block.
    buffer: Vec<u8>,
    /// `(unpadded size, uncompressed size)` per block emitted so far.
    records: Vec<(usize, usize)>,
    /// Total uncompressed bytes emitted.
    bytes_processed: u64,
    /// Whether [`finish`](XzStreamWriter::finish) has already run.
    finished: bool,
    /// Optional progress sink (wrapper-emitted, one-shot per block).
    progress: Option<ProgressHandle>,
    /// Optional cancellation token checked before each block.
    cancel: Option<CancellationToken>,
}

impl<W: Write> std::fmt::Debug for XzStreamWriter<W> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("XzStreamWriter")
            .field("buffered_bytes", &self.buffer.len())
            .field("block_size", &self.block_size)
            .field("blocks", &self.records.len())
            .field("finished", &self.finished)
            .finish()
    }
}

impl<W: Write> XzStreamWriter<W> {
    /// Create a streaming writer over `writer`, writing the XZ Stream Header
    /// immediately.
    ///
    /// The check type is [`CheckType::Crc32`], the same default
    /// [`XzWriter::new`] uses; use
    /// [`with_check_type`](XzStreamWriter::with_check_type) for any other
    /// one. It cannot be a builder here because the check type is encoded in
    /// the Stream Header, which this constructor has already written.
    ///
    /// # Errors
    ///
    /// Returns [`OxiArcError::Io`] if the Stream Header cannot be written to
    /// `writer`.
    pub fn new(writer: W, level: LzmaLevel) -> Result<Self> {
        Self::with_check_type(writer, level, CheckType::Crc32)
    }

    /// Create a streaming writer that uses `check_type` for the stream and
    /// for every block's integrity check, writing the Stream Header
    /// immediately.
    ///
    /// This is the streaming counterpart of [`XzWriter::with_check_type`].
    ///
    /// # Errors
    ///
    /// Returns [`OxiArcError::Io`] if the Stream Header cannot be written to
    /// `writer`.
    pub fn with_check_type(mut writer: W, level: LzmaLevel, check_type: CheckType) -> Result<Self> {
        let stream_flags = StreamFlags::new(check_type);
        write_stream_header_to(&mut writer, stream_flags)?;

        Ok(Self {
            writer: Some(writer),
            level,
            check_type,
            stream_flags,
            block_size: usize::try_from(DEFAULT_BLOCK_SIZE).unwrap_or(usize::MAX),
            buffer: Vec::new(),
            records: Vec::new(),
            bytes_processed: 0,
            finished: false,
            progress: None,
            cancel: None,
        })
    }

    /// Set the largest uncompressed payload buffered before a block is
    /// emitted.
    ///
    /// Mirrors [`XzWriter::with_block_size`], including its consequence: at
    /// the same block size this writer reproduces
    /// [`XzWriter::compress`]'s bytes exactly. Lowering it trades compression
    /// ratio (and byte-identity with the one-shot writer) for a smaller
    /// memory ceiling; raising it above `DEFAULT_BLOCK_SIZE` risks emitting a
    /// block larger than the compressed-size limit the reader enforces. A
    /// value of zero is treated as one byte per block.
    #[must_use]
    pub fn with_block_size(mut self, uncompressed_bytes: u64) -> Self {
        self.block_size = usize::try_from(uncompressed_bytes).unwrap_or(usize::MAX);
        self
    }

    /// Attach a progress sink. Notified once per block with the cumulative
    /// uncompressed byte count, and with `on_finish()` after the stream
    /// footer has been written.
    ///
    /// Mirrors [`XzWriter::with_progress`]; the total is unknown to a
    /// streaming writer, so `None` is passed as the total.
    #[must_use]
    pub fn with_progress(mut self, handle: ProgressHandle) -> Self {
        self.progress = Some(handle);
        self
    }

    /// Attach a cancellation token. Checked before each block is compressed;
    /// a cancelled token makes the next [`write`](Write::write) or
    /// [`finish`](XzStreamWriter::finish) fail with
    /// [`OxiArcError::Cancelled`].
    ///
    /// Mirrors [`XzWriter::with_cancel`].
    #[must_use]
    pub fn with_cancel(mut self, token: CancellationToken) -> Self {
        self.cancel = Some(token);
        self
    }

    /// Build the error used when `writer` is unexpectedly `None`.
    ///
    /// Centralized so the (unreachable-via-the-public-API) message is written
    /// once rather than duplicated at every call site.
    #[cold]
    fn writer_taken_error() -> OxiArcError {
        OxiArcError::Io(std::io::Error::other(
            "XzStreamWriter: writer accessed after into_inner (unreachable via the public API)",
        ))
    }

    /// Fallibly borrow the underlying writer.
    #[inline]
    fn writer_mut(&mut self) -> Result<&mut W> {
        self.writer.as_mut().ok_or_else(Self::writer_taken_error)
    }

    /// Compress and emit one complete block, recording its index entry.
    fn emit_block(&mut self, block: &[u8]) -> Result<()> {
        if let Some(ref token) = self.cancel {
            token.check()?;
        }

        let level = self.level;
        let check_type = self.check_type;
        let unpadded = write_block_to(self.writer_mut()?, level, check_type, block)?;
        self.records.push((unpadded, block.len()));
        self.bytes_processed += block.len() as u64;
        if let Some(ref handle) = self.progress {
            handle.on_progress(self.bytes_processed, None);
        }
        Ok(())
    }

    /// Finish the stream: emit the trailing (partial) block, then the Index
    /// and Stream Footer.
    ///
    /// A stream that received no bytes at all still emits exactly one empty
    /// block, matching what [`XzWriter::compress`] produces for empty input.
    ///
    /// Idempotent: calling it more than once (including implicitly via
    /// [`Drop`]) writes the trailer only once.
    ///
    /// # Errors
    ///
    /// Returns [`OxiArcError::Cancelled`] if a cancellation token was
    /// attached and tripped, or [`OxiArcError::Io`] if the underlying writer
    /// fails.
    pub fn finish(&mut self) -> Result<()> {
        if self.finished {
            return Ok(());
        }

        let tail = std::mem::take(&mut self.buffer);
        if self.records.is_empty() || !tail.is_empty() {
            self.emit_block(&tail)?;
        }

        let index = build_index(&self.records);
        let stream_flags = self.stream_flags;
        self.writer_mut()?.write_all(&index)?;
        write_stream_footer_to(self.writer_mut()?, stream_flags, index.len())?;
        self.writer_mut()?.flush()?;

        self.finished = true;
        if let Some(ref handle) = self.progress {
            handle.on_finish();
        }
        Ok(())
    }

    /// Finish the stream and return the inner writer.
    ///
    /// # Errors
    ///
    /// As [`finish`](XzStreamWriter::finish).
    pub fn into_inner(mut self) -> Result<W> {
        let finish_result = self.finish();
        let writer = self.writer.take().ok_or_else(Self::writer_taken_error)?;
        finish_result?;
        Ok(writer)
    }

    /// Number of uncompressed bytes currently buffered, not yet emitted as a
    /// block.
    pub fn buffered_bytes(&self) -> usize {
        self.buffer.len()
    }

    /// Number of blocks emitted so far.
    pub fn blocks_written(&self) -> usize {
        self.records.len()
    }
}

impl<W: Write> Write for XzStreamWriter<W> {
    /// Buffer `buf`, emitting every complete block it fills.
    ///
    /// Only `block_size` bytes (plus the caller's slice) are ever held, so
    /// memory stays bounded however large `buf` is.
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.finished {
            return Err(std::io::Error::other("write to a finished XzStreamWriter"));
        }

        let mut remaining = buf;
        while !remaining.is_empty() {
            let room = self.block_size.saturating_sub(self.buffer.len()).max(1);
            let take = room.min(remaining.len());
            self.buffer.extend_from_slice(&remaining[..take]);
            remaining = &remaining[take..];
            if self.buffer.len() >= self.block_size {
                let block = std::mem::take(&mut self.buffer);
                self.emit_block(&block).map_err(to_io_error)?;
            }
        }

        Ok(buf.len())
    }

    /// Emit the buffered bytes as a block and flush the inner writer.
    ///
    /// Unlike [`finish`](XzStreamWriter::finish) this does not write the Index
    /// or Stream Footer: the stream stays open for further writes.
    fn flush(&mut self) -> std::io::Result<()> {
        if self.finished {
            return Err(std::io::Error::other("flush on a finished XzStreamWriter"));
        }
        if !self.buffer.is_empty() {
            let block = std::mem::take(&mut self.buffer);
            self.emit_block(&block).map_err(to_io_error)?;
        }
        self.writer_mut().map_err(to_io_error)?.flush()
    }
}

impl<W: Write> Drop for XzStreamWriter<W> {
    fn drop(&mut self) {
        // `into_inner` takes `writer`, leaving `None`, right before this
        // `self` finishes its own (now-skipped) drop; every other path still
        // has `Some` and gets the usual best-effort finish.
        if self.writer.is_some() {
            let _ = self.finish();
        }
    }
}

/// Convert an [`OxiArcError`] into an [`std::io::Error`] for the
/// [`Write`] impl, which cannot return the crate's own error type.
fn to_io_error(err: OxiArcError) -> std::io::Error {
    std::io::Error::other(err.to_string())
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
        let level = LzmaLevel::new(6);
        let check_type = CheckType::Crc32;
        let stream_flags = StreamFlags::new(check_type);

        let block_a = xorshift_bytes(0x1234_5678_9ABC_DEF0, 48 * 1024);
        let block_b: Vec<u8> = (0..96 * 1024).map(|i| (i % 251) as u8).collect();

        let mut output = Vec::new();
        write_stream_header_to(&mut output, stream_flags).expect("write stream header");
        let unpadded_a =
            write_block_to(&mut output, level, check_type, &block_a).expect("write block a");
        let unpadded_b =
            write_block_to(&mut output, level, check_type, &block_b).expect("write block b");

        // Build a genuine 2-record index (Index Indicator + Number of
        // Records + records + padding + CRC32), matching the on-disk layout
        // `build_index` produces for a single record.
        let index = build_index(&[(unpadded_a, block_a.len()), (unpadded_b, block_b.len())]);
        output.extend_from_slice(&index);

        write_stream_footer_to(&mut output, stream_flags, index.len())
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

    /// Stream `data` through [`XzStreamWriter`] in `write_chunk`-sized
    /// `write()` calls (the last call may be shorter).
    fn stream_compress(
        data: &[u8],
        write_chunk: usize,
        block_size: u64,
        check_type: CheckType,
    ) -> Vec<u8> {
        let mut out = Vec::new();
        {
            let mut writer = XzStreamWriter::with_check_type(&mut out, LzmaLevel::FAST, check_type)
                .expect("stream writer")
                .with_block_size(block_size);
            let mut pos = 0;
            while pos < data.len() {
                let end = (pos + write_chunk).min(data.len());
                writer.write_all(&data[pos..end]).expect("stream write_all");
                pos = end;
            }
            writer.finish().expect("stream finish");
        }
        out
    }

    /// Deterministic pseudo-random payload (incompressible enough to exercise
    /// the LZMA2 "stored chunk" fallback) built from a 64-bit xorshift.
    fn pseudo_random_bytes(len: usize) -> Vec<u8> {
        xorshift_bytes(0x2545_F491_4F6C_DD1D, len)
    }

    /// A large, highly compressible payload streamed through
    /// [`XzStreamWriter`] must decompress back to exactly the original, and
    /// must land in more than one block so the multi-block index is
    /// exercised.
    #[test]
    fn test_xz_stream_writer_round_trips_compressible_payload() {
        let block_size = 64 * 1024u64;
        let data: Vec<u8> = (0..(5 * block_size + 1234) as usize)
            .map(|i| b"the quick brown fox jumps over the lazy dog "[i % 44])
            .collect();
        assert!(
            data.len() > block_size as usize,
            "payload must span several blocks"
        );

        let compressed = stream_compress(&data, 7777, block_size, CheckType::Crc32);
        let decompressed = decompress_slice(&compressed).expect("decompress streamed stream");
        assert_eq!(decompressed, data, "streamed round trip lost data");
    }

    /// An incompressible payload must also round-trip: this is the path where
    /// LZMA2 falls back to uncompressed chunks and the block's Compressed Size
    /// can exceed its Uncompressed Size, so the block header's declared sizes
    /// and the index record have to agree.
    #[test]
    fn test_xz_stream_writer_round_trips_incompressible_payload() {
        let block_size = 32 * 1024u64;
        let data = pseudo_random_bytes((3 * block_size + 11) as usize);

        let compressed = stream_compress(&data, 1024, block_size, CheckType::Crc32);
        let decompressed = decompress_slice(&compressed).expect("decompress streamed stream");
        assert_eq!(decompressed, data, "incompressible round trip lost data");
    }

    /// The streaming writer must be byte-identical to the one-shot
    /// [`XzWriter::compress`] at the same block size — the buffer is filled to
    /// `block_size` and compressed on exactly the same boundaries
    /// `data.chunks(block_size)` uses, so even deliberately ragged `write()`
    /// calls cannot change a single byte of output.
    #[test]
    fn test_xz_stream_writer_is_byte_identical_to_compress() {
        let block_size = 16 * 1024u64;
        let cases: Vec<Vec<u8>> = vec![
            // Empty input (one empty block on both paths).
            Vec::new(),
            // Shorter than one block.
            b"short payload".to_vec(),
            // Exactly one block.
            vec![b'x'; block_size as usize],
            // Ragged multi-block, compressible.
            (0..(4 * block_size + 7) as usize)
                .map(|i| (i % 251) as u8)
                .collect(),
            // Ragged multi-block, incompressible.
            pseudo_random_bytes((4 * block_size + 999) as usize),
        ];

        for data in cases {
            let one_shot = XzWriter::new(LzmaLevel::FAST)
                .with_block_size(block_size)
                .compress(&data)
                .expect("one-shot compress");
            let streamed = stream_compress(&data, 333, block_size, CheckType::Crc32);
            assert_eq!(
                streamed,
                one_shot,
                "streaming output diverged from compress() for a {} byte payload",
                data.len()
            );
        }
    }

    /// Empty input still emits exactly one empty block, so the stream has a
    /// valid (if pointless) index — same as `compress(&[])`.
    #[test]
    fn test_xz_stream_writer_empty_input_matches_compress() {
        let streamed = stream_compress(&[], 4096, 64 * 1024, CheckType::Crc32);
        let one_shot = XzWriter::new(LzmaLevel::FAST)
            .compress(&[])
            .expect("compress empty");
        assert_eq!(streamed, one_shot);
        assert_eq!(
            decompress_slice(&streamed).expect("decompress empty stream"),
            Vec::<u8>::new()
        );
    }

    /// Option parity with the one-shot writer: every check type the format
    /// defines must stream and verify, and the buffer must stay bounded by
    /// the block size no matter how much the caller hands over at once.
    #[test]
    fn test_xz_stream_writer_check_types_and_bounded_buffer() {
        let block_size = 8 * 1024u64;
        let data = pseudo_random_bytes(10 * block_size as usize);

        for check_type in [
            CheckType::None,
            CheckType::Crc32,
            CheckType::Crc64,
            CheckType::Sha256,
        ] {
            let mut out = Vec::new();
            {
                let mut writer =
                    XzStreamWriter::with_check_type(&mut out, LzmaLevel::FAST, check_type)
                        .expect("stream writer")
                        .with_block_size(block_size);
                // One single oversized write: the buffer must never hold more
                // than one block.
                writer.write_all(&data).expect("oversized write");
                assert!(
                    writer.buffered_bytes() < block_size as usize,
                    "buffer grew to {} bytes with a {block_size} byte block size",
                    writer.buffered_bytes()
                );
                assert_eq!(writer.blocks_written(), data.len() / block_size as usize);
                writer.finish().expect("finish");
            }
            let decompressed = decompress_slice(&out).expect("decompress");
            assert_eq!(decompressed, data, "check type {check_type:?} lost data");
        }
    }

    /// `into_inner` finishes the stream and hands back the inner writer, and
    /// `Drop` (which runs afterwards) must not append a second trailer.
    #[test]
    fn test_xz_stream_writer_into_inner_finishes_once() {
        let mut out = Vec::new();
        {
            let inner = {
                let mut writer = XzStreamWriter::new(&mut out, LzmaLevel::FAST).expect("new");
                writer.write_all(b"payload").expect("write");
                writer.into_inner().expect("into_inner")
            };
            // The inner writer is still usable after the stream was finished.
            inner.write_all(b"").expect("inner write");
        }
        assert_eq!(
            decompress_slice(&out).expect("decompress"),
            b"payload".to_vec()
        );
    }

    /// Dropping without calling `finish()` still yields a complete stream
    /// (best-effort finish), matching the archive writers' drop contract.
    #[test]
    fn test_xz_stream_writer_drop_finishes_stream() {
        let mut out = Vec::new();
        {
            let mut writer = XzStreamWriter::new(&mut out, LzmaLevel::FAST).expect("new");
            writer.write_all(b"dropped without finish").expect("write");
        }
        assert_eq!(
            decompress_slice(&out).expect("decompress"),
            b"dropped without finish".to_vec()
        );
    }

    /// Writing after `finish()` is an error, not a silent corruption.
    #[test]
    fn test_xz_stream_writer_write_after_finish_errors() {
        let mut out = Vec::new();
        let mut writer = XzStreamWriter::new(&mut out, LzmaLevel::FAST).expect("new");
        writer.finish().expect("finish");
        assert!(writer.write_all(b"late").is_err());
    }

    /// A tripped cancellation token aborts the stream instead of compressing
    /// another block, mirroring `XzWriter::compress`.
    #[test]
    fn test_xz_stream_writer_cancel_forwarding() {
        use oxiarc_core::cancel::CancellationToken;

        let token = CancellationToken::new();
        token.cancel();

        let mut out = Vec::new();
        let mut writer = XzStreamWriter::new(&mut out, LzmaLevel::FAST)
            .expect("new")
            .with_cancel(token)
            .with_block_size(16);
        // The first block fills, tripping the check.
        let err = writer
            .write_all(&[b'A'; 64])
            .expect_err("a cancelled token must abort the block");
        assert!(err.to_string().contains("ancel"), "unexpected error: {err}");
    }

    /// Progress reporting parity: one notification per emitted block plus a
    /// final `on_finish()`.
    #[test]
    fn test_xz_stream_writer_progress_forwarding() {
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

        let block_size = 4096u64;
        let data = vec![b'A'; 4 * block_size as usize];
        let mut out = Vec::new();
        {
            let mut writer = XzStreamWriter::new(&mut out, LzmaLevel::FAST)
                .expect("new")
                .with_progress(handle)
                .with_block_size(block_size);
            writer.write_all(&data).expect("write");
            writer.finish().expect("finish");
        }

        assert_eq!(sink.progress_count.load(Ordering::SeqCst), 4);
        assert_eq!(sink.finish_count.load(Ordering::SeqCst), 1);
        assert_eq!(
            sink.last_processed.load(Ordering::SeqCst),
            data.len() as u64
        );
    }
}
