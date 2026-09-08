//! DEFLATE compression (RFC 1951).
//!
//! [`Deflater`] drives the zlib-equivalent encoder in this crate's private
//! `encoder` module:
//! a persistent 32 KiB window with hash chains, zlib's per-level
//! `configuration_table`, greedy matching at levels 1-3 and lazy matching
//! (with the `TOO_FAR` rule) at levels 4-9, blocks cut at 16 383 symbols, and
//! a per-block stored/fixed/dynamic choice made on real bit costs.
//!
//! All encoder state persists across calls, so a stream fed as many small
//! `deflate()` calls costs the same as one large call and produces the same
//! bytes — matches, Huffman trees and block boundaries never restart at a call
//! boundary.

use crate::encoder::{DeflateEncoder, Flush, Strategy};
use crate::lz77::{Lz77Params, Lz77Preset};
use crate::pool::{DeflatePool, PooledBuf, PooledU16Buf};
use oxiarc_core::error::Result;
use oxiarc_core::traits::{CompressStatus, Compressor, FlushMode};
use std::io::Write;

/// Extract the inner `Vec<u8>` from a [`PooledBuf`] without returning it to
/// the pool.  Ownership moves to the encoder; the buffer comes back via
/// [`Deflater::drop`].
fn extract_pooled_buf(guard: PooledBuf) -> Vec<u8> {
    let mut md = std::mem::ManuallyDrop::new(guard);
    std::mem::take(&mut md.buf)
}

/// Extract the inner `Vec<u16>` from a [`PooledU16Buf`]. Mirrors
/// [`extract_pooled_buf`].
fn extract_pooled_u16_buf(guard: PooledU16Buf) -> Vec<u16> {
    let mut md = std::mem::ManuallyDrop::new(guard);
    std::mem::take(&mut md.buf)
}

/// Maximum dictionary size for DEFLATE (32KB).
pub const MAX_DICTIONARY_SIZE: usize = 32768;

/// DEFLATE compressor.
///
/// Supports an optional [`DeflatePool`] to amortise buffer allocations across
/// many successive encode calls.  Enable via [`Deflater::with_pool`].
#[derive(Debug)]
pub struct Deflater {
    /// The zlib-equivalent encoder core.
    enc: DeflateEncoder,
    /// Dictionary Adler-32 checksum (if a dictionary is set).
    dictionary_checksum: Option<u32>,
    /// Compressed bytes awaiting delivery via the streaming [`Compressor`]
    /// trait (`compress`). Retained so a caller-supplied output buffer
    /// smaller than the produced bytes drains across calls instead of
    /// silently truncating the stream.
    out_pending: Vec<u8>,
    /// Cursor into `out_pending`: bytes before it have been delivered.
    out_pending_pos: usize,
    /// Optional pool used to recycle the window and hash buffers.
    pool: Option<DeflatePool>,
}

impl Drop for Deflater {
    fn drop(&mut self) {
        if let Some(pool) = self.pool.take() {
            let old = std::mem::replace(&mut self.enc, DeflateEncoder::new(0));
            let (window, hash_table, hash_chain) = old.into_buffers();
            pool.return_window(window);
            pool.return_hash_head(hash_table);
            pool.return_hash_prev(hash_chain);
        }
    }
}

impl Deflater {
    /// Create a new DEFLATE compressor with the specified level (0-9).
    ///
    /// # Example
    ///
    /// ```rust
    /// use oxiarc_deflate::{Deflater, inflate};
    ///
    /// let mut d = Deflater::new(6);
    /// let out = d.compress_to_vec(b"hello hello hello").expect("deflate");
    /// assert_eq!(inflate(&out).expect("inflate"), b"hello hello hello");
    /// ```
    pub fn new(level: u8) -> Self {
        Self {
            enc: DeflateEncoder::new(level),
            dictionary_checksum: None,
            out_pending: Vec::new(),
            out_pending_pos: 0,
            pool: None,
        }
    }

