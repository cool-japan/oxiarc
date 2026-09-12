//! Streaming compression and decompression for Brotli.
//!
//! Provides a genuinely incremental `Write`-based compressor and a
//! `Read`-based decompressor adapter.
//!
//! ## [`BrotliCompressor`] — incremental
//!
//! Written data is buffered only up to a bounded threshold
//! ([`BrotliCompressor::with_max_input`], default one meta-block). Once the
//! threshold is reached — and on every explicit [`flush`](std::io::Write::flush) — the
//! buffered input is encoded as one or more Brotli content meta-blocks
//! (`ISLAST = 0`) and every *complete* byte of the resulting bitstream is
//! pushed to the inner writer. Peak memory is therefore bounded by the
//! threshold rather than by the total input size.
//!
//! A Brotli stream is a single continuous bitstream, so a `flush` can push
//! only whole bytes; a residue of at most 7 bits from the final meta-block
//! stays buffered until the next meta-block completes it or
//! [`BrotliCompressor::finish`] pads the stream. This is inherent to the
//! bit-packed format, not a buffering shortcut — `flush` really does compress
//! and emit everything it validly can.
//!
//! [`BrotliCompressor::finish`] flushes the last buffered input, writes the
//! empty final meta-block, pads to a byte boundary and returns the inner
//! writer.
//!
//! Because the encoder emits incrementally it cannot retroactively re-verify
//! the whole stream the way the one-shot [`crate::compress::compress`] path
//! does; the per-meta-block encoder it shares is the same code that path
//! round-trips and that reference decoders accept.
//!
//! ## [`BrotliDecompressor`] — incremental
//!
//! A thin `Read` shell over [`crate::BrotliStream`]. It pulls at most 64 KiB
//! of compressed data at a time and decodes straight into the caller's buffer,
//! so the first byte is available long before the source reaches EOF and peak
//! memory is `O(window)` rather than `O(decompressed size)`. A `read` that
//! cannot be satisfied from one refill loops; `ErrorKind::Interrupted` from the
//! inner reader is retried, `WouldBlock` propagates unchanged (the decoder
//! state stays intact for the retry), and an inner `Ok(0)` switches the decoder
//! to [`FlushMode::Finish`](oxiarc_core::traits::FlushMode::Finish) so a
//! truncated stream is an error rather than a short read.
//!
//! ## Drop behavior
//!
//! Dropping a [`BrotliCompressor`] that has emitted or still holds data
//! performs a best-effort finish so the sink never receives an unterminated
//! (invalid) stream; that best-effort path **silently swallows I/O and
//! encoding errors**. Always call [`BrotliCompressor::finish`] explicitly when
//! you need to observe failures — between the last write and an unchecked
//! drop, a write error is lost.
//!
//! ## Progress and Cancellation
//!
//! Both types support optional progress reporting and cooperative
//! cancellation via `oxiarc-core` primitives:
//!
//! ```rust,no_run
//! use std::io::Write;
//! use oxiarc_brotli::streaming::BrotliCompressor;
//! use oxiarc_brotli::compress::BrotliParams;
//! use oxiarc_core::{CancellationToken, noop_progress};
//!
//! let token = CancellationToken::new();
//! let mut output = Vec::new();
//! let params = BrotliParams::default();
//! let mut compressor = BrotliCompressor::new(&mut output, params)
//!     .with_progress(noop_progress())
//!     .with_cancel(token.clone());
//! compressor.write_all(b"Hello, Brotli!").expect("write");
//! let _ = compressor.finish();
//! ```

use std::io::{self, Read, Write};

use oxiarc_core::cancel::CancellationToken;
use oxiarc_core::progress::ProgressHandle;

use oxiarc_core::traits::FlushMode;

use crate::bit_writer::BitWriter;
use crate::compress::{BrotliParams, EncoderState, encode_meta_block, write_window_bits};
use crate::decompress::decompress_with_hooks;
use crate::error::BrotliError;
use crate::pool::BrotliPool;
use crate::stream::{BrotliStatus, BrotliStream};

/// Default buffer size for streaming operations (256KB).
const DEFAULT_BUF_SIZE: usize = 256 * 1024;

