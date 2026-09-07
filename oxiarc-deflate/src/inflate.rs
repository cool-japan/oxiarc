//! DEFLATE decompression (inflate).
//!
//! This module implements the DEFLATE decompression algorithm as specified
//! in RFC 1951. It supports all three block types:
//! - Type 0: Stored (uncompressed)
//! - Type 1: Fixed Huffman codes
//! - Type 2: Dynamic Huffman codes

use crate::huffman::HuffmanTree;
use crate::sink::{BoundedSink, InflateSink};
use crate::stream::{InflateStatus, InflateStream};
use crate::tables::{
    CODE_LENGTH_ORDER, DISTANCE_EXTRA_BITS, LENGTH_EXTRA_BITS, decode_distance, decode_length,
    fixed_distance_tree, fixed_litlen_tree,
};
use crate::window::{DecodeSink, InflateWindow};
use oxiarc_core::error::{OxiArcError, Result};
use oxiarc_core::traits::{DecompressStatus, Decompressor, FlushMode};
use oxiarc_core::{BitCache, BitReader};
use std::io::Read;

/// Maximum dictionary size for DEFLATE (32KB).
pub const MAX_DICTIONARY_SIZE: usize = 32768;

/// Output capacity a fresh [`Inflater`] starts with when no size hint is
/// supplied.
const DEFAULT_OUTPUT_CAPACITY: usize = 65536;

/// Upper bound applied to a caller-supplied output size hint.
///
/// A hint originates from data the decoder has not verified (a GZIP `ISIZE`
/// field, a container's declared tile size, ...), so it is clamped rather
/// than trusted: 64 MiB is far above any realistic single DEFLATE member yet
/// small enough that a crafted hint cannot be turned into a memory
/// exhaustion primitive.
pub const MAX_OUTPUT_CAPACITY_HINT: usize = 64 * 1024 * 1024;

/// DEFLATE decompressor.
#[derive(Debug)]
pub struct Inflater {
    /// Decoded output plus the LZ77 history window.
    output: InflateWindow,
    /// Whether we've seen the final block.
    final_block: bool,
    /// Whether decompression is complete.
    finished: bool,
    /// Expected dictionary checksum (if dictionary is required).
    expected_dict_checksum: Option<u32>,
    /// Set when `inflate_stored` processes a zero-length stored block (sync flush).
    last_empty_stored: bool,
    /// Resumable core backing the [`Decompressor`] trait implementation.
    ///
    /// Allocated on first use so an `Inflater` driven only through
    /// [`Inflater::inflate_reader`] (the ZIP/CAB path) pays nothing for it.
    trait_stream: Option<Box<InflateStream>>,
    /// The preset dictionary, retained so the lazily-created trait core can
    /// be seeded with it. Only the trailing [`MAX_DICTIONARY_SIZE`] bytes
    /// are kept, and the allocation is reused across calls.
    dictionary: Option<Vec<u8>>,
}

impl Inflater {
    /// Create a new DEFLATE decompressor.
    pub fn new() -> Self {
        Self::with_output_capacity(DEFAULT_OUTPUT_CAPACITY)
    }

    /// Create a new DEFLATE decompressor that pre-allocates room for
    /// `size_hint` decompressed bytes.
    ///
    /// This only avoids repeated reallocation while decoding; it is **not**
    /// trusted as an authoritative output size. The hint is clamped to
    /// [`MAX_OUTPUT_CAPACITY_HINT`] so a hostile container header (a GZIP
    /// `ISIZE`, a TIFF tile size, ...) cannot turn into a huge speculative
    /// allocation, and the buffer still grows normally if the real output is
    /// larger.
    pub fn with_output_capacity(size_hint: usize) -> Self {
        Self {
            output: InflateWindow::with_capacity(size_hint.min(MAX_OUTPUT_CAPACITY_HINT)),
            final_block: false,
            finished: false,
            expected_dict_checksum: None,
            last_empty_stored: false,
            trait_stream: None,
            dictionary: None,
        }
    }

    /// Create a new DEFLATE decompressor with a preset dictionary.
    ///
    /// The dictionary must match the one used during compression.
    /// The decompressor uses the dictionary to resolve back-references
    /// that point into the dictionary content.
    ///
    /// # Arguments
    ///
    /// * `dictionary` - Dictionary data (up to 32KB). If larger, only the
    ///   last 32KB is used.
    ///
    /// # Returns
    ///
    /// A new Inflater with the dictionary preloaded.
    pub fn with_dictionary(dictionary: &[u8]) -> Self {
        let mut inflater = Self::new();
        inflater.set_dictionary(dictionary);
        inflater
    }

    /// Set a preset dictionary for decompression.
    ///
    /// # Arguments
    ///
    /// * `dictionary` - Dictionary data (up to 32KB). If larger, only the
    ///   last 32KB is used.
    ///
    /// # Returns
    ///
    /// The Adler-32 checksum of the dictionary.
    pub fn set_dictionary(&mut self, dictionary: &[u8]) -> u32 {
        self.output.preload_dictionary(dictionary);
        let tail = dictionary
            .get(dictionary.len().saturating_sub(MAX_DICTIONARY_SIZE)..)
            .unwrap_or(dictionary);
        let kept = self.dictionary.get_or_insert_with(Vec::new);
        kept.clear();
        kept.extend_from_slice(tail);
        if let Some(stream) = self.trait_stream.as_mut() {
            stream.set_dictionary(tail);
        }
        let checksum = Self::adler32(dictionary);
        self.expected_dict_checksum = Some(checksum);
        checksum
    }