    /// Attach a memory pool to this compressor.
    ///
    /// The compressor's window and hash buffers are replaced by ones acquired
    /// from `pool`.  When the `Deflater` is dropped those buffers are returned
    /// to the pool for reuse by a future compressor.
    ///
    /// Any previously set dictionary is **not** preserved — call
    /// [`Deflater::set_dictionary`] after attaching the pool.
    ///
    /// # Example
    ///
    /// ```rust
    /// use oxiarc_deflate::{Deflater, pool::DeflatePool};
    ///
    /// let pool = DeflatePool::new();
    /// let mut d = Deflater::new(6).with_pool(&pool);
    /// let out = d.compress_to_vec(b"hello").expect("deflate");
    /// // buffers returned to pool when `d` is dropped
    /// drop(d);
    /// assert!(pool.stats().window_hits == 0); // first call always allocates
    /// ```
    #[must_use]
    pub fn with_pool(mut self, pool: &DeflatePool) -> Self {
        let window = extract_pooled_buf(pool.get_window());
        let hash_head = extract_pooled_u16_buf(pool.get_hash_head());
        let hash_prev = extract_pooled_u16_buf(pool.get_hash_prev());

        let level = self.enc.level();
        let optimal = self.enc.is_optimal();
        let strategy = self.enc.strategy();
        let cfg = self.enc.config();
        let mut enc = DeflateEncoder::with_buffers(level, window, hash_head, hash_prev);
        enc.set_optimal(optimal);
        enc.set_strategy(strategy);
        enc.set_config(cfg);
        self.enc = enc;
        self.dictionary_checksum = None;
        self.pool = Some(pool.clone());
        self
    }

    /// Create a new DEFLATE compressor with the dynamic-programming ("optimal")
    /// parser enabled.
    ///
    /// The optimal parser computes a shortest path through the LZ77 token graph
    /// of every span, fed with the **full** candidate set from the hash chains
    /// (not just the longest match per position) and with Huffman costs
    /// refined from the previous pass's histogram.  It also scores a lazy
    /// parse of the same candidates and keeps whichever is cheaper, so it can
    /// cost encode time but never compression ratio.
    ///
    /// It is opt-in because the default level ladder is byte-compatible with
    /// zlib; this mode deliberately leaves that behind in exchange for a
    /// smaller stream.
    ///
    /// Like the default ladder, it is **call-size invariant**: the parser waits
    /// for a whole span before running, and the wait threshold is a function of
    /// the window position alone, so any split of the same input produces the
    /// same bytes. It is roughly 4-8x slower than the ladder per byte, but the
    /// cost is proportional to the input and not to the number of calls
    /// (measured on 200 KB of log lines at level 9: 0.36 s in 1-byte calls
    /// against 0.33 s in one call).
    ///
    /// # Example
    ///
    /// ```rust
    /// use oxiarc_deflate::{Deflater, inflate};
    ///
    /// let data: Vec<u8> = (0..4096u32).map(|i| (i % 251) as u8).collect();
    /// let mut d = Deflater::with_optimal_parsing(9);
    /// let out = d.compress_to_vec(&data).expect("deflate");
    /// assert_eq!(inflate(&out).expect("inflate"), data);
    /// ```
    pub fn with_optimal_parsing(level: u8) -> Self {
        let mut this = Self::new(level);
        this.enc.set_optimal(true);
        this
    }

    /// Select the compression strategy (zlib's `Z_DEFAULT_STRATEGY`,
    /// `Z_FILTERED`, `Z_HUFFMAN_ONLY`, `Z_RLE`, `Z_FIXED`).
    ///
    /// [`Strategy::Filtered`] is the right choice for data produced by a
    /// predictor (PNG scanlines, TIFF horizontal differencing): it discards
    /// short matches so literals dominate.  [`Strategy::Rle`] restricts
    /// matching to distance 1, which is fast and ideal for run-heavy data.
    ///
    /// # Example
    ///
    /// ```rust
    /// use oxiarc_deflate::{Deflater, Strategy, inflate};
    ///
    /// let mut d = Deflater::new(6).with_strategy(Strategy::Rle);
    /// let out = d.compress_to_vec(&vec![7u8; 10_000]).expect("deflate");
    /// assert_eq!(inflate(&out).expect("inflate").len(), 10_000);
    /// assert!(out.len() < 100);
    /// ```
    #[must_use]
    pub fn with_strategy(mut self, strategy: Strategy) -> Self {
        self.enc.set_strategy(strategy);
        self
    }