/// A streaming Brotli compressor that implements `Write`.
///
/// Written data is buffered only up to a bounded threshold and encoded into
/// Brotli meta-blocks incrementally, so peak memory does not grow with the
/// total input. On each [`flush`](std::io::Write::flush) every complete compressed byte is
/// pushed to the inner writer (a sub-byte residue is inherent to the format).
/// Call [`BrotliCompressor::finish`] explicitly to write the stream
/// terminator and observe any error; the `Drop` fallback completes the stream
/// best-effort and silently discards errors.
///
/// Supports optional progress reporting via [`ProgressHandle`] and
/// cooperative cancellation via [`CancellationToken`].
///
/// # Example
///
/// ```rust,no_run
/// use std::io::Write;
/// use oxiarc_brotli::streaming::BrotliCompressor;
/// use oxiarc_brotli::compress::BrotliParams;
///
/// let mut output = Vec::new();
/// let params = BrotliParams::default();
/// let mut compressor = BrotliCompressor::new(&mut output, params);
/// compressor.write_all(b"Hello, Brotli!").expect("write");
/// let output = compressor.finish().expect("finish");
/// ```
pub struct BrotliCompressor<W: Write> {
    /// Inner writer that receives compressed data.
    inner: Option<W>,
    /// Compression parameters.
    params: BrotliParams,
    /// Pending (not-yet-encoded) input.
    buffer: Vec<u8>,
    /// Persistent output bitstream; drained (whole bytes) into `inner`.
    writer: BitWriter,
    /// Distance-ring state carried across meta-blocks / calls.
    state: EncoderState,
    /// Whether the stream's window header has been emitted into `writer`.
    header_written: bool,
    /// Whether the stream has been terminated (final meta-block written).
    finished: bool,
    /// Buffer threshold: `write` emits meta-blocks once `buffer` reaches this.
    max_input: usize,
    /// Optional progress sink; receives cumulative `on_progress` per drain.
    progress: Option<ProgressHandle>,
    /// Optional cancellation token; checked before each meta-block emission.
    cancel: Option<CancellationToken>,
    /// Cumulative compressed bytes pushed to the inner writer.
    bytes_out: u64,
    /// Optional buffer pool for per-encode allocations.
    pool: Option<BrotliPool>,
}

impl<W: Write> BrotliCompressor<W> {
    /// Create a new streaming Brotli compressor.
    ///
    /// Invalid `params` are not rejected here (the constructor is infallible);
    /// they surface as an `io::Error` from the first `write`/`flush`/`finish`
    /// that touches the encoder, matching the one-shot path. `block_size()` is
    /// only consulted for validated params, since an out-of-range `lgblock`
    /// would otherwise overflow the shift.
    pub fn new(inner: W, params: BrotliParams) -> Self {
        let max_input = if params.validate().is_ok() {
            params.block_size().max(1)
        } else {
            DEFAULT_BUF_SIZE
        };
        BrotliCompressor {
            inner: Some(inner),
            params,
            buffer: Vec::with_capacity(DEFAULT_BUF_SIZE),
            writer: BitWriter::new(),
            state: EncoderState::new(),
            header_written: false,
            finished: false,
            max_input,
            progress: None,
            cancel: None,
            bytes_out: 0,
            pool: None,
        }
    }

    /// Create a new streaming compressor with a custom buffering threshold.
    ///
    /// `buf_size` (minimum 1 KiB) is the number of buffered input bytes that
    /// triggers an incremental meta-block emission during [`Write::write`]. A
    /// smaller value bounds memory more tightly at a small ratio cost.
    pub fn with_buffer_size(inner: W, params: BrotliParams, buf_size: usize) -> Self {
        let mut compressor = Self::new(inner, params);
        compressor.max_input = buf_size.max(1024);
        compressor
    }

    /// Set the buffered-input threshold that triggers incremental emission.
    ///
    /// Bounds peak memory: at most `max_input` (minimum 1 KiB) bytes of input
    /// plus one meta-block of output are held before bytes are pushed to the
    /// inner writer.
    #[must_use]
    pub fn with_max_input(mut self, max_input: usize) -> Self {
        self.max_input = max_input.max(1024);
        self
    }

    /// Attach a buffer pool to amortise per-encode allocations.
    ///
    /// The pool is cloned internally (cheap `Arc` clone) so the caller can
    /// reuse the same pool across multiple compressors without lifetime
    /// constraints.
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// use std::io::Write;
    /// use oxiarc_brotli::streaming::BrotliCompressor;
    /// use oxiarc_brotli::compress::BrotliParams;
    /// use oxiarc_brotli::pool::BrotliPool;
    ///
    /// let pool = BrotliPool::new();
    /// let mut output = Vec::new();
    /// let params = BrotliParams::default();
    /// let mut compressor = BrotliCompressor::new(&mut output, params)
    ///     .with_pool(&pool);
    /// compressor.write_all(b"Hello, pooled Brotli!").expect("write");
    /// let _ = compressor.finish();
    /// ```
    #[must_use]
    pub fn with_pool(mut self, pool: &BrotliPool) -> Self {
        self.pool = Some(pool.clone());
        self
    }