    /// Get the expected dictionary checksum.
    pub fn expected_dictionary_checksum(&self) -> Option<u32> {
        self.expected_dict_checksum
    }

    /// Check if a dictionary is currently set.
    pub fn has_dictionary(&self) -> bool {
        self.expected_dict_checksum.is_some()
    }

    /// Calculate Adler-32 checksum (for dictionary identification).
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
                a += byte as u32;
                b += a;
            }

            a %= MOD_ADLER;
            b %= MOD_ADLER;
        }

        for &byte in remaining {
            a += byte as u32;
            b += a;
        }

        ((b % MOD_ADLER) << 16) | (a % MOD_ADLER)
    }

    /// Reset the decompressor.
    ///
    /// Everything is cleared: the decoded output and its history, the
    /// dictionary, the block state and — since 0.4.2 — the streaming core
    /// backing [`Decompressor::decompress`], including its latched error.
    /// A reset `Inflater` therefore decodes a fresh, independent stream,
    /// which is what a multi-block CAB/MSZIP folder relies on.
    pub fn reset(&mut self) {
        self.output.clear();
        self.final_block = false;
        self.finished = false;
        self.expected_dict_checksum = None;
        self.last_empty_stored = false;
        self.dictionary = None;
        self.trait_stream = None;
    }

    /// Reset the decompressor but keep the dictionary.
    pub fn reset_keep_dictionary(&mut self) {
        let checksum = self.expected_dict_checksum;
        self.output.clear();
        self.final_block = false;
        self.finished = false;
        self.expected_dict_checksum = checksum;
        self.last_empty_stored = false;
        if let Some(stream) = self.trait_stream.as_mut() {
            stream.reset();
            if let Some(dictionary) = self.dictionary.as_deref() {
                stream.set_dictionary(dictionary);
            }
        }
    }

    /// Decompress data from a reader.
    ///
    /// The `BitReader` created here owns `reader` for the duration of the
    /// call, so it uses the fast buffered refill mode. Bytes prefetched past
    /// the end of the DEFLATE stream are *not* returned to `reader`; use
    /// [`Inflater::inflate_consumed`] with a caller-owned
    /// [`BitReader::new`] when the same stream must be read on afterwards.
    pub fn inflate_reader<R: Read>(&mut self, reader: &mut R) -> Result<Vec<u8>> {
        let mut bit_reader = BitReader::buffered(reader);
        self.inflate(&mut bit_reader)
    }

    /// Decompress data from a caller-owned [`BitReader`] and also report
    /// how many whole bytes of the underlying byte-stream the DEFLATE
    /// data consumed.
    ///
    /// DEFLATE is bit-aligned but this method rounds the consumed-bit
    /// count up to the next whole byte — this is the byte count needed
    /// by formats that place a byte-aligned trailer immediately after
    /// the compressed stream (e.g., ZIP's data-descriptor per APPNOTE
    /// §4.3.9).
    ///
    /// Because the `BitReader` is owned by the caller, any further reads
    /// on the SAME `BitReader` after this method returns will correctly
    /// drain its internal buffer before touching the underlying reader —
    /// no bytes are lost to the DEFLATE prefetch.
    ///
    /// On return, `reader` is aligned to the next byte boundary
    /// (intra-byte padding bits have been skipped). Reads via the
    /// `BitReader` continue from that byte boundary.
    pub fn inflate_consumed<R: Read>(
        &mut self,
        reader: &mut BitReader<R>,
    ) -> Result<(Vec<u8>, u64)> {
        let bits_before = reader.bits_read();
        let decompressed = self.inflate(reader)?;
        // Align to the byte boundary so byte-aligned structures following
        // the DEFLATE stream start at a clean offset.
        reader.align_to_byte();
        let bits_after = reader.bits_read();
        let consumed = (bits_after - bits_before).div_ceil(8);
        Ok((decompressed, consumed))
    }

    /// Decompress data from a bit reader.
    pub fn inflate<R: Read>(&mut self, reader: &mut BitReader<R>) -> Result<Vec<u8>> {
        while !self.final_block {
            self.inflate_block(reader)?;
        }

        self.finished = true;
        Ok(self.output.output().to_vec())
    }

    /// Decompress a single block into this inflater's window.
    fn inflate_block<R: Read>(&mut self, reader: &mut BitReader<R>) -> Result<()> {
        let mut state = BlockState {
            final_block: self.final_block,
            last_empty_stored: self.last_empty_stored,
        };
        let result = inflate_block_into(reader, &mut self.output, &mut state);
        self.final_block = state.final_block;
        self.last_empty_stored = state.last_empty_stored;
        result
    }

    /// Get the decompressed output.
    pub fn output(&self) -> &[u8] {
        self.output.output()
    }

    /// Take ownership of the decompressed output.
    pub fn into_output(self) -> Vec<u8> {
        self.output.into_output()
    }

    /// Try to decompress one RFC 4978 sync-flushed unit from `input`.
    ///
    /// Processes DEFLATE blocks one at a time until an empty stored block
    /// (sync flush, `LEN=0`/`NLEN=0xFFFF`) is encountered. Detection is done at
    /// the **bit level** (correct) — no byte-pattern scanning (which would give
    /// false positives in Huffman-encoded data).
    ///
    /// # Returns
    ///
    /// - `Ok(Some((bytes, consumed)))` — decompressed bytes and number of bytes
    ///   consumed from `input`. The LZ77 sliding window is advanced.
    /// - `Ok(None)` — more input bytes are needed; the inflater state is **fully
    ///   restored** to what it was before this call (snapshot/restore), so the
    ///   caller can safely retry with a larger buffer.
    /// - `Err(e)` — unrecoverable parse error; discard this inflater.
    pub fn try_decompress_sync_unit(
        &mut self,
        input: &[u8],
    ) -> oxiarc_core::error::Result<Option<(Vec<u8>, usize)>> {
        // Snapshot before any work so we can roll back on partial-delivery EOF.
        let ring_snap = self.output.snapshot();
        let out_len_before = self.output.output_len();

        let cursor = std::io::Cursor::new(input);
        // The BitReader owns the cursor and `bits_read()` stays exact, so the
        // fast buffered refill mode is safe here.
        let mut br = oxiarc_core::BitReader::buffered(cursor);

        loop {
            self.last_empty_stored = false;
            match self.inflate_block(&mut br) {
                Ok(()) => {
                    if self.last_empty_stored {
                        // Sync flush complete — align and report bytes consumed.
                        br.align_to_byte();
                        let bytes_consumed = usize::try_from(br.bits_read())
                            .unwrap_or(usize::MAX)
                            .div_ceil(8);
                        let decompressed = self.output.drain();
                        return Ok(Some((decompressed, bytes_consumed)));
                    }
                    // Non-empty block; continue to next block.
                }
                Err(oxiarc_core::error::OxiArcError::Io(ref io_err))
                    if io_err.kind() == std::io::ErrorKind::UnexpectedEof =>
                {
                    // Input exhausted before sync flush — need more data.
                    // Fully restore to pre-call state so the caller can retry.
                    self.output.restore(&ring_snap, out_len_before);
                    self.last_empty_stored = false;
                    return Ok(None);
                }
                Err(oxiarc_core::error::OxiArcError::UnexpectedEof { .. }) => {
                    self.output.restore(&ring_snap, out_len_before);
                    self.last_empty_stored = false;
                    return Ok(None);
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// The retained preset dictionary, for the crate's own async adapter.
    #[cfg(feature = "async-io")]
    pub(crate) fn trait_dictionary(&self) -> Option<&[u8]> {
        self.dictionary.as_deref()
    }

    /// Mark the stream complete after an out-of-band decode.
    ///
    /// Used by the async adapter, which drives its own [`InflateStream`]
    /// but must leave `is_finished()` reporting the truth afterwards.
    #[cfg(feature = "async-io")]
    pub(crate) fn mark_finished(&mut self) {
        self.finished = true;
    }

    /// Decompress one complete sync-flushed chunk (convenience wrapper).
    ///
    /// `input` must be a complete sync-flush unit (all bytes from after the
    /// previous boundary up to and including the sync-flush empty stored block).
    /// Returns the decompressed bytes; LZ77 window is preserved for the next call.
    pub fn decompress_sync_chunk(&mut self, input: &[u8]) -> oxiarc_core::error::Result<Vec<u8>> {
        match self.try_decompress_sync_unit(input)? {
            Some((out, _)) => Ok(out),
            None => Err(oxiarc_core::error::OxiArcError::UnexpectedEof { expected: 1 }),
        }
    }
}

impl Default for Inflater {
    fn default() -> Self {
        Self::new()
    }
}

impl Decompressor for Inflater {
    /// Incrementally decompress a raw DEFLATE stream.
    ///
    /// Since 0.4.2 this is a genuine push decoder: state survives across
    /// calls, so a caller-supplied `output` smaller than the payload is
    /// drained over as many calls as it takes.
    ///
    /// # Contract
    ///
    /// `input` is the **whole remaining** compressed stream, per
    /// [`Decompressor`]'s trait-level contract: the call runs with
    /// [`FlushMode::Finish`], so a slice that ends mid-symbol is
    /// [`OxiArcError::UnexpectedEof`], never a short `Ok`. Callers that
    /// feed genuine chunks — bytes still in flight — must use
    /// [`InflateStream`] or [`WrappedInflate`](crate::WrappedInflate)
    /// directly, where the flush mode is an explicit parameter.
    ///
    /// Between calls `input` **must** be advanced by the reported
    /// `consumed` count. This changed in 0.4.2: the pre-0.4.2
    /// implementation decoded the whole slice on the first call and merely
    /// drained afterwards, so re-passing an unadvanced slice happened to be
    /// harmless. It no longer is — bytes already reported consumed would be
    /// decoded a second time and the caller would collect duplicated
    /// output. `decompress_all` and the async wrapper in this crate advance
    /// correctly; a caller writing its own loop must too:
    ///
    /// ```
    /// use oxiarc_core::traits::{DecompressStatus, Decompressor};
    /// use oxiarc_deflate::{deflate, Inflater};
    ///
    /// let compressed = deflate(&b"advance by consumed".repeat(200), 6).expect("deflate");
    /// let mut inflater = Inflater::new();
    /// let mut out = Vec::new();
    /// let mut scratch = [0u8; 64];
    /// let mut fed = 0usize;
    /// loop {
    ///     let (consumed, produced, status) = inflater
    ///         .decompress(&compressed[fed..], &mut scratch)
    ///         .expect("decompress");
    ///     fed += consumed; // <- mandatory
    ///     out.extend_from_slice(&scratch[..produced]);
    ///     if status == DecompressStatus::Done {
    ///         break;
    ///     }
    /// }
    /// assert_eq!(out, b"advance by consumed".repeat(200));
    /// ```
    ///
    /// Returns [`DecompressStatus::NeedsOutput`] while decoded bytes remain
    /// undelivered and [`DecompressStatus::Done`] only once the stream has
    /// ended — a payload larger than `output` is never silently truncated.
    ///
    /// # Errors
    ///
    /// Errors are **sticky**: once a stream has failed, every later call
    /// returns the same error until [`Inflater::reset`] is called. In
    /// particular a stream truncated mid-block cannot be turned into a
    /// short success by calling again.
    fn decompress(
        &mut self,
        input: &[u8],
        output: &mut [u8],
    ) -> Result<(usize, usize, DecompressStatus)> {
        if self.finished {
            return Ok((0, 0, DecompressStatus::Done));
        }
        if self.trait_stream.is_none() {
            let mut fresh = InflateStream::new();
            if let Some(dictionary) = self.dictionary.as_deref() {
                fresh.set_dictionary(dictionary);
            }
            self.trait_stream = Some(Box::new(fresh));
        }
        let Some(stream) = self.trait_stream.as_mut() else {
            return Err(OxiArcError::corrupted(
                0,
                "inflater streaming core unavailable",
            ));
        };
        let progress = stream.inflate(input, output, FlushMode::Finish)?;
        let status = match progress.status {
            InflateStatus::StreamEnd => {
                self.finished = true;
                DecompressStatus::Done
            }
            InflateStatus::NeedOutput => DecompressStatus::NeedsOutput,
            // Unreachable under `Finish`: running dry is raised as
            // `UnexpectedEof` rather than a request for more input.
            _ => DecompressStatus::NeedsInput,
        };
        Ok((progress.consumed, progress.produced, status))
    }

    fn reset(&mut self) {
        Inflater::reset(self);
    }

    fn is_finished(&self) -> bool {
        self.finished
    }
}

/// Decompress a raw DEFLATE stream held entirely in memory.
///
/// `data` must contain the whole stream; bytes after the final block are
/// ignored. Decoding runs on the resumable core with a growable sink, so
/// the compressed input is read in place — no copy into an internal reader
/// buffer, and no per-block table allocations.
///
/// # Errors
///
/// [`OxiArcError::UnexpectedEof`] if the stream ends before its final
/// block, plus the usual header/Huffman/back-reference errors for corrupt
/// input.
///
/// # Example
///
/// ```
/// use oxiarc_deflate::{deflate, inflate};
///
/// let compressed = deflate(b"round trip", 6).expect("deflate");
/// assert_eq!(inflate(&compressed).expect("inflate"), b"round trip");
/// ```
pub fn inflate(data: &[u8]) -> Result<Vec<u8>> {
    let mut stream = InflateStream::new();
    stream.inflate_to_vec(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_inflate_stored() {
        // Stored block: BFINAL=1, BTYPE=00, then aligned LEN=5, NLEN=!5, "Hello"
        // Header: 0b00000001 (BFINAL=1, BTYPE=00)
        // LEN: 0x05, 0x00
        // NLEN: 0xFA, 0xFF
        // Data: "Hello"
        let compressed = vec![
            0x01, // BFINAL=1, BTYPE=00, padding
            0x05, 0x00, // LEN=5
            0xFA, 0xFF, // NLEN=65530
            b'H', b'e', b'l', b'l', b'o',
        ];

        let result = inflate(&compressed).expect("inflate of stored block should succeed");
        assert_eq!(result, b"Hello");
    }

    #[test]
    fn test_inflate_empty() {
        // Empty stored block
        let compressed = vec![
            0x01, // BFINAL=1, BTYPE=00
            0x00, 0x00, // LEN=0
            0xFF, 0xFF, // NLEN
        ];

        let result = inflate(&compressed).expect("inflate of empty stored block should succeed");
        assert!(result.is_empty());
    }

    #[test]
    fn test_inflate_consumed_stored() -> Result<()> {
        // A stored block followed by trailing bytes that must remain
        // readable via the same BitReader after inflate completes.
        // Block: BFINAL=1, BTYPE=00, LEN=5, NLEN=!5, "Hello" = 10 bytes.
        let mut data = vec![
            0x01, // BFINAL=1, BTYPE=00, padding
            0x05, 0x00, // LEN=5
            0xFA, 0xFF, // NLEN
            b'H', b'e', b'l', b'l', b'o',
        ];
        data.extend_from_slice(&[0xAA, 0xBB, 0xCC, 0xDD]);

        let cursor = std::io::Cursor::new(&data);
        let mut bit_reader = BitReader::new(cursor);
        let mut inflater = Inflater::new();
        let (decompressed, consumed) = inflater.inflate_consumed(&mut bit_reader)?;
        assert_eq!(decompressed, b"Hello");
        // Stored block wire length: 1 header + 2 LEN + 2 NLEN + 5 data = 10 bytes
        assert_eq!(consumed, 10);

        // The 4 trailer bytes must be readable via the BitReader
        // (they drain from its buffer first, then the underlying cursor).
        let mut trailer = [0u8; 4];
        bit_reader.read_bytes(&mut trailer)?;
        assert_eq!(&trailer, &[0xAA, 0xBB, 0xCC, 0xDD]);
        Ok(())
    }

    /// Decoder-only test: hand-build a fixed-Huffman (BTYPE=01) block
    /// containing a literal followed by a maximum-length (258) back
    /// reference, and confirm `inflate` expands it correctly. This closes
    /// the coverage gap where length-258 handling was only ever exercised
    /// indirectly via encode-then-decode roundtrips (see
    /// `tests/edge_cases.rs::test_max_match_length`) and `tables.rs` unit
    /// tests on `length_to_code`, never via a decoder-only hand-built
    /// bitstream.
    #[test]
    fn test_inflate_hand_built_length_258_match() -> Result<()> {
        use oxiarc_core::BitWriter;

        // Write `length` bits of `code`, most-significant bit first.
        // DEFLATE Huffman codes are transmitted MSB-first (RFC 1951
        // SS3.2.2), while `BitWriter::write_bits` packs its `value`
        // LSB-first (bit 0 goes out first) — the same reason
        // `Deflater::write_huffman_code` pre-reverses canonical codes
        // before handing them to `BitWriter`. Feeding one bit at a time
        // from the top down sidesteps needing that reversal here.
        fn write_code_msb_first(
            writer: &mut BitWriter<&mut Vec<u8>>,
            code: u32,
            length: u8,
        ) -> Result<()> {
            for i in (0..length).rev() {
                writer.write_bit(((code >> i) & 1) != 0)?;
            }
            Ok(())
        }

        // Hand-build one fixed-Huffman block: a single literal 'A', then a
        // length-258/distance-1 back-reference (i.e. "repeat the last byte
        // 258 more times"), then end-of-block. Values per RFC 1951 SS3.2.6
        // and this crate's own `tables::length_to_code`/`distance_to_code`:
        //   - literal 'A' (0x41): symbols 0-143 use 8-bit codes 0x30-0xBF,
        //     so code = 0x30 + 0x41 = 0x71.
        //   - length 258 -> length code 285, 0 extra bits
        //     (`length_to_code(258) == (285, 0, 0)`). Code 285 falls in the
        //     280-287 range (8-bit codes 0xC0-0xC7): code = 0xC0 + 5 = 0xC5.
        //   - distance 1 -> distance code 0, 0 extra bits
        //     (`distance_to_code(1) == (0, 0, 0)`). Fixed distance codes
        //     are plain 5-bit values; code 0 is all-zero bits.
        //   - end-of-block: symbol 256 uses a 7-bit code, 0x00.
        let mut compressed = Vec::new();
        {
            let mut writer = BitWriter::new(&mut compressed);

            writer.write_bit(true)?; // BFINAL = 1 (only block)
            writer.write_bits(0b01, 2)?; // BTYPE = 01 (fixed Huffman)

            write_code_msb_first(&mut writer, 0x71, 8)?; // literal 'A'
            write_code_msb_first(&mut writer, 0xC5, 8)?; // length code 285
            write_code_msb_first(&mut writer, 0x00, 5)?; // distance code 0
            write_code_msb_first(&mut writer, 0x00, 7)?; // end-of-block

            writer.flush()?;
        }

        let result = inflate(&compressed)
            .expect("inflate of hand-built length-258 match stream should succeed");

        // One literal 'A' plus a length-258/distance-1 match: the match
        // repeats the immediately preceding byte 258 times, so the full
        // output is 259 copies of 'A'.
        let expected = vec![b'A'; 259];
        assert_eq!(result.len(), 259);
        assert_eq!(result, expected);

        Ok(())
    }

    // Note: fixed-Huffman decoder coverage (including the length-258
    // maximum-match case) is exercised above via a hand-built bitstream;
    // dynamic-Huffman blocks are still only covered indirectly through
    // encode-then-decode roundtrips elsewhere in this crate.

    #[test]
    fn test_try_decompress_sync_unit_roundtrip() {
        use crate::deflate::Deflater;
        let mut d = Deflater::new(6);
        let mut inflater = Inflater::new();

        let plain1 = b"Hello, IMAP COMPRESS=DEFLATE!";
        let plain2 = b"Second chunk, back-refs possible.";

        let mut comp1 = Vec::new();
        d.deflate_sync(plain1, &mut comp1)
            .expect("deflate_sync of first chunk should succeed");
        let mut comp2 = Vec::new();
        d.deflate_sync(plain2, &mut comp2)
            .expect("deflate_sync of second chunk should succeed");

        let (dec1, consumed1) = inflater
            .try_decompress_sync_unit(&comp1)
            .expect("try_decompress_sync_unit should not error on valid input")
            .expect("complete sync unit should be decompressed");
        assert_eq!(dec1, plain1);
        assert_eq!(consumed1, comp1.len());

        let (dec2, consumed2) = inflater
            .try_decompress_sync_unit(&comp2)
            .expect("try_decompress_sync_unit should not error on valid second chunk")
            .expect("complete second sync unit should be decompressed");
        assert_eq!(dec2, plain2);
        assert_eq!(consumed2, comp2.len());
    }

    #[test]
    fn test_try_decompress_sync_unit_needs_more_data() {
        use crate::deflate::Deflater;
        let mut d = Deflater::new(6);
        let mut inflater = Inflater::new();

        let plain = b"ABCDEFGHIJ";
        let mut comp = Vec::new();
        d.deflate_sync(plain, &mut comp)
            .expect("deflate_sync should succeed");

        // Feed only a partial chunk — should return None (needs more data)
        // and leave inflater state unchanged.
        let partial = &comp[..comp.len() / 2];
        let result = inflater
            .try_decompress_sync_unit(partial)
            .expect("try_decompress_sync_unit should not error on partial valid input");
        assert!(result.is_none(), "expected None for partial chunk");

        // Now feed the full chunk — should succeed.
        let (dec, consumed) = inflater
            .try_decompress_sync_unit(&comp)
            .expect("try_decompress_sync_unit should not error on full valid input")
            .expect("full sync unit should be decompressed");
        assert_eq!(dec, plain);
        assert_eq!(consumed, comp.len());
    }

    #[test]
    fn test_decompress_sync_chunk_lz77_preserved() {
        use crate::deflate::Deflater;
        let mut d = Deflater::new(6);
        let mut inflater = Inflater::new();

        let plain1 = b"ABCDEFGH";
        let plain2 = b"ABCDEFGHABCDEFGH"; // back-references into first chunk

        let mut c1 = Vec::new();
        d.deflate_sync(plain1, &mut c1)
            .expect("deflate_sync of first lz77 chunk should succeed");
        let mut c2 = Vec::new();
        d.deflate_sync(plain2, &mut c2)
            .expect("deflate_sync of second lz77 chunk should succeed");

        assert_eq!(
            inflater
                .decompress_sync_chunk(&c1)
                .expect("decompress_sync_chunk of first chunk should succeed"),
            plain1
        );
        assert_eq!(
            inflater
                .decompress_sync_chunk(&c2)
                .expect("decompress_sync_chunk of second chunk with back-refs should succeed"),
            plain2
        );
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Block decoder
//
// The block decoding routines are free functions generic over
// [`DecodeSink`] so that the growable-`Vec` path ([`Inflater`]) and the
// decompress-into-a-slice path ([`inflate_into`]) share one implementation —
// in particular one copy of the symbol loop, which is the only place where a
// subtle decoding bug could hide.
// ─────────────────────────────────────────────────────────────────────────

/// Per-stream block flags carried across [`inflate_block_into`] calls.
#[derive(Debug, Default, Clone, Copy)]
struct BlockState {
    /// Set once the block just decoded had `BFINAL = 1`.
    final_block: bool,
    /// Set when the block just decoded was an empty stored block (a
    /// sync flush).
    last_empty_stored: bool,
}

/// Decompress one DEFLATE block into `sink`.
fn inflate_block_into<R: Read, S: DecodeSink>(
    reader: &mut BitReader<R>,
    sink: &mut S,
    state: &mut BlockState,
) -> Result<()> {
    // Read block header
    let bfinal = reader.read_bit()?;
    let btype = reader.read_bits(2)?;

    state.final_block = bfinal;

    match btype {
        0 => inflate_stored_into(reader, sink, state),
        1 => {
            state.last_empty_stored = false;
            let litlen_tree = fixed_litlen_tree()?;
            let dist_tree = fixed_distance_tree()?;
            inflate_huffman_into(reader, sink, litlen_tree, dist_tree)
        }
        2 => {
            state.last_empty_stored = false;
            inflate_dynamic_into(reader, sink)
        }
        3 => Err(OxiArcError::invalid_header("Reserved block type 3")),
        _ => Err(OxiArcError::invalid_header("Invalid block type")),
    }
}

/// Decompress a stored (uncompressed) block.
fn inflate_stored_into<R: Read, S: DecodeSink>(
    reader: &mut BitReader<R>,
    sink: &mut S,
    state: &mut BlockState,
) -> Result<()> {
    // Align to byte boundary
    reader.align_to_byte();

    // Read LEN and NLEN
    let len = reader.read_bits(16)? as u16;
    let nlen = reader.read_bits(16)? as u16;

    // Validate
    if len != !nlen {
        return Err(OxiArcError::corrupted(
            reader.bit_position() / 8,
            format!("LEN/NLEN mismatch: {} vs {}", len, !nlen),
        ));
    }

    // Detect sync-flush: empty stored block (LEN=0).
    state.last_empty_stored = len == 0;

    // Copy bytes
    let mut buf = vec![0u8; len as usize];
    reader.read_bytes(&mut buf)?;
    sink.write_literals(&buf)
}

/// Decompress a block with dynamic Huffman codes.
fn inflate_dynamic_into<R: Read, S: DecodeSink>(
    reader: &mut BitReader<R>,
    sink: &mut S,
) -> Result<()> {
    // Read code counts
    let hlit = reader.read_bits(5)? as usize + 257; // literal/length codes
    let hdist = reader.read_bits(5)? as usize + 1; // distance codes
    let hclen = reader.read_bits(4)? as usize + 4; // code length codes

    // Read code length code lengths
    let mut code_length_lengths = [0u8; 19];
    for i in 0..hclen {
        let order = CODE_LENGTH_ORDER.get(i).copied().unwrap_or(0);
        let bits = reader.read_bits(3)? as u8;
        if let Some(slot) = code_length_lengths.get_mut(order) {
            *slot = bits;
        }
    }

    // Build code length tree. The code-length (19-symbol) alphabet MUST be
    // a complete Huffman code per RFC 1951 §3.2.7, so use the strict
    // constructor that rejects an incomplete set (as zlib does). This is
    // the exact class of corruption a buggy dynamic-Huffman encoder produces
    // and it must not be silently accepted.
    let code_length_tree = HuffmanTree::from_code_length_code(&code_length_lengths)?;

    // Read literal/length and distance code lengths
    let mut all_lengths = vec![0u8; hlit + hdist];
    let mut i = 0;

    while i < all_lengths.len() {
        let code = code_length_tree.decode(reader)?;

        match code {
            0..=15 => {
                if let Some(slot) = all_lengths.get_mut(i) {
                    *slot = code as u8;
                }
                i += 1;
            }
            16 => {
                // Copy previous length 3-6 times
                if i == 0 {
                    return Err(OxiArcError::corrupted(
                        reader.bit_position() / 8,
                        "Code 16 at start of lengths",
                    ));
                }
                let repeat = reader.read_bits(2)? as usize + 3;
                let prev = all_lengths.get(i - 1).copied().unwrap_or(0);
                if i + repeat > all_lengths.len() {
                    return Err(OxiArcError::corrupted(
                        reader.bit_position() / 8,
                        "Code length overflow",
                    ));
                }
                for _ in 0..repeat {
                    if let Some(slot) = all_lengths.get_mut(i) {
                        *slot = prev;
                    }
                    i += 1;
                }
            }
            17 | 18 => {
                // Repeat 0 for 3-10 (code 17) or 11-138 (code 18) times
                let repeat = if code == 17 {
                    reader.read_bits(3)? as usize + 3
                } else {
                    reader.read_bits(7)? as usize + 11
                };
                if i + repeat > all_lengths.len() {
                    return Err(OxiArcError::corrupted(
                        reader.bit_position() / 8,
                        "Code length overflow",
                    ));
                }
                for _ in 0..repeat {
                    if let Some(slot) = all_lengths.get_mut(i) {
                        *slot = 0;
                    }
                    i += 1;
                }
            }
            _ => {
                return Err(OxiArcError::invalid_huffman(reader.bit_position()));
            }
        }
    }

    // Split into literal/length and distance lengths
    let (litlen_lengths, dist_lengths) = all_lengths.split_at(hlit);

    // Build trees
    let litlen_tree = HuffmanTree::from_code_lengths(litlen_lengths)?;
    let dist_tree = HuffmanTree::from_code_lengths(dist_lengths)?;

    inflate_huffman_into(reader, sink, &litlen_tree, &dist_tree)
}

/// Decompress a Huffman-coded block.
///
/// The bit accumulator is detached into a [`BitCache`] local so the compiler
/// can keep it in registers: the dependency chain
/// `accumulator -> table lookup -> shift -> accumulator` runs once per
/// symbol, and forcing it through memory adds a store-to-load forwarding
/// stall to every single one. The accumulator is refilled once per symbol
/// group rather than once per symbol — a worst-case length/distance pair
/// needs 15 + 5 + 15 + 13 = 48 bits, which always fits in the >= 56 bits a
/// refill provides.
///
/// Table entries are consumed as packed `u32`s so the innermost path never
/// materialises the crate's 48-byte `Result`. Whenever fewer bits are
/// buffered than a code might need — end of stream, or an exact-mode reader
/// where speculative refills are deliberately disabled — the cache is handed
/// back and [`HuffmanTree::decode`] takes over, reading exactly what it
/// needs and reporting errors precisely.
fn inflate_huffman_into<R: Read, S: DecodeSink>(
    reader: &mut BitReader<R>,
    sink: &mut S,
    litlen_tree: &HuffmanTree,
    dist_tree: &HuffmanTree,
) -> Result<()> {
    let cache = reader.detach();
    // The cache is handed over and back **by value**: taking `&mut BitCache`
    // across a non-inlined call would let its address escape, and the
    // compiler would then have to keep the accumulator in memory for the
    // whole loop.
    let (result, cache) = inflate_huffman_cached(reader, cache, sink, litlen_tree, dist_tree);
    reader.reattach(cache);
    result
}

fn inflate_huffman_cached<R: Read, S: DecodeSink>(
    reader: &mut BitReader<R>,
    mut cache: BitCache,
    sink: &mut S,
    litlen_tree: &HuffmanTree,
    dist_tree: &HuffmanTree,
) -> (Result<()>, BitCache) {
    // Widest single code either tree can produce.
    let litlen_max = litlen_tree.max_code_length();
    // A length/distance tail needs at most 5 length-extra + 15 distance +
    // 13 distance-extra bits.
    let match_tail_bits = 5 + dist_tree.max_code_length() + 13;

    loop {
        if let Err(e) = reader.refill_cache(&mut cache, litlen_max) {
            return (Err(e), cache);
        }

        // ── literal / length symbol ─────────────────────────────────────
        let entry = litlen_tree.lookup_cached(&cache);
        let bits = HuffmanTree::entry_length(entry);
        let code = if bits != 0 && bits <= cache.available() {
            cache.consume(bits);
            HuffmanTree::entry_symbol(entry)
        } else {
            match decode_detached(reader, cache, litlen_tree) {
                Ok((symbol, c)) => {
                    cache = c;
                    symbol
                }
                Err((e, c)) => return (Err(e), c),
            }
        };

        if code < 256 {
            // Literal byte
            if let Err(e) = sink.write_literal(code as u8) {
                return (Err(e), cache);
            }
            continue;
        }
        if code == 256 {
            // End of block
            break;
        }
        if code > 285 {
            let at = (reader.bit_position() + cache.consumed()) / 8;
            return (
                Err(OxiArcError::corrupted(
                    at,
                    format!("Invalid literal/length code: {}", code),
                )),
                cache,
            );
        }

        // Top up once for the whole length/distance tail.
        if let Err(e) = reader.refill_cache(&mut cache, match_tail_bits) {
            return (Err(e), cache);
        }

        // ── length extra bits ───────────────────────────────────────────
        let length_idx = (code - 257) as usize;
        let extra_bits = LENGTH_EXTRA_BITS
            .get(length_idx)
            .copied()
            .unwrap_or_default();
        let extra = match read_extra(reader, &mut cache, extra_bits) {
            Ok(v) => v,
            Err(e) => return (Err(e), cache),
        };
        let length = decode_length(code, extra);

        // ── distance symbol ─────────────────────────────────────────────
        let dist_entry = dist_tree.lookup_cached(&cache);
        let dist_bits = HuffmanTree::entry_length(dist_entry);
        let dist_code = if dist_bits != 0 && dist_bits <= cache.available() {
            cache.consume(dist_bits);
            HuffmanTree::entry_symbol(dist_entry)
        } else {
            match decode_detached(reader, cache, dist_tree) {
                Ok((symbol, c)) => {
                    cache = c;
                    symbol
                }
                Err((e, c)) => return (Err(e), c),
            }
        };
        if dist_code >= 30 {
            let at = (reader.bit_position() + cache.consumed()) / 8;
            return (
                Err(OxiArcError::corrupted(
                    at,
                    format!("Invalid distance code: {}", dist_code),
                )),
                cache,
            );
        }

        // ── distance extra bits ─────────────────────────────────────────
        let dist_extra_bits = DISTANCE_EXTRA_BITS
            .get(dist_code as usize)
            .copied()
            .unwrap_or_default();
        let dist_extra = match read_extra(reader, &mut cache, dist_extra_bits) {
            Ok(v) => v,
            Err(e) => return (Err(e), cache),
        };
        let distance = decode_distance(dist_code, dist_extra);

        // Copy from history
        if let Err(e) = sink.copy_match(distance as usize, length as usize) {
            return (Err(e), cache);
        }
    }

    (Ok(()), cache)
}

/// Read `count` extra bits, falling back to the reader when the detached
/// cache does not hold enough (end of stream, or exact mode).
#[inline(always)]
fn read_extra<R: Read>(reader: &mut BitReader<R>, cache: &mut BitCache, count: u8) -> Result<u16> {
    if count <= cache.available() {
        let value = cache.peek_bits(count);
        cache.consume(count);
        return Ok(value as u16);
    }
    reader.reattach(*cache);
    let value = reader.read_bits(count);
    *cache = reader.detach();
    Ok(value? as u16)
}

/// Decode one symbol through the general (cache-free) path.
///
/// Used when too few bits are buffered for the table lookup to be trusted;
/// the cache is reattached first so `decode` sees the true bit position.
#[inline]
fn decode_detached<R: Read>(
    reader: &mut BitReader<R>,
    cache: BitCache,
    tree: &HuffmanTree,
) -> core::result::Result<(u16, BitCache), (OxiArcError, BitCache)> {
    reader.reattach(cache);
    let symbol = tree.decode(reader);
    let cache = reader.detach();
    match symbol {
        Ok(symbol) => Ok((symbol, cache)),
        Err(e) => Err((e, cache)),
    }
}

/// Decompress a raw DEFLATE stream straight into a caller-supplied buffer.
///
/// No output `Vec` is allocated, no capacity is guessed and nothing is
/// re-grown or copied: literals and matches are written straight into `dst`,
/// and `dst` itself is the history window, so there is no per-call window
/// update either. That makes it the entry point to reach for when the
/// decompressed size is known in advance (a GeoTIFF tile, a PNG scanline
/// block, a fixed-size record).
///
/// The saving is in the allocation, not in the symbol loop: this runs the
/// same decoder as [`inflate`], with a bounds check per write in place of
/// [`Vec`] growth. Measured interleaved on 100 KiB corpora it is a few per
/// cent quicker than [`inflate`] on compressible input and indistinguishable
/// on incompressible input — so choose between them by whether the size is
/// known, not by expecting a different class of speed.
///
/// # Returns
///
/// The number of bytes written to `dst`.
///
/// # Errors
///
/// * [`OxiArcError::BufferTooSmall`] if the stream decodes to more than
///   `dst.len()` bytes — the excess is never written and never silently
///   dropped.
/// * [`OxiArcError::InvalidDistance`] if a back-reference reaches behind the
///   start of `dst`. Because no history precedes `dst`, a preset dictionary
///   is not supported here; use [`Inflater::with_dictionary`] for that.
/// * The usual header/EOF/Huffman errors for a corrupt stream.
///
/// Nothing about `dst` is trusted to the input: a hostile stream can neither
/// over-allocate (nothing is allocated), nor write outside `dst`, nor loop
/// forever (every block consumes input and every match is bounded).
///
/// # Example
///
/// ```
/// use oxiarc_deflate::{deflate, inflate_into};
///
/// let original = b"Hello, World! Hello, World!";
/// let compressed = deflate(original, 6).expect("deflate");
///
/// let mut out = vec![0u8; original.len()];
/// let n = inflate_into(&compressed, &mut out).expect("inflate_into");
/// assert_eq!(&out[..n], original);
/// ```
pub fn inflate_into(src: &[u8], dst: &mut [u8]) -> Result<usize> {
    let capacity = dst.len();
    let mut stream = InflateStream::new();
    let mut sink = BoundedSink::new(dst, None);
    let (_consumed, status) = stream.inflate_sink(src, &mut sink, FlushMode::Finish)?;
    match status {
        InflateStatus::StreamEnd => Ok(sink.written()),
        // The sink filled before the stream ended: report what it would
        // have taken rather than truncating.
        InflateStatus::NeedOutput => Err(OxiArcError::buffer_too_small(
            sink.written().saturating_add(1),
            capacity,
        )),
        // Unreachable under `Finish`, which raises a short stream itself.
        _ => Err(OxiArcError::unexpected_eof(1)),
    }
}