    /// Override the per-level match-finding parameters.
    ///
    /// Replaces the level's row of zlib's `configuration_table`. The window,
    /// hash chains and any dictionary are preserved — only the tuning knobs
    /// change. Use [`Lz77Params::for_level`] to start from a known row.
    ///
    /// # Example
    ///
    /// ```rust
    /// use oxiarc_deflate::{Deflater, lz77::Lz77Params};
    ///
    /// // Level 6's search effort, but stop as soon as a 32-byte match appears.
    /// let params = Lz77Params { nice_length: 32, ..Lz77Params::for_level(6) };
    /// let mut deflater = Deflater::new(6).with_lz77_params(params);
    /// let out = deflater.compress_to_vec(b"aaaaaaaaaaaaaaaa").expect("deflate");
    /// assert!(!out.is_empty());
    /// ```
    #[must_use]
    pub fn with_lz77_params(mut self, params: Lz77Params) -> Self {
        self.enc.set_config(params.into());
        self
    }

    /// Override the per-level match-finding parameters using a named preset.
    ///
    /// # Example
    ///
    /// ```rust
    /// use oxiarc_deflate::{Deflater, lz77::Lz77Preset};
    ///
    /// let mut deflater = Deflater::new(6).with_lz77_preset(Lz77Preset::Best);
    /// let out = deflater.compress_to_vec(b"hello hello hello").expect("deflate");
    /// assert!(!out.is_empty());
    /// ```
    #[must_use]
    pub fn with_lz77_preset(mut self, preset: Lz77Preset) -> Self {
        self.enc.set_config(preset.params().into());
        self
    }

    /// The match-finding parameters currently in effect.
    pub fn lz77_params(&self) -> Lz77Params {
        let cfg = self.enc.config();
        Lz77Params {
            good_length: cfg.good_length,
            max_lazy: cfg.max_lazy,
            nice_length: cfg.nice_length,
            max_chain: cfg.max_chain,
        }
    }

    /// Create a new DEFLATE compressor with a preset dictionary.
    ///
    /// The dictionary seeds the sliding window, so matches can reference it
    /// from the very first byte.
    ///
    /// # Arguments
    ///
    /// * `level` - Compression level (0-9)
    /// * `dictionary` - Dictionary data (up to 32KB). If larger, only the
    ///   last 32KB is used.
    pub fn with_dictionary(level: u8, dictionary: &[u8]) -> Self {
        let mut deflater = Self::new(level);
        deflater.set_dictionary(dictionary);
        deflater
    }

    /// Set a preset dictionary for improved compression.
    ///
    /// Returns the Adler-32 checksum of the dictionary (the value a zlib
    /// header advertises via `FDICT`).
    pub fn set_dictionary(&mut self, dictionary: &[u8]) -> u32 {
        self.enc.set_dictionary(dictionary);
        let checksum = adler32(dictionary);
        self.dictionary_checksum = Some(checksum);
        self.out_pending = Vec::new();
        self.out_pending_pos = 0;
        checksum
    }

    /// Get the dictionary checksum, if a dictionary is set.
    pub fn dictionary_checksum(&self) -> Option<u32> {
        self.dictionary_checksum
    }

    /// Check if a dictionary is currently set.
    pub fn has_dictionary(&self) -> bool {
        self.dictionary_checksum.is_some()
    }

    /// Reset the compressor to its initial state.
    pub fn reset(&mut self) {
        self.enc.reset();
        self.dictionary_checksum = None;
        self.out_pending = Vec::new();
        self.out_pending_pos = 0;
    }

