//! Bit-level I/O operations for compression algorithms.
//!
//! This module provides `BitReader` and `BitWriter` for reading and writing
//! data at the bit level, which is essential for variable-length codes used
//! in Huffman coding and other compression algorithms.
//!
//! # Bit Ordering
//!
//! DEFLATE uses LSB-first (Least Significant Bit first) ordering within bytes:
//! bits are packed starting from the least significant bit of each byte. The
//! [`BitReader`] and [`BitWriter`] in this module implement that LSB-first
//! convention.
//!
//! Note that canonical LZH/LHA (the classic `lha` / `LHarc` family, e.g. the
//! `-lh5-` method) instead uses MSB-first ordering, where bits fill from the
//! most significant bit downward. For that convention see the sibling
//! [`crate::msb_bitstream`] module (`MsbBitReader` / `MsbBitWriter`). These
//! LSB-first types remain correct for DEFLATE and for OxiArc's current
//! (not-yet-spec-compliant) LZH format.
//!
//! # Example
//!
//! ```
//! use oxiarc_core::bitstream::{BitReader, BitWriter};
//! use std::io::Cursor;
//!
//! // Writing bits
//! let mut output = Vec::new();
//! {
//!     let mut writer = BitWriter::new(&mut output);
//!     writer.write_bits(0b101, 3).expect("write 3 bits");  // Write 3 bits
//!     writer.write_bits(0b1100, 4).expect("write 4 bits"); // Write 4 bits
//!     writer.flush().expect("flush writer");
//! }
//!
//! // Reading bits
//! let mut reader = BitReader::new(Cursor::new(&output));
//! assert_eq!(reader.read_bits(3).expect("read 3 bits"), 0b101);
//! assert_eq!(reader.read_bits(4).expect("read 4 bits"), 0b1100);
//! ```

use crate::error::{OxiArcError, Result};
use std::io::{Read, Write};

/// Default size of the internal prefetch buffer used by
/// [`BitReader::buffered`], in bytes.
///
/// Sized to stay comfortably inside L1 while amortising one
/// [`Read::read`] call over thousands of decoded symbols.
pub const DEFAULT_PREFETCH_CAPACITY: usize = 8192;

/// Smallest usable prefetch buffer.
///
/// The bulk refill path loads 8 bytes at a time, so a buffer below that
/// would degenerate to the byte-at-a-time path on every refill.
const MIN_PREFETCH_CAPACITY: usize = 64;

/// Load 8 bytes into a bit accumulator, consuming only whole bytes.
///
/// Returns the number of bytes consumed (`1..=7`). The caller guarantees
/// `*bits <= 55`, which makes the byte count non-zero (so refill loops
/// cannot spin) and keeps the mask shift in range.
///
/// The mask keeps the invariant "every bit at index >= `*bits` is zero".
/// Without it those high bits would hold a copy of the *next* unconsumed
/// bytes, which is only harmless while the byte source stays contiguous —
/// it does not across a buffer refill. This is the safe `from_le_bytes`
/// equivalent of the `get_unaligned_le64` refill zlib/libdeflate use.
#[inline(always)]
fn bulk_load(chunk: &[u8; 8], buffer: &mut u64, bits: &mut u8) -> usize {
    debug_assert!(*bits <= 55);
    let word = u64::from_le_bytes(*chunk);
    let nbytes = ((63 - *bits) >> 3) as usize;
    let nbits = (nbytes as u8) << 3;
    let masked = word & (u64::MAX >> (64 - nbits));
    *buffer |= masked << *bits;
    *bits += nbits;
    nbytes
}

/// Bit accumulator detached from a [`BitReader`].
///
/// A table-driven decoder's inner loop is dominated by the dependency chain
/// `load accumulator -> table lookup -> shift -> store accumulator`. Keeping
/// the accumulator in a local `BitCache` lets the compiler hold it in
/// registers and removes the store-to-load forwarding stall from that chain
/// — the same reason zlib's `inflate_fast` copies `hold`/`bits` into locals.
///
/// # Contract
///
/// While a cache is detached the owning `BitReader` holds **no** bits: every
/// other method on it (`read_bits`, `peek_bits`, `read_bytes`, ...) would
/// start from an empty accumulator and mis-parse the stream. Use only
/// [`BitReader::refill_cache`] until [`BitReader::reattach`] puts the bits
/// back — `reattach` also folds the cache's consumed-bit count into
/// [`BitReader::bits_read`]. Mis-use cannot cause undefined behaviour, only
/// a decode error.
#[derive(Clone, Copy, Debug, Default)]
pub struct BitCache {
    /// Bits, LSB-first. All bits at index >= `len` are zero.
    buffer: u64,
    /// Number of valid bits in `buffer`.
    len: u8,
    /// Bits consumed since the cache was detached.
    consumed: u64,
}

impl BitCache {
    /// Number of bits currently held.
    #[inline(always)]
    pub fn available(&self) -> u8 {
        self.len
    }

    /// Bits consumed since this cache was detached.
    #[inline(always)]
    pub fn consumed(&self) -> u64 {
        self.consumed
    }

    /// Extract the low bits selected by `mask` without consuming them.
    ///
    /// Bits beyond [`BitCache::available`] read as zero, so the caller must
    /// validate that the code it decoded is no longer than the number of
    /// bits actually available.
    #[inline(always)]
    pub fn peek_mask(&self, mask: u32) -> u32 {
        (self.buffer as u32) & mask
    }

    /// Peek at up to 32 bits without consuming them (zero padded past the
    /// end, as for [`BitCache::peek_mask`]).
    #[inline(always)]
    pub fn peek_bits(&self, count: u8) -> u32 {
        debug_assert!(count <= 32);
        let mask = (1u64 << count).wrapping_sub(1);
        (self.buffer & mask) as u32
    }

    /// Consume `count` bits. Consuming more than are available empties the
    /// cache instead of corrupting the bit position.
    #[inline(always)]
    pub fn consume(&mut self, count: u8) {
        let n = count.min(self.len);
        if n >= 64 {
            self.buffer = 0;
        } else {
            self.buffer >>= n;
        }
        self.len -= n;
        self.consumed += n as u64;
    }

    /// Load whole bytes from `src` with one unaligned 64-bit little-endian
    /// read, returning the number of bytes taken (`0` or `1..=7`).
    ///
    /// This is the accumulator-only form of the bulk refill a
    /// [`BitReader`] performs against its prefetch buffer: it lets a decoder
    /// that already owns the compressed bytes as a slice run the same single
    /// `u64::from_le_bytes` per refill without a `Read` in the loop.
    ///
    /// Returns `0` (a no-op) when fewer than 8 bytes are available — the
    /// load reads 8 bytes even though it keeps at most 7 — or when the cache
    /// already holds more than 55 bits. Callers must therefore fall back to
    /// [`BitCache::refill_bytes`] for the tail of a stream.
    ///
    /// # Examples
    ///
    /// ```
    /// use oxiarc_core::BitCache;
    ///
    /// let mut cache = BitCache::default();
    /// let taken = cache.refill_bulk(&[0xFF; 8]);
    /// assert_eq!(taken, 7);
    /// assert_eq!(cache.available(), 56);
    /// // Fewer than 8 bytes: nothing is loaded.
    /// assert_eq!(BitCache::default().refill_bulk(&[0xFF; 7]), 0);
    /// ```
    #[inline(always)]
    pub fn refill_bulk(&mut self, src: &[u8]) -> usize {
        if self.len > 55 {
            return 0;
        }
        match src.first_chunk::<8>() {
            Some(chunk) => bulk_load(chunk, &mut self.buffer, &mut self.len),
            None => 0,
        }
    }