    /// Attach a progress sink.
    ///
    /// The sink's `on_progress(bytes_out, None)` is called with the cumulative
    /// compressed byte count each time bytes are drained to the inner writer
    /// (i.e. on threshold emission, on `flush`, and on `finish`).
    #[must_use]
    pub fn with_progress(mut self, handle: ProgressHandle) -> Self {
        self.progress = Some(handle);
        self
    }

    /// Attach a cancellation token.
    ///
    /// The token is checked before each meta-block emission. If it has been
    /// cancelled, the operation returns an I/O error with the message
    /// `"operation cancelled"`.
    #[must_use]
    pub fn with_cancel(mut self, token: CancellationToken) -> Self {
        self.cancel = Some(token);
        self
    }

    /// Finish compression and return the inner writer.
    ///
    /// Encodes any remaining buffered input, writes the empty final
    /// meta-block, pads to a byte boundary, drains everything to the inner
    /// writer and returns it.
    pub fn finish(mut self) -> io::Result<W> {
        self.do_finish()?;
        self.inner
            .take()
            .ok_or_else(|| io::Error::other("compressor already finished"))
    }

    /// Emit the one-time window header into the persistent bitstream.
    ///
    /// Validates the parameters first, so an invalid `quality`/`lgwin`/
    /// `lgblock` surfaces as a typed error exactly where the one-shot encoder
    /// would reject it — before any meta-block (or `block_size()` shift) runs.
    fn ensure_header(&mut self) -> io::Result<()> {
        if !self.header_written {
            self.params
                .validate()
                .map_err(|e| io::Error::other(e.to_string()))?;
            write_window_bits(&mut self.writer, self.params.lgwin)
                .map_err(|e| io::Error::other(e.to_string()))?;
            self.header_written = true;
        }
        Ok(())
    }

    /// Drain every complete byte of the bitstream into the inner writer,
    /// updating the cumulative counter and firing progress.
    fn drain_to_inner(&mut self) -> io::Result<()> {
        let bytes = self.writer.drain_complete_bytes();
        if bytes.is_empty() {
            return Ok(());
        }
        if let Some(ref mut writer) = self.inner {
            writer.write_all(&bytes)?;
        }
        self.bytes_out += bytes.len() as u64;
        if let Some(ref handle) = self.progress {
            handle.on_progress(self.bytes_out, None);
        }
        Ok(())
    }

    /// Encode all currently-buffered input as content meta-blocks and drain
    /// the complete bytes produced. A no-op when nothing is buffered.
    fn emit_buffered(&mut self) -> io::Result<()> {
        if let Some(ref token) = self.cancel {
            token.check().map_err(|e| io::Error::other(e.to_string()))?;
        }
        if self.buffer.is_empty() {
            return Ok(());
        }
        self.ensure_header()?;
        let data = std::mem::take(&mut self.buffer);
        let block_size = self.params.block_size().max(1);
        for chunk in data.chunks(block_size) {
            encode_meta_block(
                &mut self.writer,
                chunk,
                &self.params,
                &mut self.state,
                self.pool.as_ref(),
            )
            .map_err(|e| io::Error::other(e.to_string()))?;
            self.drain_to_inner()?;
        }
        Ok(())
    }

    /// Internal finish implementation (idempotent).
    fn do_finish(&mut self) -> io::Result<()> {
        if self.finished {
            return Ok(());
        }
        // Encode whatever remains buffered.
        self.emit_buffered()?;
        // A valid stream needs the window header even if nothing was written.
        self.ensure_header()?;
        // Empty last meta-block: ISLAST = 1, ISLASTEMPTY = 1.
        self.writer
            .write_bit(true)
            .map_err(|e| io::Error::other(e.to_string()))?;
        self.writer
            .write_bit(true)
            .map_err(|e| io::Error::other(e.to_string()))?;
        // Pad the trailing partial byte and drain everything.
        self.writer.flush();
        self.drain_to_inner()?;
        if let Some(ref mut writer) = self.inner {
            writer.flush()?;
        }
        self.finished = true;
        Ok(())
    }
}

impl<W: Write> Write for BrotliCompressor<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.finished {
            return Err(io::Error::other("write after finish"));
        }
        self.buffer.extend_from_slice(buf);
        // Note: if the threshold emission fails (sink or encoder error), `buf`
        // has already been absorbed into the stream, so this reports `Err`
        // after taking the bytes. A caller retrying the same bytes via
        // `write_all` would duplicate them; treat a `write` error as terminal
        // for the stream (drop or stop), do not retry the same input.
        if self.buffer.len() >= self.max_input {
            self.emit_buffered()?;
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.finished {
            return Ok(());
        }
        // Compress and push every complete byte we can; a <=7-bit residue of
        // the final meta-block necessarily stays until the next block/finish.
        self.emit_buffered()?;
        if let Some(ref mut writer) = self.inner {
            writer.flush()?;
        }
        Ok(())
    }
}