    /// Reset only the match-finder state (used by full flush).
    ///
    /// Note that [`FlushMode::Full`] already forgets the history as part of
    /// the flush; calling this in addition is not required.
    pub fn reset_lz77(&mut self) {
        self.enc.clear_history();
    }

    /// Reset the compressor but keep the dictionary checksum.
    pub fn reset_keep_dictionary(&mut self) {
        let checksum = self.dictionary_checksum;
        self.enc.reset();
        self.dictionary_checksum = checksum;
        self.out_pending = Vec::new();
        self.out_pending_pos = 0;
    }

    /// The configured compression level.
    pub fn level(&self) -> u8 {
        self.enc.level()
    }

    /// Compress data.
    ///
    /// With `finish == false` the encoder buffers whatever it cannot yet
    /// encode (DEFLATE needs 262 bytes of lookahead to decide a match) and
    /// keeps the trailing partial byte, so at levels 1..=9 repeated
    /// `deflate(_, false)` calls followed by a final `deflate(_, true)` produce
    /// one continuous DEFLATE stream that is byte-identical to compressing the
    /// concatenation in one call. Only `finish == true` (or an explicit flush)
    /// forces every fed byte out.
    ///
    /// **Level 0 is the exception** and byte-identity there is not promised:
    /// stored blocks are cut from whatever is buffered when a call arrives
    /// (zlib's `deflate_stored` behaves the same way, cutting on `avail_out`
    /// instead), so a stream fed in small calls can carry more 5-byte block
    /// headers than the same bytes fed in one. Measured: 65 537 bytes at
    /// level 0 cost 65 547 bytes in one call and 65 552 bytes in 4 KiB calls —
    /// the *decoded* bytes are of course identical either way, and no stored
    /// block ever exceeds the format's 65 535-byte maximum.
    ///
    /// # Example
    ///
    /// ```rust
    /// use oxiarc_deflate::{Deflater, inflate};
    ///
    /// let mut d = Deflater::new(6);
    /// let mut out = Vec::new();
    /// d.deflate(b"first half ", &mut out, false).expect("deflate");
    /// d.deflate(b"second half", &mut out, true).expect("deflate");
    /// assert_eq!(inflate(&out).expect("inflate"), b"first half second half");
    /// ```
    pub fn deflate<W: Write>(&mut self, data: &[u8], writer: &mut W, finish: bool) -> Result<()> {
        let flush = if finish { Flush::Finish } else { Flush::None };
        self.run(data, writer, flush)
    }

    /// Compress data with a sync flush: everything fed so far is emitted and
    /// an empty stored block (`00 00 FF FF`) is appended, so a decoder can
    /// produce all bytes so far and resynchronise.
    pub fn deflate_sync<W: Write>(&mut self, data: &[u8], writer: &mut W) -> Result<()> {
        self.run(data, writer, Flush::Sync)
    }

    /// Compress data with a partial flush: everything fed so far is emitted
    /// and an empty *fixed-Huffman* block is appended (zlib's
    /// `Z_PARTIAL_FLUSH`).  Unlike a sync flush this leaves no `00 00 FF FF`
    /// resynchronisation marker and does not byte-align the stream.
    pub fn deflate_partial<W: Write>(&mut self, data: &[u8], writer: &mut W) -> Result<()> {
        self.run(data, writer, Flush::Partial)
    }

    /// Compress data with a full flush: like a sync flush, and the match
    /// history is forgotten so the next block can be decoded independently.
    pub fn deflate_full<W: Write>(&mut self, data: &[u8], writer: &mut W) -> Result<()> {
        self.run(data, writer, Flush::Full)
    }

    fn run<W: Write>(&mut self, data: &[u8], writer: &mut W, flush: Flush) -> Result<()> {
        self.enc.push(data, flush);
        writer.write_all(self.enc.output())?;
        self.enc.clear_output();
        Ok(())
    }

    /// Compress data to a Vec, terminating the stream.
    pub fn compress_to_vec(&mut self, data: &[u8]) -> Result<Vec<u8>> {
        let mut output = Vec::new();
        self.deflate(data, &mut output, true)?;
        Ok(output)
    }
}