    /// Top up the cache one byte at a time until it holds at least `want`
    /// bits or `src` is exhausted, returning the number of bytes taken.
    ///
    /// `want` must be at most 56, which keeps the accumulator within its
    /// 63-bit ceiling. Running out of input is not an error: the caller
    /// detects it through [`BitCache::available`].
    ///
    /// # Examples
    ///
    /// ```
    /// use oxiarc_core::BitCache;
    ///
    /// let mut cache = BitCache::default();
    /// assert_eq!(cache.refill_bytes(&[0x01, 0x02, 0x03], 16), 2);
    /// assert_eq!(cache.available(), 16);
    /// assert_eq!(cache.peek_bits(8), 0x01);
    /// ```
    #[inline]
    pub fn refill_bytes(&mut self, src: &[u8], want: u8) -> usize {
        debug_assert!(want <= 56);
        let mut used = 0usize;
        while self.len < want && self.len <= 55 {
            let Some(&byte) = src.get(used) else {
                break;
            };
            self.buffer |= (byte as u64) << self.len;
            self.len += 8;
            used += 1;
        }
        used
    }

    /// Discard the sub-byte remainder so the cache holds whole bytes only.
    ///
    /// Returns the number of bits discarded, which are counted as consumed
    /// (the analogue of [`BitReader::align_to_byte`]).
    ///
    /// # Examples
    ///
    /// ```
    /// use oxiarc_core::BitCache;
    ///
    /// let mut cache = BitCache::default();
    /// cache.refill_bytes(&[0xAB, 0xCD], 16);
    /// cache.consume(3);
    /// assert_eq!(cache.align_to_byte(), 5);
    /// assert_eq!(cache.available(), 8);
    /// ```
    #[inline]
    pub fn align_to_byte(&mut self) -> u8 {
        let remainder = self.len % 8;
        if remainder > 0 {
            self.consume(remainder);
        }
        remainder
    }

    /// Pop one whole byte, LSB-first, when at least 8 bits are held.
    ///
    /// The cache must already be byte-aligned (see
    /// [`BitCache::align_to_byte`]); otherwise the byte returned would
    /// straddle two stream bytes.
    ///
    /// # Examples
    ///
    /// ```
    /// use oxiarc_core::BitCache;
    ///
    /// let mut cache = BitCache::default();
    /// cache.refill_bytes(&[0xAB, 0xCD], 16);
    /// assert_eq!(cache.take_byte(), Some(0xAB));
    /// assert_eq!(cache.take_byte(), Some(0xCD));
    /// assert_eq!(cache.take_byte(), None);
    /// ```
    #[inline]
    pub fn take_byte(&mut self) -> Option<u8> {
        if self.len < 8 {
            return None;
        }
        let byte = (self.buffer & 0xFF) as u8;
        self.consume(8);
        Some(byte)
    }
}

/// A bit-level reader that wraps any `Read` implementation.
///
/// `BitReader` maintains a 64-bit accumulator so that bits can be read
/// across byte boundaries without touching the underlying reader for every
/// symbol.
///
/// # Prefetching modes
///
/// The accumulator can be refilled in two ways, chosen at construction:
///
/// * **Exact mode** ([`BitReader::new`]) — the reader is asked for exactly
///   as many bytes as the requested bit count needs (at most 7 at a time).
///   No byte is ever consumed from the underlying reader beyond the bits
///   actually requested, so the reader can be used directly afterwards.
///   This is required when a `BitReader` is layered over a shared stream
///   that continues to be read by other code once the bit-level section
///   ends (for example the ZIP streaming reader, which parses a
///   byte-aligned data descriptor after the DEFLATE payload).
///
/// * **Buffered mode** ([`BitReader::buffered`] /
///   [`BitReader::with_buffer_capacity`]) — bytes are pulled from the
///   underlying reader in large chunks into an internal buffer, and the
///   accumulator is refilled with single unaligned 64-bit little-endian
///   loads from that buffer. This is dramatically faster (it is what makes
///   the DEFLATE decoder competitive with C zlib) but the reader may be
///   advanced past the end of the bit stream by up to the buffer capacity.
///   All `BitReader` methods drain the internal buffer first, so no data is
///   lost as long as every subsequent read goes through the same
///   `BitReader`; [`BitReader::into_parts`] hands the un-consumed prefetch
///   back when the underlying reader must be used directly again.
#[derive(Debug)]
pub struct BitReader<R: Read> {
    /// Underlying reader.
    reader: R,
    /// Bit buffer (LSB-first).
    ///
    /// Invariant: all bits at index >= `bits_in_buffer` are zero.
    buffer: u64,
    /// Number of valid bits in buffer.
    bits_in_buffer: u8,
    /// Total bits read (for error reporting).
    total_bits_read: u64,
    /// Prefetch buffer. Always exactly `prefetch` bytes long; empty in
    /// exact mode.
    buf: Vec<u8>,
    /// Read cursor into `buf`.
    buf_pos: usize,
    /// Number of valid bytes in `buf` (`buf_pos <= buf_len <= buf.len()`).
    buf_len: usize,
    /// Prefetch capacity; `0` selects exact mode.
    prefetch: usize,
    /// Set once the underlying reader has signalled end of stream.
    src_eof: bool,
}

impl<R: Read> BitReader<R> {
    /// Create a new `BitReader` wrapping the given reader in **exact mode**.
    ///
    /// The underlying reader is never advanced past the bits actually
    /// consumed (beyond the current partial byte). Prefer
    /// [`BitReader::buffered`] when this `BitReader` owns the stream — it is
    /// several times faster.
    pub fn new(reader: R) -> Self {
        Self {
            reader,
            buffer: 0,
            bits_in_buffer: 0,
            total_bits_read: 0,
            buf: Vec::new(),
            buf_pos: 0,
            buf_len: 0,
            prefetch: 0,
            src_eof: false,
        }
    }

    /// Create a new `BitReader` in **buffered mode** with the default
    /// prefetch capacity ([`DEFAULT_PREFETCH_CAPACITY`]).
    ///
    /// Use this whenever the `BitReader` owns the byte stream (or every
    /// subsequent read goes through the same `BitReader`). It is the fast
    /// path: bytes are pulled in bulk and the accumulator is refilled with
    /// 64-bit loads instead of per-symbol [`Read::read`] calls.
    pub fn buffered(reader: R) -> Self {
        Self::with_buffer_capacity(reader, DEFAULT_PREFETCH_CAPACITY)
    }

    /// Create a new `BitReader` in **buffered mode** with an explicit
    /// prefetch capacity (clamped to a sane minimum).
    ///
    /// See [`BitReader::buffered`] for the trade-off; `capacity` bounds how
    /// far past the end of the bit stream the underlying reader may be
    /// advanced.
    pub fn with_buffer_capacity(reader: R, capacity: usize) -> Self {
        let prefetch = capacity.max(MIN_PREFETCH_CAPACITY);
        Self {
            reader,
            buffer: 0,
            bits_in_buffer: 0,
            total_bits_read: 0,
            buf: vec![0u8; prefetch],
            buf_pos: 0,
            buf_len: 0,
            prefetch,
            src_eof: false,
        }
    }

    /// Get a reference to the underlying reader.
    pub fn get_ref(&self) -> &R {
        &self.reader
    }

    /// Get a mutable reference to the underlying reader.
    ///
    /// In buffered mode the reader may already have been advanced past the
    /// bits consumed so far; use [`BitReader::into_parts`] to recover the
    /// prefetched-but-unconsumed bytes.
    pub fn get_mut(&mut self) -> &mut R {
        &mut self.reader
    }