impl<W: Write> Drop for BrotliCompressor<W> {
    fn drop(&mut self) {
        // Complete the stream best-effort if we have committed anything to the
        // sink (header emitted) or still hold buffered input; otherwise there
        // is nothing to terminate and an untouched sink stays empty. Skipping
        // this once committed would leave an unterminated, invalid stream.
        if !self.finished
            && self.inner.is_some()
            && (self.header_written || !self.buffer.is_empty())
        {
            let _ = self.do_finish();
        }
    }
}

/// Size of the compressed-input staging buffer pulled from the inner reader.
const DECODE_STAGING: usize = 64 * 1024;

/// Hard ceiling on the staging buffer.
///
/// The buffer only grows past [`DECODE_STAGING`] when the decoder consumed
/// nothing from a full buffer — which can only happen inside an atomic
/// meta-block-header retry. [`BrotliStream`] rejects a header longer than
/// 1 MiB, so this ceiling is never the thing that stops a valid stream; it just
/// makes the adapter's own memory bound explicit instead of implied.
const MAX_STAGING: usize = 4 * 1024 * 1024;

/// A streaming Brotli decompressor that implements `Read`.
///
/// A thin shell over [`BrotliStream`]: at most 64 KiB of compressed data is
/// held at a time and decoded straight into the caller's buffer, so output is
/// available before the source reaches EOF and peak memory is bounded by the
/// stream's sliding window rather than by the decompressed size.
///
/// I/O behaviour:
///
/// * `ErrorKind::Interrupted` from the inner reader is retried;
/// * `ErrorKind::WouldBlock` propagates unchanged, with the decoder state
///   intact so the same `read` can simply be retried;
/// * an inner `Ok(0)` means end of input, which switches the decoder to
///   [`FlushMode::Finish`] — a stream that ends mid-meta-block is then an
///   error, never a short read;
/// * `read(&mut [])` returns `Ok(0)` without touching the inner reader;
/// * a source that is empty from the very first read yields `Ok(0)` rather
///   than an error. This is the behaviour this type has always had and is kept
///   deliberately: a zero-byte body is treated as "nothing to decode", not as a
///   truncated stream. The strict reading — a zero-byte body is not a valid
///   Brotli stream — is what [`crate::decompress()`] and [`BrotliStream`] apply.
///
/// Supports optional progress reporting via [`ProgressHandle`],
/// cooperative cancellation via [`CancellationToken`], and a memory budget
/// via [`BrotliDecompressor::with_max_output`].
///
/// # Example
///
/// ```rust
/// use std::io::Read;
/// use oxiarc_brotli::{compress, streaming::BrotliDecompressor};
///
/// let compressed = compress(b"decoded as it arrives", 5).expect("compress");
/// let mut decompressor = BrotliDecompressor::new(&compressed[..]);
/// let mut output = Vec::new();
/// decompressor.read_to_end(&mut output).expect("read");
/// assert_eq!(output, b"decoded as it arrives");
/// ```
pub struct BrotliDecompressor<R: Read> {
    /// Inner reader providing compressed data.
    inner: R,
    /// The bounded push decoder doing the actual work.
    stream: BrotliStream,
    /// Compressed bytes staged from `inner`.
    staging: Vec<u8>,
    /// How many bytes of `staging` are valid.
    staged: usize,
    /// How many bytes of `staging` the decoder has taken.
    staged_pos: usize,
    /// Whether `inner` has reported end of input.
    input_done: bool,
    /// Whether the decoder reported [`BrotliStatus::StreamEnd`].
    finished: bool,
    /// Optional progress sink, forwarded to the decoder.
    progress: Option<ProgressHandle>,
    /// Optional cancellation token, forwarded to the decoder.
    cancel: Option<CancellationToken>,
    /// Optional memory budget, forwarded to the decoder.
    max_output: Option<usize>,
    /// Whether `stream` has been configured from the builders above.
    configured: bool,
}

impl<R: Read> BrotliDecompressor<R> {
    /// Create a new streaming Brotli decompressor.
    pub fn new(inner: R) -> Self {
        BrotliDecompressor {
            inner,
            stream: BrotliStream::new(),
            staging: Vec::new(),
            staged: 0,
            staged_pos: 0,
            input_done: false,
            finished: false,
            progress: None,
            cancel: None,
            max_output: None,
            configured: false,
        }
    }