/// Adler-32 checksum (used to identify a preset dictionary).
fn adler32(data: &[u8]) -> u32 {
    const MOD_ADLER: u32 = 65521;
    const NMAX: usize = 5552;

    let mut a: u32 = 1;
    let mut b: u32 = 0;
    let mut remaining = data;

    while remaining.len() >= NMAX {
        let (chunk, rest) = remaining.split_at(NMAX);
        remaining = rest;
        for &byte in chunk {
            a += u32::from(byte);
            b += a;
        }
        a %= MOD_ADLER;
        b %= MOD_ADLER;
    }
    for &byte in remaining {
        a += u32::from(byte);
        b += a;
    }
    ((b % MOD_ADLER) << 16) | (a % MOD_ADLER)
}

impl Default for Deflater {
    fn default() -> Self {
        Self::new(6)
    }
}

impl Compressor for Deflater {
    /// Streaming compression: produced bytes that do not fit in `output`
    /// are buffered internally and drained across subsequent calls.
    ///
    /// Returns [`CompressStatus::NeedsOutput`] while produced bytes remain
    /// undelivered (with `consumed == 0` on pure drain calls, so the caller
    /// re-offers unconsumed input) and [`CompressStatus::Done`] only after
    /// a `Finish` stream has been fully delivered — a bounded output buffer
    /// never receives a silently truncated stream.
    fn compress(
        &mut self,
        input: &[u8],
        output: &mut [u8],
        flush: FlushMode,
    ) -> Result<(usize, usize, CompressStatus)> {
        // Drain compressed bytes buffered by a previous call first.
        if self.out_pending_pos < self.out_pending.len() {
            let remaining = self.out_pending.len() - self.out_pending_pos;
            let to_copy = remaining.min(output.len());
            output[..to_copy].copy_from_slice(
                &self.out_pending[self.out_pending_pos..self.out_pending_pos + to_copy],
            );
            self.out_pending_pos += to_copy;

            let status = if self.out_pending_pos < self.out_pending.len() {
                CompressStatus::NeedsOutput
            } else {
                self.out_pending = Vec::new();
                self.out_pending_pos = 0;
                if self.enc.is_finished() {
                    CompressStatus::Done
                } else {
                    CompressStatus::NeedsInput
                }
            };
            return Ok((0, to_copy, status));
        }

        if self.enc.is_finished() {
            return Ok((0, 0, CompressStatus::Done));
        }

        let mode = match flush {
            FlushMode::Finish => Flush::Finish,
            FlushMode::Sync => Flush::Sync,
            FlushMode::Full => Flush::Full,
            FlushMode::Partial => Flush::Partial,
            // `FlushMode` is `#[non_exhaustive]`; treat any future mode as the
            // conservative buffered (no-flush) path rather than panicking.
            _ => Flush::None,
        };
        self.enc.push(input, mode);
        let buffer = self.enc.take_output();

        let finish = matches!(flush, FlushMode::Finish);
        let to_copy = buffer.len().min(output.len());
        output[..to_copy].copy_from_slice(&buffer[..to_copy]);

        let status = if to_copy < buffer.len() {
            self.out_pending = buffer;
            self.out_pending_pos = to_copy;
            CompressStatus::NeedsOutput
        } else if finish {
            CompressStatus::Done
        } else {
            CompressStatus::NeedsInput
        };

        Ok((input.len(), to_copy, status))
    }

    fn reset(&mut self) {
        Deflater::reset(self);
    }

    fn is_finished(&self) -> bool {
        self.enc.is_finished()
    }
}

/// Compress data using DEFLATE.
///
/// # Example
///
/// ```rust
/// use oxiarc_deflate::{deflate, inflate};
///
/// let compressed = deflate(b"data data data", 6).expect("deflate");
/// assert_eq!(inflate(&compressed).expect("inflate"), b"data data data");
/// ```
pub fn deflate(data: &[u8], level: u8) -> Result<Vec<u8>> {
    let mut deflater = Deflater::new(level);
    deflater.compress_to_vec(data)
}