    /// Consume this `BitReader` and return the underlying reader.
    ///
    /// In buffered mode any prefetched-but-unconsumed bytes are discarded;
    /// use [`BitReader::into_parts`] to get them back.
    pub fn into_inner(self) -> R {
        self.reader
    }

    /// Consume this `BitReader` and return the underlying reader together
    /// with the bytes that were prefetched but not consumed.
    ///
    /// The returned bytes logically precede anything the reader will yield
    /// next, so a caller that wants to keep reading the stream directly must
    /// process them first. Any partial (sub-byte) remainder in the bit
    /// accumulator is discarded, matching [`BitReader::align_to_byte`].
    pub fn into_parts(mut self) -> (R, Vec<u8>) {
        self.align_to_byte();
        let mut pending = Vec::with_capacity(self.buffered_len());
        while self.bits_in_buffer >= 8 {
            pending.push((self.buffer & 0xFF) as u8);
            self.buffer >>= 8;
            self.bits_in_buffer -= 8;
        }
        if let Some(rest) = self.buf.get(self.buf_pos..self.buf_len) {
            pending.extend_from_slice(rest);
        }
        (self.reader, pending)
    }

    /// Number of whole bytes held inside this `BitReader` that have been
    /// taken from the underlying reader but not yet consumed as bits.
    pub fn buffered_len(&self) -> usize {
        (self.bits_in_buffer / 8) as usize + (self.buf_len - self.buf_pos)
    }

    /// Get the total number of bits read so far.
    pub fn bits_read(&self) -> u64 {
        self.total_bits_read
    }

    /// Get the current bit position (for error reporting).
    pub fn bit_position(&self) -> u64 {
        self.total_bits_read
    }

    /// Number of bits currently held in the accumulator.
    ///
    /// Together with [`BitReader::refill`], [`BitReader::peek_bits_prefilled`]
    /// and [`BitReader::consume_bits`] this allows a decoder's inner loop to
    /// refill once and then extract several symbols with no further checks.
    #[inline(always)]
    pub fn available_bits(&self) -> u8 {
        self.bits_in_buffer
    }

    /// Best-effort refill of the accumulator to at least 56 bits.
    ///
    /// Never fails at end of stream — it simply leaves fewer bits available
    /// (check with [`BitReader::available_bits`]). Only a genuine I/O error
    /// from the underlying reader is reported. In exact mode this is a no-op,
    /// because filling speculatively would consume bytes the caller has not
    /// asked for.
    #[inline(always)]
    pub fn refill(&mut self) -> Result<()> {
        if self.bits_in_buffer > 55 || self.prefetch == 0 {
            return Ok(());
        }
        // Common case: at least 8 bytes buffered — one unaligned 64-bit load.
        if self.buf_len - self.buf_pos >= 8 {
            self.refill_bulk();
            return Ok(());
        }
        self.refill_to(56)?;
        Ok(())
    }