    /// Attach a progress sink.
    ///
    /// The sink's `on_progress(bytes_out_so_far, None)` is called after every
    /// decoded meta-block.
    #[must_use]
    pub fn with_progress(mut self, handle: ProgressHandle) -> Self {
        self.progress = Some(handle);
        self
    }

    /// Attach a cancellation token.
    ///
    /// The token is checked at every meta-block boundary. If it has been
    /// cancelled, reading returns an I/O error with the message
    /// `"operation cancelled"`.
    #[must_use]
    pub fn with_cancel(mut self, token: CancellationToken) -> Self {
        self.cancel = Some(token);
        self
    }

    /// Cap the decompressed output at `max_output` bytes.
    ///
    /// The cap is enforced *during* decoding — before the meta-block that
    /// would exceed it is decoded — so an over-budget stream is rejected
    /// without its expansion being produced, and without the compressed
    /// remainder being downloaded. Reading then fails with an I/O error
    /// carrying [`crate::BrotliError::MemoryBudgetExceeded`]'s message.
    /// Without this setting the crate's default 256 MB guard applies.
    #[must_use]
    pub fn with_max_output(mut self, max_output: usize) -> Self {
        self.max_output = Some(max_output);
        self
    }

    /// Refuse a stream whose declared sliding window exceeds `bytes`.
    ///
    /// Checked while reading the stream header, before the window is
    /// allocated. Defaults to [`crate::DEFAULT_MAX_WINDOW`] (16 MiB), which
    /// admits every RFC 7932 window.
    #[must_use]
    pub fn with_max_window(mut self, bytes: usize) -> Self {
        self.stream = std::mem::take(&mut self.stream).with_max_window(bytes);
        self
    }

    /// Attach a shared (custom LZ77) dictionary.
    ///
    /// The source's backward references may then reach into `dictionary`; this
    /// is the `Read`-shaped counterpart of
    /// [`BrotliStream::with_dictionary`](crate::BrotliStream::with_dictionary)
    /// and is what an HTTP client feeds a `Content-Encoding: dcb` body after
    /// stripping its 36-byte header (see [`crate::dcb`]).
    ///
    /// # Example
    ///
    /// ```rust
    /// use std::io::Read;
    /// use oxiarc_brotli::{compress_with_dictionary, BrotliDecompressor, BrotliParams};
    ///
    /// let dictionary = b"a dictionary both peers hold".repeat(16);
    /// let params = BrotliParams { quality: 9, ..BrotliParams::default() };
    /// let compressed =
    ///     compress_with_dictionary(b"a dictionary both peers hold!", &dictionary, &params)
    ///         .expect("compress");
    ///
    /// let mut out = Vec::new();
    /// BrotliDecompressor::new(&compressed[..])
    ///     .with_dictionary(dictionary)
    ///     .read_to_end(&mut out)
    ///     .expect("decompress");
    /// assert_eq!(out, b"a dictionary both peers hold!");
    /// ```
    #[must_use]
    pub fn with_dictionary(mut self, dictionary: Vec<u8>) -> Self {
        self.stream = std::mem::take(&mut self.stream).with_dictionary(dictionary);
        self
    }

    /// Apply the builder settings to the decoder on first use.
    fn configure(&mut self) {
        if self.configured {
            return;
        }
        self.configured = true;
        let mut stream = std::mem::take(&mut self.stream);
        if let Some(limit) = self.max_output {
            stream = stream.with_max_output(limit as u64);
        }
        if let Some(handle) = self.progress.clone() {
            stream = stream.with_progress(handle);
        }
        if let Some(token) = self.cancel.clone() {
            stream = stream.with_cancel(token);
        }
        self.stream = stream;
    }

    /// Pull one staging buffer's worth of compressed bytes from `inner`.
    ///
    /// Retries `Interrupted`; propagates every other error, `WouldBlock`
    /// included, without disturbing the decoder.
    fn refill(&mut self) -> io::Result<()> {
        if self.staging.is_empty() {
            self.staging = vec![0u8; DECODE_STAGING];
        }
        if self.staged_pos > 0 {
            self.staging.copy_within(self.staged_pos..self.staged, 0);
            self.staged -= self.staged_pos;
            self.staged_pos = 0;
        }
        if self.staged == self.staging.len() {
            // Nothing consumed from a full buffer: the decoder is holding it
            // all inside an atomic meta-block-header parse. Grow so progress is
            // still possible.
            if self.staging.len() >= MAX_STAGING {
                return Err(io::Error::other(
                    "brotli meta-block header exceeds the staging buffer",
                ));
            }
            let grown = (self.staging.len() * 2).min(MAX_STAGING);
            self.staging.resize(grown, 0);
        }
        loop {
            match self.inner.read(&mut self.staging[self.staged..]) {
                Ok(0) => {
                    self.input_done = true;
                    return Ok(());
                }
                Ok(n) => {
                    self.staged += n;
                    return Ok(());
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
    }
}

impl<R: Read> Read for BrotliDecompressor<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() || self.finished {
            return Ok(0);
        }
        self.configure();
        loop {
            let flush = if self.input_done {
                FlushMode::Finish
            } else {
                FlushMode::None
            };
            let progress = self
                .stream
                .decode(&self.staging[self.staged_pos..self.staged], buf, flush)
                .map_err(io::Error::from)?;
            self.staged_pos += progress.consumed;
            if progress.status == BrotliStatus::StreamEnd {
                self.finished = true;
            }
            if progress.produced > 0 {
                return Ok(progress.produced);
            }
            if self.finished {
                return Ok(0);
            }
            if progress.status == BrotliStatus::NeedOutput {
                // `buf` is non-empty, so a decoder that produced nothing while
                // asking for more room cannot make progress.
                return Err(io::Error::other("brotli decoder made no progress"));
            }
            if self.input_done {
                // `FlushMode::Finish` above turns a genuine shortfall into an
                // error, so reaching here means the decoder is idling.
                return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
            }
            self.refill()?;
            if self.input_done && self.stream.total_in() == 0 {
                // The source was empty from the start: preserve this type's
                // long-standing "nothing to decode" answer rather than
                // reporting a truncated stream.
                self.finished = true;
                return Ok(0);
            }
        }
    }
}

/// Convenience function: compress data and write to a writer.
pub fn compress_to_writer<W: Write>(
    data: &[u8],
    writer: &mut W,
    quality: u32,
) -> Result<(), BrotliError> {
    let params = BrotliParams {
        quality,
        ..BrotliParams::default()
    };
    let compressed = crate::compress::compress_with_hooks_pooled(data, &params, None, None, None)?;
    writer.write_all(&compressed).map_err(BrotliError::from)?;
    Ok(())
}

/// Convenience function: decompress data from a reader.
pub fn decompress_from_reader<R: Read>(reader: &mut R) -> Result<Vec<u8>, BrotliError> {
    let mut compressed = Vec::new();
    reader
        .read_to_end(&mut compressed)
        .map_err(BrotliError::from)?;
    decompress_with_hooks(&compressed, None, None, None)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_compressor_basic() {
        let mut output = Vec::new();
        {
            let params = BrotliParams {
                quality: 0,
                ..BrotliParams::default()
            };
            let compressor = BrotliCompressor::new(&mut output, params);
            let _ = compressor.finish();
        }
        // Even without writing, finish produces a valid stream.
        assert!(!output.is_empty());
    }

    #[test]
    fn test_compressor_with_data() {
        let mut output = Vec::new();
        {
            let params = BrotliParams {
                quality: 0,
                ..BrotliParams::default()
            };
            let mut compressor = BrotliCompressor::new(&mut output, params);
            compressor.write_all(b"Hello, Brotli!").ok();
            let _ = compressor.finish();
        }
        assert!(!output.is_empty());
    }

    #[test]
    fn test_compressor_empty() {
        let mut output = Vec::new();
        {
            let params = BrotliParams::default();
            let compressor = BrotliCompressor::new(&mut output, params);
            // Don't write anything, just finish.
            let _ = compressor.finish();
        }
        // Even empty data should produce some output (empty stream).
        assert!(!output.is_empty());
    }

    #[test]
    fn test_compressor_with_buffer_size() {
        let mut output = Vec::new();
        let params = BrotliParams {
            quality: 0,
            ..BrotliParams::default()
        };
        let mut compressor = BrotliCompressor::with_buffer_size(&mut output, params, 64);
        compressor.write_all(b"test data").ok();
        let _ = compressor.finish();
        assert!(!output.is_empty());
    }

    #[test]
    fn test_decompressor_placeholder() {
        // This test verifies the decompressor can be constructed.
        let data: Vec<u8> = Vec::new();
        let decompressor = BrotliDecompressor::new(&data[..]);
        assert!(!decompressor.finished);
    }

    #[test]
    fn test_compress_to_writer_fn() {
        let mut output = Vec::new();
        let result = compress_to_writer(b"test", &mut output, 0);
        assert!(result.is_ok());
        assert!(!output.is_empty());
    }

    #[test]
    fn test_compressor_with_progress_builder() {
        use oxiarc_core::noop_progress;
        let mut output = Vec::new();
        let params = BrotliParams {
            quality: 0,
            ..BrotliParams::default()
        };
        let mut compressor =
            BrotliCompressor::new(&mut output, params).with_progress(noop_progress());
        compressor.write_all(b"progress test data").ok();
        let _ = compressor.finish();
        assert!(!output.is_empty());
    }

    #[test]
    fn test_compressor_with_cancel_builder() {
        use oxiarc_core::cancel::CancellationToken;
        let mut output = Vec::new();
        let params = BrotliParams {
            quality: 0,
            ..BrotliParams::default()
        };
        let token = CancellationToken::new();
        let mut compressor = BrotliCompressor::new(&mut output, params).with_cancel(token);
        compressor.write_all(b"cancel test data").ok();
        let _ = compressor.finish();
        // Completed without cancellation.
        assert!(!output.is_empty());
    }

    #[test]
    fn test_decompressor_with_progress_builder() {
        use crate::compress::compress;
        use oxiarc_core::noop_progress;

        let data = b"hello decompressor progress";
        let compressed = compress(data, 0).expect("compress");
        let decompressor = BrotliDecompressor::new(&compressed[..]).with_progress(noop_progress());
        let mut output = Vec::new();
        let mut d = decompressor;
        d.read_to_end(&mut output).expect("decompress");
        assert_eq!(output, data);
    }

    #[test]
    fn test_decompressor_with_cancel_builder() {
        use crate::compress::compress;
        use oxiarc_core::cancel::CancellationToken;

        let data = b"hello decompressor cancel";
        let compressed = compress(data, 0).expect("compress");
        let token = CancellationToken::new();
        let decompressor = BrotliDecompressor::new(&compressed[..]).with_cancel(token);
        let mut output = Vec::new();
        let mut d = decompressor;
        d.read_to_end(&mut output)
            .expect("decompress without cancel");
        assert_eq!(output, data);
    }

    // ── Incremental streaming tests ───────────────────────────────────────────

    /// A `Write` sink backed by a shared buffer so a test can observe what the
    /// compressor has actually pushed *before* `finish` consumes it.
    #[derive(Clone)]
    struct SharedSink(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl SharedSink {
        fn new() -> Self {
            SharedSink(std::sync::Arc::new(std::sync::Mutex::new(Vec::new())))
        }
        fn len(&self) -> usize {
            self.0.lock().expect("sink lock").len()
        }
        fn snapshot(&self) -> Vec<u8> {
            self.0.lock().expect("sink lock").clone()
        }
    }

    impl Write for SharedSink {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().expect("sink lock").extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn roundtrip(bytes: &[u8], expected: &[u8]) {
        let decoded = crate::decompress::decompress(bytes).expect("decompress streamed output");
        assert_eq!(decoded, expected, "streamed stream must round-trip");
    }

    /// `flush` must actually compress buffered input and push complete bytes to
    /// the sink — not the old no-op — while still round-tripping after finish.
    #[test]
    fn test_streaming_flush_pushes_complete_bytes() {
        let sink = SharedSink::new();
        let params = BrotliParams {
            quality: 5,
            ..BrotliParams::default()
        };
        let mut compressor = BrotliCompressor::new(sink.clone(), params);
        let input = vec![b'A'; 8192];
        compressor.write_all(&input).expect("write");
        compressor.flush().expect("flush");
        assert!(
            sink.len() > 0,
            "flush must push compressed bytes to the sink (was a no-op before)"
        );
        compressor.finish().expect("finish");
        roundtrip(&sink.snapshot(), &input);
    }

    /// Many single-byte writes must not accumulate unbounded input and must
    /// still produce a stream that decodes back to the exact input.
    #[test]
    fn test_streaming_many_small_writes_roundtrip() {
        let sink = SharedSink::new();
        let params = BrotliParams {
            quality: 4,
            ..BrotliParams::default()
        };
        // Tight threshold so most writes trigger an incremental emission.
        let mut compressor = BrotliCompressor::new(sink.clone(), params).with_max_input(1024);
        let input: Vec<u8> = (0u32..5000).map(|i| (i % 251) as u8).collect();
        for byte in &input {
            compressor.write_all(&[*byte]).expect("write one byte");
        }
        assert!(
            sink.len() > 0,
            "with a 1 KiB threshold and 5000 bytes, output must have started before finish"
        );
        compressor.finish().expect("finish");
        roundtrip(&sink.snapshot(), &input);
    }

    /// Interleaving writes and flushes must round-trip.
    #[test]
    fn test_streaming_flush_then_write_roundtrip() {
        let sink = SharedSink::new();
        let params = BrotliParams::default();
        let mut compressor = BrotliCompressor::new(sink.clone(), params);
        let mut expected = Vec::new();
        for chunk in ["first chunk ", "second chunk ", "third and final chunk"] {
            compressor.write_all(chunk.as_bytes()).expect("write");
            compressor.flush().expect("flush");
            expected.extend_from_slice(chunk.as_bytes());
        }
        compressor.finish().expect("finish");
        roundtrip(&sink.snapshot(), &expected);
    }

    /// A cap-breach mid-write (input far larger than the threshold) must bound
    /// memory by emitting during `write`, and still round-trip.
    #[test]
    fn test_streaming_cap_breach_midwrite_roundtrip() {
        let sink = SharedSink::new();
        let params = BrotliParams {
            quality: 6,
            ..BrotliParams::default()
        };
        let mut compressor = BrotliCompressor::new(sink.clone(), params).with_max_input(4096);
        // Compressible but varied so it is not degenerate.
        let input: Vec<u8> = (0u32..40_000).map(|i| ((i * 31 + 7) % 256) as u8).collect();
        // Write in 3000-byte chunks; each write past 4096 buffered triggers emit.
        for chunk in input.chunks(3000) {
            compressor.write_all(chunk).expect("write chunk");
        }
        compressor.finish().expect("finish");
        roundtrip(&sink.snapshot(), &input);
    }

    /// Finishing without any write must produce a valid empty stream.
    #[test]
    fn test_streaming_empty_finish_roundtrips() {
        let sink = SharedSink::new();
        let compressor = BrotliCompressor::new(sink.clone(), BrotliParams::default());
        compressor.finish().expect("finish");
        roundtrip(&sink.snapshot(), b"");
    }

    /// Quality 0 (stored meta-blocks) must also stream and round-trip.
    #[test]
    fn test_streaming_quality0_roundtrip() {
        let sink = SharedSink::new();
        let params = BrotliParams {
            quality: 0,
            ..BrotliParams::default()
        };
        let mut compressor = BrotliCompressor::new(sink.clone(), params).with_max_input(2048);
        let input: Vec<u8> = (0u32..9000).map(|i| (i % 97) as u8).collect();
        for chunk in input.chunks(1500) {
            compressor.write_all(chunk).expect("write");
            compressor.flush().expect("flush");
        }
        compressor.finish().expect("finish");
        roundtrip(&sink.snapshot(), &input);
    }

    /// Dropping after a flush (without `finish`) must still terminate the
    /// stream so the sink holds a valid, decodable brotli stream.
    #[test]
    fn test_streaming_drop_after_flush_terminates_stream() {
        let sink = SharedSink::new();
        let input = b"data written then flushed then dropped without finish";
        {
            let mut compressor = BrotliCompressor::new(sink.clone(), BrotliParams::default());
            compressor.write_all(input).expect("write");
            compressor.flush().expect("flush");
            // No finish(): Drop must complete the stream.
        }
        roundtrip(&sink.snapshot(), input);
    }

    /// Invalid parameters must not panic the infallible constructor and must
    /// surface as an error from `finish` (matching the one-shot path). Covers
    /// both an out-of-range quality and an out-of-range `lgblock` whose
    /// `block_size()` shift would otherwise overflow.
    #[test]
    fn test_streaming_invalid_params_error_without_panic() {
        // Out-of-range quality.
        let sink = SharedSink::new();
        let bad_quality = BrotliParams {
            quality: 99,
            ..BrotliParams::default()
        };
        let compressor = BrotliCompressor::new(sink, bad_quality);
        assert!(
            compressor.finish().is_err(),
            "quality 99 must error at finish, not silently encode"
        );

        // Out-of-range lgblock: `1 << 64` would panic if `block_size()` were
        // consulted; construction and buffered write must stay panic-free.
        let sink = SharedSink::new();
        let bad_block = BrotliParams {
            lgblock: 64,
            ..BrotliParams::default()
        };
        let mut compressor = BrotliCompressor::new(sink, bad_block);
        let _ = compressor.write_all(b"data"); // buffers without touching encoder
        assert!(
            compressor.finish().is_err(),
            "lgblock 64 must error, not panic"
        );
    }

    /// Dropping after buffered writes (never flushed, never finished) must also
    /// produce a valid terminated stream (best-effort Drop).
    #[test]
    fn test_streaming_drop_with_buffered_data_terminates_stream() {
        let sink = SharedSink::new();
        let input = b"buffered but never flushed or finished";
        {
            let mut compressor = BrotliCompressor::new(sink.clone(), BrotliParams::default());
            compressor.write_all(input).expect("write");
        }
        roundtrip(&sink.snapshot(), input);
    }
}