    /// Try to make at least `count` bits (`count <= 57`) available, reporting
    /// end of stream as `Ok(false)` instead of an error.
    ///
    /// Unlike [`BitReader::refill`] this also works in exact mode, where it
    /// reads exactly the bytes the requested bit count needs. It is the
    /// primitive a table-driven decoder wants: top up the accumulator, then
    /// validate the decoded code length against
    /// [`BitReader::available_bits`].
    #[inline]
    pub fn try_fill(&mut self, count: u8) -> Result<bool> {
        debug_assert!(count <= 57, "Cannot fill more than 57 bits at once");
        if self.bits_in_buffer >= count {
            return Ok(true);
        }
        if self.prefetch != 0 {
            return self.refill_to(count);
        }
        match self.fill_buffer_exact(count) {
            Ok(()) => Ok(true),
            Err(OxiArcError::UnexpectedEof { .. }) => Ok(false),
            Err(OxiArcError::Io(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Peek at up to 32 bits **without** refilling.
    ///
    /// Bits beyond [`BitReader::available_bits`] read as zero, so a caller
    /// must validate that the code it decoded is no longer than the number of
    /// bits that were actually available.
    #[inline(always)]
    pub fn peek_bits_prefilled(&self, count: u8) -> u32 {
        debug_assert!(count <= 32, "Cannot peek more than 32 bits at once");
        let mask = (1u64 << count).wrapping_sub(1);
        (self.buffer & mask) as u32
    }

    /// Consume `count` bits that are already present in the accumulator.
    ///
    /// If fewer than `count` bits are available the accumulator is simply
    /// emptied; callers are expected to check [`BitReader::available_bits`]
    /// first (this path exists so that a mis-use degrades into a decode error
    /// rather than corrupting the bit position).
    #[inline(always)]
    pub fn consume_bits(&mut self, count: u8) {
        let n = count.min(self.bits_in_buffer);
        if n >= 64 {
            self.buffer = 0;
        } else {
            self.buffer >>= n;
        }
        self.bits_in_buffer -= n;
        self.total_bits_read += n as u64;
    }

    /// Load 8 bytes from the prefetch buffer into the accumulator.
    ///
    /// The caller must have checked that at least 8 bytes are buffered and
    /// that `bits_in_buffer <= 55`.
    #[inline(always)]
    fn refill_bulk(&mut self) {
        let Some(chunk) = self
            .buf
            .get(self.buf_pos..self.buf_len)
            .and_then(<[u8]>::first_chunk::<8>)
        else {
            return;
        };
        let nbytes = bulk_load(chunk, &mut self.buffer, &mut self.bits_in_buffer);
        self.buf_pos += nbytes;
    }

    /// Detach the bit accumulator for register-resident decoding.
    ///
    /// See [`BitCache`] for the contract; the returned cache must be handed
    /// back with [`BitReader::reattach`].
    #[inline(always)]
    pub fn detach(&mut self) -> BitCache {
        let cache = BitCache {
            buffer: self.buffer,
            len: self.bits_in_buffer,
            consumed: 0,
        };
        self.buffer = 0;
        self.bits_in_buffer = 0;
        cache
    }

    /// Put a detached [`BitCache`] back, folding its consumed-bit count into
    /// [`BitReader::bits_read`].
    #[inline(always)]
    pub fn reattach(&mut self, cache: BitCache) {
        self.buffer = cache.buffer;
        self.bits_in_buffer = cache.len;
        self.total_bits_read += cache.consumed;
    }

    /// Top up a detached [`BitCache`] to at least `want` bits (`want <= 57`).
    ///
    /// End of stream is not an error: the cache is simply left holding fewer
    /// bits, which the caller detects via [`BitCache::available`].
    #[inline(always)]
    pub fn refill_cache(&mut self, cache: &mut BitCache, want: u8) -> Result<()> {
        debug_assert!(want <= 57);
        if cache.len >= want {
            return Ok(());
        }
        if self.prefetch != 0 && cache.len <= 55 && self.buf_len - self.buf_pos >= 8 {
            // The 8 bytes at `buf_pos` are known valid (the check above), so
            // slicing from `buf_pos` alone avoids a second range check.
            if let Some(chunk) = self
                .buf
                .get(self.buf_pos..)
                .and_then(<[u8]>::first_chunk::<8>)
            {
                self.buf_pos += bulk_load(chunk, &mut cache.buffer, &mut cache.len);
                if cache.len >= want {
                    return Ok(());
                }
            }
        }
        self.refill_cache_slow(cache, want)
    }

    /// Cache refill for everything the inline fast path does not cover:
    /// fewer than 8 bytes prefetched, and exact mode (where reading ahead is
    /// forbidden). Routing through the ordinary machinery keeps a single
    /// implementation of the end-of-stream and short-read rules.
    ///
    /// Inlined on purpose despite being the cold path: it takes the cache by
    /// reference, so leaving it out of line would make the cache's address
    /// escape and force the accumulator back into memory for the whole
    /// decode loop — exactly what [`BitCache`] exists to avoid. The bulk of
    /// the work still lives in the out-of-line `try_fill`/`refill_to`.
    #[inline]
    fn refill_cache_slow(&mut self, cache: &mut BitCache, want: u8) -> Result<()> {
        self.reattach(*cache);
        let r = self.try_fill(want);
        *cache = self.detach();
        r.map(|_| ())
    }

    /// Load a single byte from the prefetch buffer into the accumulator.
    ///
    /// Returns `false` when the prefetch buffer is empty.
    #[inline]
    fn refill_byte(&mut self) -> bool {
        debug_assert!(self.bits_in_buffer <= 56);
        if self.buf_pos >= self.buf_len {
            return false;
        }
        let Some(&byte) = self.buf.get(self.buf_pos) else {
            return false;
        };
        self.buffer |= (byte as u64) << self.bits_in_buffer;
        self.bits_in_buffer += 8;
        self.buf_pos += 1;
        true
    }

    /// Pull more bytes from the underlying reader into the prefetch buffer.
    ///
    /// Returns `false` once the underlying reader has signalled end of
    /// stream. Short reads are retried by the caller (they are normal for
    /// pipes, sockets and throttled adapters) — only `Ok(0)` means EOF.
    fn pull(&mut self) -> Result<bool> {
        if self.src_eof {
            return Ok(false);
        }
        // Compact: move the unconsumed tail to the front.
        if self.buf_pos > 0 {
            let keep = self.buf_len - self.buf_pos;
            if keep > 0 {
                self.buf.copy_within(self.buf_pos..self.buf_len, 0);
            }
            self.buf_len = keep;
            self.buf_pos = 0;
        }
        let Some(dst) = self.buf.get_mut(self.buf_len..) else {
            return Ok(false);
        };
        if dst.is_empty() {
            return Ok(true);
        }
        match self.reader.read(dst) {
            Ok(0) => {
                self.src_eof = true;
                Ok(false)
            }
            Ok(n) => {
                self.buf_len += n;
                Ok(true)
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Fill the accumulator until it holds at least `want` bits (`want <= 57`).
    ///
    /// Returns `Ok(false)` when the stream ended first; the accumulator then
    /// holds every remaining bit.
    fn refill_to(&mut self, want: u8) -> Result<bool> {
        while self.bits_in_buffer < want {
            if self.bits_in_buffer <= 55 && self.buf_len - self.buf_pos >= 8 {
                self.refill_bulk();
                continue;
            }
            if self.bits_in_buffer <= 56 && self.refill_byte() {
                continue;
            }
            if !self.pull()? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Exact-mode fill: read only the bytes the requested bit count needs.
    ///
    /// A short read (fewer bytes than requested — common with TCP/pipe/stdin/
    /// throttled/adapter readers) is retried rather than treated as end of
    /// file; only a genuine `Ok(0)` before enough bits are available is a real
    /// EOF. Reading in a loop preserves the LSB-first packing order exactly,
    /// because each byte is still shifted into `buffer` at the current
    /// `bits_in_buffer`.
    #[inline]
    fn fill_buffer_exact(&mut self, count: u8) -> Result<()> {
        let mut temp_buf = [0u8; 8];
        while self.bits_in_buffer < count {
            // Bytes still required, capped at 7 so the 64-bit buffer never
            // overflows (`count <= 57`, so packing stops at <= 64 bits).
            let bits_needed = count - self.bits_in_buffer;
            let bytes_needed = bits_needed.div_ceil(8).min(7) as usize;

            match self.reader.read(&mut temp_buf[..bytes_needed]) {
                Ok(0) => {
                    return Err(OxiArcError::unexpected_eof(bytes_needed));
                }
                Ok(n) => {
                    // Pack bytes into buffer (LSB-first).
                    for byte in temp_buf.iter().take(n) {
                        self.buffer |= (*byte as u64) << self.bits_in_buffer;
                        self.bits_in_buffer += 8;
                    }
                }
                Err(e) => return Err(e.into()),
            }
        }

        Ok(())
    }

    /// Ensure at least `count` bits are available in the buffer.
    ///
    /// Kept fully inlineable: `OxiArcError` is 48 bytes, so an out-of-line
    /// `Result<()>` return here would cost an indirect (sret) store on every
    /// refill — measurably more than the refill itself.
    #[inline]
    fn fill_buffer(&mut self, count: u8) -> Result<()> {
        debug_assert!(count <= 57, "Cannot fill more than 57 bits at once");

        if self.bits_in_buffer >= count {
            return Ok(());
        }

        if self.prefetch != 0 {
            // Buffered mode: refill from the internal byte buffer, pulling
            // more bytes from the underlying reader only when it runs dry.
            loop {
                if self.bits_in_buffer <= 55 && self.buf_len - self.buf_pos >= 8 {
                    self.refill_bulk();
                } else if self.bits_in_buffer <= 56 && self.refill_byte() {
                    // One byte added.
                } else if !self.pull()? {
                    let short = count.saturating_sub(self.bits_in_buffer);
                    return Err(OxiArcError::unexpected_eof(short.div_ceil(8) as usize));
                }
                if self.bits_in_buffer >= count {
                    return Ok(());
                }
            }
        }

        self.fill_buffer_exact(count)
    }

    /// Read up to 32 bits from the stream.
    ///
    /// # Arguments
    ///
    /// * `count` - Number of bits to read (0-32)
    ///
    /// # Returns
    ///
    /// The bits read as a u32, with the first bit read in the LSB position.
    #[inline]
    pub fn read_bits(&mut self, count: u8) -> Result<u32> {
        debug_assert!(count <= 32, "Cannot read more than 32 bits at once");

        if count == 0 {
            return Ok(0);
        }

        self.fill_buffer(count)?;

        // Extract bits from buffer (optimized with wrapping_sub to avoid overflow check)
        let mask = (1u64 << count).wrapping_sub(1);
        let result = (self.buffer & mask) as u32;

        // Remove read bits from buffer
        self.buffer >>= count;
        self.bits_in_buffer -= count;
        self.total_bits_read += count as u64;

        Ok(result)
    }

    /// Peek at up to 32 bits without consuming them.
    ///
    /// # Arguments
    ///
    /// * `count` - Number of bits to peek (0-32)
    ///
    /// # Returns
    ///
    /// The bits as a u32, without advancing the read position.
    #[inline]
    pub fn peek_bits(&mut self, count: u8) -> Result<u32> {
        debug_assert!(count <= 32, "Cannot peek more than 32 bits at once");

        if count == 0 {
            return Ok(0);
        }

        self.fill_buffer(count)?;

        let mask = (1u64 << count) - 1;
        Ok((self.buffer & mask) as u32)
    }

    /// Skip a number of bits.
    ///
    /// # Arguments
    ///
    /// * `count` - Number of bits to skip
    #[inline]
    pub fn skip_bits(&mut self, count: u8) -> Result<()> {
        if count == 0 {
            return Ok(());
        }

        // Counts above 32 are split so that neither `fill_buffer` (which can
        // hold at most 57 bits) nor the shift below can overflow. Callers in
        // this workspace never exceed 15, but `skip_bits` is public API and a
        // count of 64 or more must not corrupt the bit position.
        if count > 32 {
            let mut remaining = count;
            while remaining > 0 {
                let step = remaining.min(32);
                self.read_bits(step)?;
                remaining -= step;
            }
            return Ok(());
        }

        self.fill_buffer(count)?;

        self.buffer >>= count;
        self.bits_in_buffer -= count;
        self.total_bits_read += count as u64;

        Ok(())
    }

    /// Read a single bit.
    pub fn read_bit(&mut self) -> Result<bool> {
        Ok(self.read_bits(1)? != 0)
    }

    /// Read an aligned byte, discarding any partial bits.
    ///
    /// If there are partial bits in the buffer, they are discarded to
    /// align to the next byte boundary.
    pub fn read_byte_aligned(&mut self) -> Result<u8> {
        // Discard partial bits
        let remainder = self.bits_in_buffer % 8;
        if remainder > 0 {
            self.skip_bits(remainder)?;
        }

        // Read from buffer if we have a full byte
        if self.bits_in_buffer >= 8 {
            let byte = (self.buffer & 0xFF) as u8;
            self.buffer >>= 8;
            self.bits_in_buffer -= 8;
            self.total_bits_read += 8;
            return Ok(byte);
        }

        // Then from the prefetch buffer (buffered mode). Bytes at or beyond
        // `buf_len` are stale, so the cursor is range-checked against it.
        if self.buf_pos < self.buf_len {
            if let Some(&byte) = self.buf.get(self.buf_pos) {
                self.buf_pos += 1;
                self.total_bits_read += 8;
                return Ok(byte);
            }
        }

        // Otherwise read directly.
        let mut byte = [0u8; 1];
        self.reader.read_exact(&mut byte)?;
        self.total_bits_read += 8;
        Ok(byte[0])
    }

    /// Align to the next byte boundary by discarding partial bits.
    pub fn align_to_byte(&mut self) {
        let remainder = self.bits_in_buffer % 8;
        if remainder > 0 {
            self.buffer >>= remainder;
            self.bits_in_buffer -= remainder;
            self.total_bits_read += remainder as u64;
        }
    }

    /// Check if the reader is at end of stream.
    ///
    /// Note: This only checks if the buffer is empty and attempts one read.
    pub fn is_eof(&mut self) -> bool {
        if self.bits_in_buffer > 0 {
            return false;
        }
        if self.buf_pos < self.buf_len {
            return !self.refill_byte();
        }

        let mut byte = [0u8; 1];
        match self.reader.read(&mut byte) {
            Ok(0) => true,
            Ok(_) => {
                self.buffer = byte[0] as u64;
                self.bits_in_buffer = 8;
                false
            }
            Err(_) => true,
        }
    }

    /// Read bytes directly, skipping the bit buffer.
    ///
    /// The bit buffer must be byte-aligned before calling this method.
    pub fn read_bytes(&mut self, buf: &mut [u8]) -> Result<()> {
        // First, drain any complete bytes from the bit buffer
        let mut offset = 0;
        while self.bits_in_buffer >= 8 && offset < buf.len() {
            if let Some(slot) = buf.get_mut(offset) {
                *slot = (self.buffer & 0xFF) as u8;
            }
            self.buffer >>= 8;
            self.bits_in_buffer -= 8;
            self.total_bits_read += 8;
            offset += 1;
        }

        // Then drain the prefetch buffer (buffered mode) — these bytes were
        // already taken from the underlying reader, so they must be delivered
        // before touching it again.
        while offset < buf.len() && self.buf_pos < self.buf_len {
            let want = buf.len() - offset;
            let have = self.buf_len - self.buf_pos;
            let n = want.min(have);
            match (
                self.buf.get(self.buf_pos..self.buf_pos + n),
                buf.get_mut(offset..offset + n),
            ) {
                (Some(src), Some(dst)) => dst.copy_from_slice(src),
                _ => break,
            }
            self.buf_pos += n;
            offset += n;
            self.total_bits_read += n as u64 * 8;
        }

        // Read remaining bytes directly
        if offset < buf.len() {
            let remaining = buf.len() - offset;
            if let Some(dst) = buf.get_mut(offset..) {
                self.reader.read_exact(dst)?;
            }
            self.total_bits_read += remaining as u64 * 8;
        }

        Ok(())
    }
}

/// A bit-level writer that wraps any `Write` implementation.
///
/// `BitWriter` accumulates bits in an internal buffer and flushes complete
/// bytes to the underlying writer. Call `flush()` when done to write any
/// remaining partial byte.
#[derive(Debug)]
pub struct BitWriter<W: Write> {
    /// Underlying writer. `None` only in the instant between `into_inner`
    /// taking it and the enclosing `self` finishing its (now-skipped) drop;
    /// no other method can observe `None` here, since every method other
    /// than `into_inner` takes `&self`/`&mut self` and `into_inner` consumes
    /// `self` by value, so the type system rules out any further call on the
    /// same `BitWriter` afterward.
    writer: Option<W>,
    /// Bit buffer (LSB-first).
    buffer: u64,
    /// Number of bits in buffer.
    bits_in_buffer: u8,
    /// Total bits written.
    total_bits_written: u64,
}

impl<W: Write> BitWriter<W> {
    /// Create a new `BitWriter` wrapping the given writer.
    pub fn new(writer: W) -> Self {
        Self {
            writer: Some(writer),
            buffer: 0,
            bits_in_buffer: 0,
            total_bits_written: 0,
        }
    }

    /// Build the error used when `writer` is unexpectedly `None`.
    ///
    /// Centralized so the (unreachable-via-the-public-API) message is
    /// written once rather than duplicated at every call site.
    #[cold]
    fn writer_taken_error() -> OxiArcError {
        OxiArcError::Io(std::io::Error::other(
            "BitWriter: writer accessed after into_inner (unreachable via the public API)",
        ))
    }

    /// Fallibly borrow the underlying writer for internal write-path use.
    ///
    /// # Errors
    ///
    /// Never, in practice: the only way `writer` becomes `None` is
    /// `into_inner`, which consumes `self` by value and therefore makes this
    /// method uncallable afterward. `Result` (rather than a panic) so the
    /// every call site propagates with a plain `?` instead of adding a new
    /// `.expect()`.
    #[inline]
    fn try_writer_mut(&mut self) -> Result<&mut W> {
        self.writer.as_mut().ok_or_else(Self::writer_taken_error)
    }

    /// Get a reference to the underlying writer.
    ///
    /// # Panics
    ///
    /// Never, in practice — see `Self::try_writer_mut`. Unlike the
    /// internal write path this is public, infallible API (mirroring
    /// [`std::io::BufWriter::get_ref`]), so there is no `Result` to
    /// propagate into.
    pub fn get_ref(&self) -> &W {
        self.writer
            .as_ref()
            .expect("BitWriter: writer accessed after into_inner (unreachable via the public API)")
    }

    /// Get a mutable reference to the underlying writer.
    ///
    /// # Panics
    ///
    /// See [`Self::get_ref`].
    pub fn get_mut(&mut self) -> &mut W {
        self.writer
            .as_mut()
            .expect("BitWriter: writer accessed after into_inner (unreachable via the public API)")
    }

    /// Consume this `BitWriter` and return the underlying writer.
    ///
    /// This flushes any remaining bits before returning the writer.
    pub fn into_inner(mut self) -> Result<W> {
        self.flush()?;
        // `flush()` above only touches the writer through `try_writer_mut()`,
        // so `self.writer` is still `Some` here; `Drop::drop` sees `None`
        // after this `take()` and skips its best-effort flush accordingly.
        self.writer.take().ok_or_else(Self::writer_taken_error)
    }

    /// Get the total number of bits written so far.
    pub fn bits_written(&self) -> u64 {
        self.total_bits_written
    }

    /// Flush complete bytes from the buffer to the writer.
    /// Optimized to write multiple bytes at once.
    #[inline]
    fn flush_bytes(&mut self) -> Result<()> {
        // Optimize: write multiple bytes at once when possible
        if self.bits_in_buffer >= 32 {
            let bytes = [
                (self.buffer & 0xFF) as u8,
                ((self.buffer >> 8) & 0xFF) as u8,
                ((self.buffer >> 16) & 0xFF) as u8,
                ((self.buffer >> 24) & 0xFF) as u8,
            ];
            self.try_writer_mut()?.write_all(&bytes)?;
            self.buffer >>= 32;
            self.bits_in_buffer -= 32;
        }

        // Write remaining complete bytes one at a time
        while self.bits_in_buffer >= 8 {
            let byte = (self.buffer & 0xFF) as u8;
            self.try_writer_mut()?.write_all(&[byte])?;
            self.buffer >>= 8;
            self.bits_in_buffer -= 8;
        }
        Ok(())
    }

    /// Write up to 32 bits to the stream.
    ///
    /// # Arguments
    ///
    /// * `value` - The bits to write (LSB-first)
    /// * `count` - Number of bits to write (0-32)
    #[inline]
    pub fn write_bits(&mut self, value: u32, count: u8) -> Result<()> {
        debug_assert!(count <= 32, "Cannot write more than 32 bits at once");

        if count == 0 {
            return Ok(());
        }

        // Mask off any extra bits (optimized with wrapping_sub)
        let mask = if count == 32 {
            u32::MAX
        } else {
            (1u32 << count).wrapping_sub(1)
        };
        let value = value & mask;

        // Add bits to buffer
        self.buffer |= (value as u64) << self.bits_in_buffer;
        self.bits_in_buffer += count;
        self.total_bits_written += count as u64;

        // Flush complete bytes
        self.flush_bytes()?;

        Ok(())
    }

    /// Write a single bit.
    #[inline(always)]
    pub fn write_bit(&mut self, bit: bool) -> Result<()> {
        // Inline the critical path for single bit writes
        self.buffer |= (bit as u64) << self.bits_in_buffer;
        self.bits_in_buffer += 1;
        self.total_bits_written += 1;

        if self.bits_in_buffer >= 8 {
            self.flush_bytes()?;
        }

        Ok(())
    }

    /// Write an aligned byte.
    ///
    /// If there are partial bits in the buffer, they are padded with zeros
    /// to complete the byte before writing the new byte.
    pub fn write_byte_aligned(&mut self, byte: u8) -> Result<()> {
        // Pad to byte boundary
        if self.bits_in_buffer % 8 != 0 {
            let padding = 8 - (self.bits_in_buffer % 8);
            self.write_bits(0, padding)?;
        }

        // Write the byte
        self.write_bits(byte as u32, 8)
    }

    /// Pad to byte boundary with zeros and flush.
    pub fn align_to_byte(&mut self) -> Result<()> {
        if self.bits_in_buffer % 8 != 0 {
            let padding = 8 - (self.bits_in_buffer % 8);
            self.write_bits(0, padding)?;
        }
        Ok(())
    }

    /// Flush any remaining bits to the underlying writer.
    ///
    /// If there are partial bits, they are padded with zeros to complete
    /// the final byte.
    pub fn flush(&mut self) -> Result<()> {
        // Pad to byte boundary
        self.align_to_byte()?;

        // Flush any remaining complete bytes
        self.flush_bytes()?;

        // Flush underlying writer
        self.try_writer_mut()?.flush()?;

        Ok(())
    }

    /// Write bytes directly to the stream.
    ///
    /// The bit buffer should be byte-aligned before calling this method.
    pub fn write_bytes(&mut self, buf: &[u8]) -> Result<()> {
        // Flush current buffer first
        self.flush_bytes()?;

        // If we have partial bits, we need to merge them
        if self.bits_in_buffer > 0 {
            for &byte in buf {
                self.write_bits(byte as u32, 8)?;
            }
        } else {
            // Direct write
            self.try_writer_mut()?.write_all(buf)?;
            self.total_bits_written += buf.len() as u64 * 8;
        }

        Ok(())
    }
}

impl<W: Write> Drop for BitWriter<W> {
    fn drop(&mut self) {
        // `into_inner` takes `writer`, leaving `None`, right before this
        // `self` finishes its own (now-skipped) drop; every other path still
        // has `Some` and gets the usual best-effort flush.
        if self.writer.is_some() {
            let _ = self.flush();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn test_bitreader_basic() {
        // 0b10110101 = 0xB5
        let data = vec![0xB5];
        let mut reader = BitReader::new(Cursor::new(data));

        assert_eq!(reader.read_bits(1).expect("read bit 1"), 1); // LSB first
        assert_eq!(reader.read_bits(1).expect("read bit 2"), 0);
        assert_eq!(reader.read_bits(1).expect("read bit 3"), 1);
        assert_eq!(reader.read_bits(1).expect("read bit 4"), 0);
        assert_eq!(reader.read_bits(1).expect("read bit 5"), 1);
        assert_eq!(reader.read_bits(1).expect("read bit 6"), 1);
        assert_eq!(reader.read_bits(1).expect("read bit 7"), 0);
        assert_eq!(reader.read_bits(1).expect("read bit 8"), 1);
    }

    #[test]
    fn test_bitreader_multi_byte() {
        let data = vec![0xFF, 0x00];
        let mut reader = BitReader::new(Cursor::new(data));

        assert_eq!(reader.read_bits(4).expect("read 4 bits"), 0xF);
        assert_eq!(
            reader
                .read_bits(8)
                .expect("read 8 bits crossing byte boundary"),
            0x0F
        ); // Crosses byte boundary
        assert_eq!(reader.read_bits(4).expect("read final 4 bits"), 0x0);
    }

    #[test]
    fn test_bitreader_peek() {
        let data = vec![0xAB];
        let mut reader = BitReader::new(Cursor::new(data));

        assert_eq!(reader.peek_bits(4).expect("peek 4 bits"), 0xB);
        assert_eq!(reader.peek_bits(4).expect("peek 4 bits again"), 0xB); // Same value
        assert_eq!(reader.read_bits(4).expect("read 4 bits"), 0xB); // Now consume
        assert_eq!(reader.peek_bits(4).expect("peek remaining 4 bits"), 0xA);
    }

    #[test]
    fn test_bitwriter_basic() {
        let mut output = Vec::new();
        {
            let mut writer = BitWriter::new(&mut output);
            // Write 0b10110101 bit by bit
            writer.write_bit(true).expect("write bit"); // 1
            writer.write_bit(false).expect("write bit"); // 0
            writer.write_bit(true).expect("write bit"); // 1
            writer.write_bit(false).expect("write bit"); // 0
            writer.write_bit(true).expect("write bit"); // 1
            writer.write_bit(true).expect("write bit"); // 1
            writer.write_bit(false).expect("write bit"); // 0
            writer.write_bit(true).expect("write bit"); // 1
            writer.flush().expect("flush writer");
        }
        assert_eq!(output, vec![0xB5]);
    }

    #[test]
    fn test_bitwriter_multi_bits() {
        let mut output = Vec::new();
        {
            let mut writer = BitWriter::new(&mut output);
            writer.write_bits(0b101, 3).expect("write 3 bits");
            writer.write_bits(0b11001, 5).expect("write 5 bits");
            writer.flush().expect("flush writer");
        }
        // 3 bits: 101, 5 bits: 11001 -> 11001_101 = 0xCD
        assert_eq!(output, vec![0xCD]);
    }

    #[test]
    fn test_roundtrip() {
        let mut output = Vec::new();
        {
            let mut writer = BitWriter::new(&mut output);
            writer.write_bits(0b101, 3).expect("write 3 bits");
            writer.write_bits(0b1111, 4).expect("write 4 bits");
            writer.write_bits(0b10, 2).expect("write 2 bits");
            writer.write_bits(0b110011, 6).expect("write 6 bits");
            writer.flush().expect("flush writer");
        }

        let mut reader = BitReader::new(Cursor::new(&output));
        assert_eq!(reader.read_bits(3).expect("read 3 bits"), 0b101);
        assert_eq!(reader.read_bits(4).expect("read 4 bits"), 0b1111);
        assert_eq!(reader.read_bits(2).expect("read 2 bits"), 0b10);
        assert_eq!(reader.read_bits(6).expect("read 6 bits"), 0b110011);
    }

    #[test]
    fn test_align_to_byte() {
        let data = vec![0xFF, 0xAA];
        let mut reader = BitReader::new(Cursor::new(data));

        reader.read_bits(3).expect("read 3 bits"); // Read 3 bits
        reader.align_to_byte(); // Skip remaining 5 bits
        assert_eq!(reader.read_bits(8).expect("read byte after align"), 0xAA);
    }

    #[test]
    fn test_read_bytes() {
        let data = vec![0x12, 0x34, 0x56, 0x78];
        let mut reader = BitReader::new(Cursor::new(data));

        let mut buf = [0u8; 2];
        reader.read_bytes(&mut buf).expect("read first 2 bytes");
        assert_eq!(buf, [0x12, 0x34]);

        reader.read_bytes(&mut buf).expect("read next 2 bytes");
        assert_eq!(buf, [0x56, 0x78]);
    }

    /// A `Read` adapter that yields at most one byte per `read()` call,
    /// reproducing the short reads produced by TCP sockets, pipes, stdin, and
    /// throttled/adapter readers. Used to exercise the `fill_buffer` retry loop
    /// (CORE-01): a naive single-`read()` implementation would spuriously report
    /// `UnexpectedEof` on the first short read of valid data.
    struct OneByteAtATime<R: Read> {
        inner: R,
    }

    impl<R: Read> Read for OneByteAtATime<R> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if buf.is_empty() {
                return Ok(0);
            }
            self.inner.read(&mut buf[..1])
        }
    }

    #[test]
    fn test_fill_buffer_handles_short_reads_16bit() {
        // 0x34, 0x12 -> LSB-first 16-bit value 0x1234. A single fill_buffer(16)
        // must pack both bytes across two short reads instead of erroring.
        let reader = OneByteAtATime {
            inner: Cursor::new(vec![0x34, 0x12]),
        };
        let mut bit_reader = BitReader::new(reader);
        assert_eq!(
            bit_reader
                .read_bits(16)
                .expect("short-read fill_buffer must assemble the full 16-bit value"),
            0x1234
        );
    }

    #[test]
    fn test_fill_buffer_handles_short_reads_32bit() {
        // Four bytes delivered one at a time, assembled into a 32-bit value.
        let reader = OneByteAtATime {
            inner: Cursor::new(vec![0x78, 0x56, 0x34, 0x12]),
        };
        let mut bit_reader = BitReader::new(reader);
        assert_eq!(
            bit_reader
                .read_bits(32)
                .expect("short-read fill_buffer must assemble the full 32-bit value"),
            0x1234_5678
        );
    }

    // ─────────────────────────────────────────────────────────────────────
    // Buffered mode / BitCache
    // ─────────────────────────────────────────────────────────────────────

    fn pseudo_random(len: usize) -> Vec<u8> {
        let mut x = 0x1234_5678_9abc_def0u64;
        (0..len)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x as u8
            })
            .collect()
    }

    /// Buffered mode must deliver exactly the same bit sequence as exact
    /// mode, across every read width and every buffer-boundary crossing.
    #[test]
    fn buffered_matches_exact_bit_for_bit() {
        let data = pseudo_random(40_000);
        for width in 1..=32u8 {
            let mut exact = BitReader::new(Cursor::new(&data));
            // A small buffer forces many refills and boundary crossings.
            let mut buffered = BitReader::with_buffer_capacity(Cursor::new(&data), 0);
            loop {
                let a = exact.read_bits(width);
                let b = buffered.read_bits(width);
                match (a, b) {
                    (Ok(x), Ok(y)) => assert_eq!(x, y, "width {width}"),
                    (Err(_), Err(_)) => break,
                    (x, y) => panic!("width {width}: divergent termination {x:?} vs {y:?}"),
                }
                assert_eq!(exact.bits_read(), buffered.bits_read(), "width {width}");
            }
        }
    }

    /// Mixed bit and byte reads must agree between the two modes: the
    /// buffered reader has to drain its prefetch before touching the source.
    #[test]
    fn buffered_mixed_bit_and_byte_reads() {
        let data = pseudo_random(9_000);
        let mut exact = BitReader::new(Cursor::new(&data));
        let mut buffered = BitReader::buffered(Cursor::new(&data));
        for round in 0..500 {
            let bits = (round % 17 + 1) as u8;
            assert_eq!(
                exact.read_bits(bits).expect("exact bits"),
                buffered.read_bits(bits).expect("buffered bits"),
                "round {round}"
            );
            exact.align_to_byte();
            buffered.align_to_byte();
            let mut a = [0u8; 5];
            let mut b = [0u8; 5];
            exact.read_bytes(&mut a).expect("exact bytes");
            buffered.read_bytes(&mut b).expect("buffered bytes");
            assert_eq!(a, b, "round {round}");
            assert_eq!(
                exact.read_byte_aligned().expect("exact byte"),
                buffered.read_byte_aligned().expect("buffered byte"),
                "round {round}"
            );
            assert_eq!(exact.bits_read(), buffered.bits_read(), "round {round}");
        }
    }

    /// `into_parts` must hand back every prefetched-but-unconsumed byte so a
    /// caller can keep reading the underlying stream directly.
    #[test]
    fn into_parts_returns_the_prefetch() {
        let data = pseudo_random(3_000);
        let mut reader = BitReader::buffered(Cursor::new(&data));
        reader.read_bits(24).expect("read");
        let consumed = (reader.bits_read() / 8) as usize;
        let (mut cursor, pending) = reader.into_parts();
        let mut rest = pending;
        let mut tail = Vec::new();
        cursor.read_to_end(&mut tail).expect("read to end");
        rest.extend_from_slice(&tail);
        assert_eq!(rest, &data[consumed..], "prefetch must not be lost");
    }

    #[test]
    fn buffered_len_tracks_prefetch() {
        let data = pseudo_random(3_000);
        let mut reader = BitReader::buffered(Cursor::new(&data));
        assert_eq!(reader.buffered_len(), 0);
        reader.read_bits(1).expect("read");
        assert!(reader.buffered_len() > 0, "buffered mode should prefetch");
        let mut exact = BitReader::new(Cursor::new(&data));
        exact.read_bits(1).expect("read");
        assert_eq!(exact.buffered_len(), 0, "exact mode must not prefetch");
    }

    /// A detached cache must decode the same bits as the reader would, and
    /// `reattach` must restore an exact bit position.
    #[test]
    fn bit_cache_round_trip() {
        let data = pseudo_random(20_000);
        let mut plain = BitReader::buffered(Cursor::new(&data));
        let mut cached = BitReader::buffered(Cursor::new(&data));

        let mut cache = cached.detach();
        for round in 0..2_000 {
            let width = (round % 13 + 1) as u8;
            cached.refill_cache(&mut cache, width).expect("refill");
            assert!(cache.available() >= width, "round {round}");
            let value = cache.peek_bits(width);
            cache.consume(width);
            assert_eq!(
                plain.read_bits(width).expect("plain"),
                value,
                "round {round}"
            );
        }
        cached.reattach(cache);
        assert_eq!(plain.bits_read(), cached.bits_read());
        // Both readers must continue identically afterwards.
        assert_eq!(
            plain.read_bits(19).expect("plain tail"),
            cached.read_bits(19).expect("cached tail")
        );
    }

    /// At end of stream the cache simply runs dry — `refill_cache` must not
    /// report an error, and `available()` tells the caller what is left.
    #[test]
    fn bit_cache_end_of_stream_is_not_an_error() {
        let data = [0xAAu8, 0x55];
        let mut reader = BitReader::buffered(Cursor::new(&data));
        let mut cache = reader.detach();
        reader.refill_cache(&mut cache, 57).expect("refill");
        assert_eq!(cache.available(), 16);
        cache.consume(16);
        reader.refill_cache(&mut cache, 8).expect("refill at eof");
        assert_eq!(cache.available(), 0);
        reader.reattach(cache);
        assert_eq!(reader.bits_read(), 16);
        assert!(reader.read_bits(1).is_err());
    }

    /// Consuming more bits than are held must empty the cache rather than
    /// corrupt the bit position.
    #[test]
    fn bit_cache_over_consume_is_bounded() {
        let data = [0xFFu8];
        let mut reader = BitReader::buffered(Cursor::new(&data));
        let mut cache = reader.detach();
        reader.refill_cache(&mut cache, 8).expect("refill");
        cache.consume(200);
        assert_eq!(cache.available(), 0);
        assert_eq!(cache.consumed(), 8);
    }

    /// `skip_bits` is public API and must handle counts above 64 without
    /// corrupting the position or panicking on an over-wide shift.
    #[test]
    fn skip_bits_handles_wide_counts() {
        let data = pseudo_random(64);
        let mut a = BitReader::new(Cursor::new(&data));
        let mut b = BitReader::new(Cursor::new(&data));
        a.skip_bits(200).expect("skip");
        for _ in 0..200 {
            b.read_bits(1).expect("read");
        }
        assert_eq!(a.bits_read(), b.bits_read());
        assert_eq!(a.read_bits(8).expect("a"), b.read_bits(8).expect("b"));
    }

    /// Buffered mode must tolerate a source that yields one byte per call.
    #[test]
    fn buffered_handles_short_reads() {
        let data = pseudo_random(1_000);
        let source = OneByteAtATime {
            inner: Cursor::new(data.clone()),
        };
        let mut reader = BitReader::buffered(source);
        let mut reference = BitReader::new(Cursor::new(&data));
        for _ in 0..500 {
            assert_eq!(
                reader.read_bits(13).expect("buffered"),
                reference.read_bits(13).expect("reference")
            );
        }
    }

    #[test]
    fn test_fill_buffer_genuine_eof_after_short_reads() {
        // Only one byte is available but 16 bits are requested: the loop must
        // consume the byte, then return UnexpectedEof on the genuine Ok(0),
        // rather than failing prematurely on the first short read.
        let reader = OneByteAtATime {
            inner: Cursor::new(vec![0xAB]),
        };
        let mut bit_reader = BitReader::new(reader);
        let err = bit_reader
            .read_bits(16)
            .expect_err("must surface EOF once the reader is genuinely exhausted");
        assert!(matches!(err, OxiArcError::UnexpectedEof { .. }));
    }

    // ── BitCache slice-side refills ─────────────────────────────────────
    //
    // These must reproduce, bit for bit, what a `BitReader` over the same
    // bytes produces: the slice-driven DEFLATE decoder relies on it.

    /// Deterministic xorshift PRNG (no external dependency).
    struct XorShift(u64);

    impl XorShift {
        fn next_u64(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
    }

    #[test]
    fn cache_refill_bulk_matches_bit_reader() {
        let mut rng = XorShift(0x1234_5678_9ABC_DEF0);
        let data: Vec<u8> = (0..4096).map(|_| rng.next_u64() as u8).collect();

        let mut cache = BitCache::default();
        let mut pos = 0usize;
        let mut reference = BitReader::buffered(Cursor::new(data.clone()));

        // Read a pseudo-random schedule of bit widths through both paths.
        for _ in 0..2000 {
            let want = (rng.next_u64() % 17) as u8 + 1;
            if cache.available() < want {
                if let Some(rest) = data.get(pos..) {
                    pos += cache.refill_bulk(rest);
                }
                if let Some(rest) = data.get(pos..) {
                    pos += cache.refill_bytes(rest, want);
                }
            }
            if cache.available() < want {
                break;
            }
            let got = cache.peek_bits(want);
            cache.consume(want);
            let expected = reference.read_bits(want).expect("reference read");
            assert_eq!(got, expected, "bit stream diverged at width {want}");
        }
    }

    #[test]
    fn cache_refill_bulk_is_a_noop_when_short_or_full() {
        let mut cache = BitCache::default();
        assert_eq!(cache.refill_bulk(&[0u8; 7]), 0);
        assert_eq!(cache.available(), 0);

        assert_eq!(cache.refill_bulk(&[0xFFu8; 8]), 7);
        assert_eq!(cache.available(), 56);
        // 56 > 55, so the second bulk load is refused.
        assert_eq!(cache.refill_bulk(&[0xFFu8; 8]), 0);
        assert_eq!(cache.available(), 56);
    }

    #[test]
    fn cache_refill_bytes_stops_at_want_and_at_end_of_input() {
        let mut cache = BitCache::default();
        assert_eq!(cache.refill_bytes(&[1, 2, 3, 4], 24), 3);
        assert_eq!(cache.available(), 24);
        // Already satisfied: nothing more is taken.
        assert_eq!(cache.refill_bytes(&[5, 6], 24), 0);
        // Short input: takes what there is.
        let mut cache = BitCache::default();
        assert_eq!(cache.refill_bytes(&[9], 32), 1);
        assert_eq!(cache.available(), 8);
    }

    #[test]
    fn cache_refill_bytes_never_exceeds_the_accumulator_ceiling() {
        let mut cache = BitCache::default();
        let src = [0xFFu8; 16];
        cache.refill_bytes(&src, 56);
        assert!(cache.available() <= 63, "accumulator overflowed");
        assert!(cache.available() >= 56);
    }

    #[test]
    fn cache_align_and_take_byte() {
        let mut cache = BitCache::default();
        cache.refill_bytes(&[0xDE, 0xAD, 0xBE], 24);
        cache.consume(4);
        assert_eq!(cache.align_to_byte(), 4);
        assert_eq!(cache.available(), 16);
        assert_eq!(cache.take_byte(), Some(0xAD));
        assert_eq!(cache.take_byte(), Some(0xBE));
        assert_eq!(cache.take_byte(), None);
        // Aligning an already-aligned cache is a no-op.
        assert_eq!(cache.align_to_byte(), 0);
    }

    #[test]
    fn cache_consumed_counts_aligned_and_taken_bits() {
        let mut cache = BitCache::default();
        cache.refill_bytes(&[1, 2, 3], 24);
        cache.consume(3);
        cache.align_to_byte();
        cache.take_byte();
        assert_eq!(cache.consumed(), 3 + 5 + 8);
    }
}
