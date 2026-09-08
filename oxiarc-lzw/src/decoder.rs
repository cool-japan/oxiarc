//! LZW decoder (decompression).
//!
//! One decode loop ([`decode_into_sink`]) serves every dialect this crate
//! implements. It walks the shared prefix/suffix code table
//! ([`crate::dictionary::LzwDictionary`]) and expands each code directly
//! into the output, so nothing is allocated per decoded code.
//!
//! # Shape of the loop
//!
//! The loop follows libtiff's `LZWDecode`, because that is the fastest
//! production LZW decoder available to compare against and its structure is
//! what makes it fast:
//!
//! * the new table entry is stored **before** the current code is emitted,
//!   with the entry's own `first` byte used as its suffix when the code
//!   being read *is* the entry being created — one comparison in place of a
//!   separate KwKwK branch;
//! * each entry carries its string's `length`, so the number of output
//!   bytes is known before a single one is written and the run can be
//!   written backwards from its end with no scratch buffer;
//! * each entry carries a `repeated` flag (all bytes equal), so the runs a
//!   flat image region produces are emitted with a fill instead of a chain
//!   walk;
//! * one-byte and two-byte strings are written from the entry's `first` and
//!   `suffix` fields without touching the chain at all — the dominant case
//!   for incompressible data, where LZW emits about one code per byte.
//!
//! Two output sinks share the loop: a growable [`Vec<u8>`] bounded by the
//! caller's `expected_size` ([`LzwDecoder::decode`]) and a caller-supplied
//! `&mut [u8]` ([`crate::decompress_tiff_into`]), which allocates nothing
//! at all.

use crate::bits::{CodeOrder, LsbCodes, MsbCodes};
use crate::config::{LzwBitOrder, LzwConfig};
use crate::dictionary::{CodeEntry, LzwDictionary, learn_entry, write_chain};
use crate::error::{LzwError, Result};

/// Size of the first output block [`VecSink`] zero-fills (8 KiB).
///
/// `expected_size` may come from untrusted framing, so pre-reserving it
/// verbatim lets tiny malicious inputs force enormous allocations
/// (resource-exhaustion DoS). The sink instead zero-fills a block at a time,
/// doubling as it goes and never past the decode limit, so allocation stays
/// proportional to bytes actually decoded — and one `memset` covers
/// thousands of codes instead of one `Vec::resize` per code.
const INITIAL_BLOCK: usize = 8 * 1024;

/// Sentinel for "no previous code" (just after a table reset).
///
/// A `u32` sentinel rather than `Option<u16>`: `u16` has no niche, so the
/// `Option` would cost the same four bytes plus a discriminant test, and
/// the comparison against a sentinel is what the loop needs anyway.
const NO_PREV: u32 = u32::MAX;

/// Destination for decoded bytes.
///
/// Every method may assume the caller has already checked that
/// [`LzwSink::space`] is large enough; the decode loop tracks the remaining
/// space itself so that the sink is not re-queried per code.
pub(crate) trait LzwSink {
    /// Bytes that may still be written before decoding must stop.
    fn space(&self) -> usize;

    /// Append one byte. `space() >= 1`.
    fn push_byte(&mut self, byte: u8);

    /// Append `len` copies of `byte`. `space() >= len`.
    fn fill(&mut self, byte: u8, len: usize);

    /// Reserve exactly `len` bytes (`len <= space()`) and return them for
    /// the caller to fill.
    fn reserve(&mut self, len: usize) -> &mut [u8];

    /// Bytes written so far.
    fn written(&self) -> usize;
}

/// Sink that appends to a `Vec<u8>` until `limit` bytes have been produced.
///
/// The vector is grown in zero-filled blocks and truncated to
/// [`VecSink::written`] when the decode finishes, so the per-code cost is a
/// single length comparison rather than a `resize` call.
pub(crate) struct VecSink<'a> {
    out: &'a mut Vec<u8>,
    written: usize,
    /// Mirror of `out.len()`, so the per-code capacity check compares two
    /// locals instead of loading the vector's length from memory.
    filled: usize,
    limit: usize,
}

impl<'a> VecSink<'a> {
    /// Wrap `out` (which must be empty) with a decode limit of `limit`.
    pub(crate) fn new(out: &'a mut Vec<u8>, limit: usize) -> Self {
        let filled = out.len();
        Self {
            out,
            written: 0,
            filled,
            limit,
        }
    }

    /// Make sure `len` more bytes are addressable.
    #[inline(always)]
    fn ensure(&mut self, len: usize) {
        if self.written + len > self.filled {
            self.grow(len);
        }
    }

    /// Zero-fill a fresh block. Cold: once per block, not once per code.
    #[cold]
    fn grow(&mut self, len: usize) {
        let need = self.written + len;
        let target = need
            .max(self.filled.saturating_mul(2))
            .max(INITIAL_BLOCK)
            .min(self.limit)
            .max(need);
        self.out.resize(target, 0);
        self.filled = self.out.len();
    }
}

impl LzwSink for VecSink<'_> {
    #[inline(always)]
    fn space(&self) -> usize {
        self.limit.saturating_sub(self.written)
    }

    #[inline(always)]
    fn push_byte(&mut self, byte: u8) {
        self.ensure(1);
        if let Some(slot) = self.out.get_mut(self.written) {
            *slot = byte;
            self.written += 1;
        }
    }

    #[inline(always)]
    fn fill(&mut self, byte: u8, len: usize) {
        self.ensure(len);
        let start = self.written;
        if let Some(run) = self.out.get_mut(start..start + len) {
            run.fill(byte);
            self.written += len;
        }
    }

    #[inline(always)]
    fn reserve(&mut self, len: usize) -> &mut [u8] {
        self.ensure(len);
        let start = self.written;
        self.written += len;
        &mut self.out[start..start + len]
    }

    #[inline(always)]
    fn written(&self) -> usize {
        self.written
    }
}

/// Sink that fills a caller-supplied slice, allocating nothing.
pub(crate) struct SliceSink<'a> {
    dst: &'a mut [u8],
    written: usize,
}

impl<'a> SliceSink<'a> {
    /// Wrap `dst`; decoding stops once it is full.
    pub(crate) fn new(dst: &'a mut [u8]) -> Self {
        Self { dst, written: 0 }
    }
}

impl LzwSink for SliceSink<'_> {
    #[inline(always)]
    fn space(&self) -> usize {
        self.dst.len() - self.written
    }

    #[inline(always)]
    fn push_byte(&mut self, byte: u8) {
        if let Some(slot) = self.dst.get_mut(self.written) {
            *slot = byte;
            self.written += 1;
        }
    }

    #[inline(always)]
    fn fill(&mut self, byte: u8, len: usize) {
        let start = self.written;
        if let Some(run) = self.dst.get_mut(start..start + len) {
            run.fill(byte);
            self.written += len;
        }
    }

    #[inline(always)]
    fn reserve(&mut self, len: usize) -> &mut [u8] {
        let start = self.written;
        self.written += len;
        &mut self.dst[start..start + len]
    }

    #[inline(always)]
    fn written(&self) -> usize {
        self.written
    }
}

/// The `next_code` value at which the code width grows, or `u32::MAX` when
/// `width` has already reached `max_width`.
///
/// The decoder adds a table entry one iteration later than the encoder, so
/// its `next_code` is always one behind and it must widen one code sooner
/// to stay in step: when the encoder adds entry 511 its `next_code` becomes
/// 512 and it widens to 10 bits while the decoder is still at 511. The
/// decoder threshold is therefore the encoder's minus one — `2^width - 1`
/// with TIFF's early change and `2^width` with the standard (late) rule.
#[inline(always)]
fn grow_threshold(width: u32, max_width: u32, early_change: bool) -> u32 {
    if width >= max_width {
        return u32::MAX;
    }
    // `width < max_width <= 16`, so the shift cannot overflow.
    if early_change {
        (1u32 << width) - 1
    } else {
        1u32 << width
    }
}

/// Emit the string of an already-loaded table entry, clipped to `space`
/// bytes, and return how many bytes were written.
///
/// Writing fewer bytes than the string holds keeps its **leading** bytes,
/// which is what a decoder does when the last code of a strip expands past
/// the end of the caller's buffer (libtiff's `LZWDecode` does the same).
///
/// `entry` is passed in rather than looked up because the decode loop has
/// already loaded it (and, for the KwKwK case, has just built it), so the
/// three fast paths below touch the code table zero times.
#[inline(always)]
fn emit_entry<S: LzwSink>(
    entries: &[CodeEntry],
    entry: CodeEntry,
    sink: &mut S,
    space: usize,
) -> usize {
    let full = usize::from(entry.length());
    if full == 1 {
        // Single byte: the overwhelmingly common case for data LZW cannot
        // compress, where almost every code is a root.
        sink.push_byte(entry.suffix());
        return 1;
    }
    let want = full.min(space);
    if want == 0 {
        return 0;
    }
    if entry.repeated() {
        // Every byte of the string is the same, so a clipped run is still
        // just a shorter run.
        sink.fill(entry.suffix(), want);
    } else if want == 1 {
        sink.push_byte(entry.first());
    } else if want == 2 && full == 2 {
        // Two bytes are the whole string: `first` then `suffix`, no walk.
        let out = sink.reserve(2);
        if let Some(pair) = out.first_chunk_mut::<2>() {
            pair[0] = entry.first();
            pair[1] = entry.suffix();
        }
    } else {
        let out = sink.reserve(want);
        write_chain(entries, entry, full, out);
    }
    want
}

/// Core LZW decode loop, shared by every entry point.
///
/// Decoding stops when the sink is full, when the EOI code is read, or with
/// an error. `EOF_IS_ERROR` selects what running out of input means while
/// the sink still has space: an error for framed dialects such as a TIFF
/// strip ([`LzwError::UnexpectedEof`] — a truncated stream is never
/// reported as success), and a normal stop for GIF image data, which simply
/// ends when its sub-blocks do.
///
/// Everything the loop touches per code — the bit position, the code width
/// and its mask, the allocation cursor, the remaining output space and the
/// previous code's entry — is a local for the duration of the run and is
/// written back to the table once at the end. That leaves one table load
/// and one table store per code and no other memory traffic besides the
/// input window and the output. See [`grow_threshold`] for the code-width
/// synchronisation rule.
///
/// `CLEAR_ALLOWED` mirrors `LzwConfig::use_clear_code` as a constant for
/// the same class of reason: as a runtime field it stayed live inside the
/// loop and the optimiser turned it into a per-code branch at the loop head
/// (`ldur`/`cbz` in the emitted arm64) even though it is only read on the
/// rare reserved-code path.
///
/// The sink is taken **by value** for the same reason: behind a `&mut` the
/// optimiser wrote the output cursor back to the sink's struct on every
/// code (again visible in the emitted arm64 as a reload of the sink pointer
/// plus a store). Owned, its fields are scalars the register allocator can
/// keep. Returns the number of bytes written.
// Never inlined into `LzwDecoder::run`: with both bit orders inlined into
// one caller the optimiser merges the two loop bodies and reintroduces a
// per-code branch on the packing (seen in the emitted arm64 as a `tbz` at
// the loop head, and worth ~6 % of throughput).
#[inline(never)]
pub(crate) fn decode_into_sink<
    const EOF_IS_ERROR: bool,
    const CLEAR_ALLOWED: bool,
    O: CodeOrder,
    S: LzwSink,
>(
    dict: &mut LzwDictionary,
    data: &[u8],
    mut sink: S,
) -> Result<usize> {
    dict.reset();
    let (entries, saved, limits) = dict.parts();
    let slots = entries.len();
    if slots == 0 || limits.min_bits == 0 || limits.max_bits > LzwConfig::MAX_SUPPORTED_BITS {
        return Err(LzwError::InvalidBitWidth(limits.max_bits));
    }
    // `code & slot_mask <= slot_mask == slots - 1 < slots` for every code,
    // so a masked index is always in bounds — and the optimiser can prove
    // it, which is what keeps the per-output-byte chain walk free of bounds
    // checks. Codes above the table are rejected before any access anyway.
    // `slots == max_code + 1`, so the slot mask *is* the largest assignable
    // code: one loop-invariant register does both jobs.
    let slot_mask = slots - 1;
    let max_code = slot_mask as u32;
    let total_bits = (data.len() as u64) << 3;
    let first_code = u32::from(limits.first_code);
    let max_width = u32::from(limits.max_bits);
    let min_width = u32::from(limits.min_bits).clamp(1, 16);
    // The two reserved codes are adjacent — `LzwConfig::eoi_code` is
    // `clear_code + 1` by construction — so one unsigned compare separates
    // "ordinary code" from "ClearCode or EOI". Two compares per code, in
    // the hottest place there is, become one. The invariant is checked here
    // rather than assumed, because the loop below depends on it.
    if limits.eoi_code != limits.clear_code.wrapping_add(1) {
        return Err(LzwError::InvalidBitWidth(limits.min_bits));
    }

    let mut next_code = first_code;
    let mut width = min_width;
    let mut code_mask = (1u32 << width) - 1;
    // `next_code` value at which the code width must grow, or `u32::MAX`
    // once the width has reached its ceiling. Precomputing it keeps the
    // per-code width bookkeeping down to one compare, with the shift and
    // the early-change rule evaluated only on the (rare) growth itself.
    let mut grow_at = grow_threshold(width, max_width, limits.early_change);
    let mut bit = 0u64;
    let mut space = sink.space();
    let mut prev = NO_PREV;
    let mut parent = CodeEntry::default();
    let mut failure: Option<LzwError> = None;

    while space > 0 {
        if bit + u64::from(width) > total_bits {
            if EOF_IS_ERROR {
                failure = Some(LzwError::UnexpectedEof { position: bit });
            }
            break;
        }
        let window = O::window(data, (bit >> 3) as usize);
        let code = O::extract(window, (bit & 7) as u32, width, code_mask);
        bit += u64::from(width);

        if code.wrapping_sub(limits.clear_code) <= 1 {
            if code == limits.eoi_code {
                break;
            }
            if !CLEAR_ALLOWED {
                failure = Some(LzwError::InvalidClearCode { position: bit });
                break;
            }
            // TIFF 6.0 mandates a ClearCode at the start of every strip and
            // again whenever the encoder's table reaches entry 4094;
            // encoders may also emit one at any other point (libtiff's
            // compression-ratio checkpoint resets), so accept it anywhere.
            width = min_width;
            code_mask = (1u32 << width) - 1;
            grow_at = grow_threshold(width, max_width, limits.early_change);
            next_code = first_code;
            prev = NO_PREV;
            parent = CodeEntry::default();
            continue;
        }

        let mut entry = entries[code as usize & slot_mask];

        if u32::from(code) < next_code {
            // Ordinary case: the entry to emit is already in the table, and
            // the entry to create is `string(prev) ++ first(code)`. The
            // first code after a reset has nothing to extend, and a full
            // table has nowhere to put it.
            if prev != NO_PREV && next_code <= max_code {
                let created = learn_entry(prev as u16, parent, entry.first());
                entries[next_code as usize & slot_mask] = created;
                next_code += 1;
                if next_code >= grow_at {
                    width += 1;
                    code_mask = (code_mask << 1) | 1;
                    grow_at = grow_threshold(width, max_width, limits.early_change);
                }
            }
        } else if u32::from(code) == next_code && prev != NO_PREV && next_code <= max_code {
            // KwKwK: the code being read *is* the entry about to be
            // created, so create it first — its last byte is the first byte
            // of the previous string — and then emit it.
            let created = learn_entry(prev as u16, parent, parent.first());
            entries[next_code as usize & slot_mask] = created;
            entry = created;
            next_code += 1;
            if next_code >= grow_at {
                width += 1;
                code_mask = (code_mask << 1) | 1;
                grow_at = grow_threshold(width, max_width, limits.early_change);
            }
        } else {
            // Past the end of the table, or a KwKwK the full table can no
            // longer serve, or the first code after a reset naming an entry
            // that does not exist yet.
            failure = Some(LzwError::InvalidCode(code));
            break;
        }

        space -= emit_entry(entries, entry, &mut sink, space);
        prev = u32::from(code);
        parent = entry;
    }

    saved.next_code = next_code;
    saved.current_bits = width as u8;
    match failure {
        Some(error) => Err(error),
        None => Ok(sink.written()),
    }
}

/// LZW decoder for decompression.
///
/// Constructing one allocates the code table, so a caller decoding many
/// strips of the same image should build a single decoder and call
/// [`LzwDecoder::decode_into`] per strip: every decode begins with a table
/// reset, which is O(1). `crate::decompress_tiff_into` is the one-shot
/// convenience wrapper and pays the table allocation per call.
#[derive(Debug)]
pub struct LzwDecoder {
    /// Code table for code lookup.
    dict: LzwDictionary,
}

impl LzwDecoder {
    /// Create a new LZW decoder with the given configuration.
    ///
    /// # Errors
    ///
    /// Returns [`LzwError::InvalidBitWidth`] when `config` fails
    /// [`LzwConfig::validate`].
    pub fn new(config: LzwConfig) -> Result<Self> {
        let dict = LzwDictionary::new(config)?;
        Ok(Self { dict })
    }

    /// Decode LZW-compressed data.
    ///
    /// Decoding runs until `expected_size` bytes have been produced, the EOI
    /// code is read, or the input is exhausted. A stream that ends without
    /// EOI before `expected_size` bytes were produced is an error; a stream
    /// that terminates early *with* EOI returns the shorter output.
    ///
    /// # Parameters
    ///
    /// - `input`: LZW-compressed data
    /// - `expected_size`: Expected size of decompressed output
    ///
    /// # Returns
    ///
    /// Decompressed byte sequence of exactly `expected_size` bytes (or less
    /// if the EOI code is encountered early).
    ///
    /// # Errors
    ///
    /// Returns [`LzwError::UnexpectedEof`] for a truncated stream,
    /// [`LzwError::InvalidCode`] for a code outside the current table and
    /// [`LzwError::InvalidClearCode`] when a ClearCode appears in a
    /// configuration that does not use clear codes.
    pub fn decode(&mut self, input: &[u8], expected_size: usize) -> Result<Vec<u8>> {
        let mut output = Vec::new();
        let written = self.run::<true, _>(input, VecSink::new(&mut output, expected_size))?;
        output.truncate(written);
        Ok(output)
    }

    /// Decode LZW-compressed data directly into `dst`.
    ///
    /// Returns the number of bytes written. Decoding stops once `dst` is
    /// full; any remaining input is ignored, exactly as libtiff's
    /// `LZWDecode` does once the scanline buffer is satisfied.
    ///
    /// # Errors
    ///
    /// Same as [`LzwDecoder::decode`].
    pub fn decode_into(&mut self, input: &[u8], dst: &mut [u8]) -> Result<usize> {
        self.run::<true, _>(input, SliceSink::new(dst))
    }

    /// Run the decode loop with the code packing this configuration
    /// selects.
    fn run<const EOF_IS_ERROR: bool, S: LzwSink>(
        &mut self,
        input: &[u8],
        sink: S,
    ) -> Result<usize> {
        let config = *self.dict.config();
        match (config.bit_order, config.use_clear_code) {
            (LzwBitOrder::Msb, true) => {
                decode_into_sink::<EOF_IS_ERROR, true, MsbCodes, S>(&mut self.dict, input, sink)
            }
            (LzwBitOrder::Msb, false) => {
                decode_into_sink::<EOF_IS_ERROR, false, MsbCodes, S>(&mut self.dict, input, sink)
            }
            (LzwBitOrder::Lsb, true) => {
                decode_into_sink::<EOF_IS_ERROR, true, LsbCodes, S>(&mut self.dict, input, sink)
            }
            (LzwBitOrder::Lsb, false) => {
                decode_into_sink::<EOF_IS_ERROR, false, LsbCodes, S>(&mut self.dict, input, sink)
            }
        }
    }

    /// Reset the decoder to initial state.
    pub fn reset(&mut self) {
        self.dict.reset();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encoder::LzwEncoder;

    #[test]
    fn test_decode_simple() {
        // Manually create a simple LZW stream
        // This is "TOBEORNOTTOBEORTOBEORNOT" compressed
        let config = LzwConfig::TIFF;
        let mut decoder = LzwDecoder::new(config).expect("create lzw decoder simple");

        // For this test, we'll use the encoder to create valid data
        let original = b"TOBEORNOTTOBEORTOBEORNOT";
        let mut encoder = LzwEncoder::new(config).expect("create lzw encoder for decode test");
        let compressed = encoder
            .encode(original)
            .expect("lzw encode for decode test");

        // Now decode it
        let decompressed = decoder
            .decode(&compressed, original.len())
            .expect("lzw decode simple");

        assert_eq!(decompressed, original);
    }

    #[test]
    fn test_decode_310_bytes() {
        // THE CRITICAL TEST - this must not truncate!
        let config = LzwConfig::TIFF;
        let mut decoder = LzwDecoder::new(config).expect("create lzw decoder 310");

        let original = b"This is a test of compression! ".repeat(10);
        assert_eq!(original.len(), 310);

        // Encode it first
        let mut encoder = LzwEncoder::new(config).expect("create lzw encoder 310 decode test");
        let compressed = encoder
            .encode(&original)
            .expect("lzw encode 310 bytes for decode");

        // Decode it
        let decompressed = decoder
            .decode(&compressed, original.len())
            .expect("lzw decode 310 bytes");

        // CRITICAL: Must be 310 bytes, not ~250!
        assert_eq!(
            decompressed.len(),
            310,
            "Decompressed length must be 310, not truncated!"
        );
        assert_eq!(decompressed, &original[..]);
    }

    #[test]
    fn test_decode_repeating_pattern() {
        let config = LzwConfig::TIFF;
        let mut decoder = LzwDecoder::new(config).expect("create lzw decoder repeating pattern");

        let original = b"ABABABABABABABABAB";

        let mut encoder = LzwEncoder::new(config).expect("create lzw encoder repeating pattern");
        let compressed = encoder
            .encode(original)
            .expect("lzw encode repeating pattern");

        let decompressed = decoder
            .decode(&compressed, original.len())
            .expect("lzw decode repeating pattern");

        assert_eq!(decompressed, original);
    }

    #[test]
    fn test_decode_single_byte() {
        let config = LzwConfig::TIFF;
        let mut decoder = LzwDecoder::new(config).expect("create lzw decoder single byte");

        let original = b"A";

        let mut encoder = LzwEncoder::new(config).expect("create lzw encoder single byte decode");
        let compressed = encoder
            .encode(original)
            .expect("lzw encode single byte for decode");

        let decompressed = decoder
            .decode(&compressed, original.len())
            .expect("lzw decode single byte");

        assert_eq!(decompressed, original);
    }

    #[test]
    fn test_decode_all_same() {
        let config = LzwConfig::TIFF;
        let mut decoder = LzwDecoder::new(config).expect("create lzw decoder all same");

        let original = vec![b'X'; 500];

        let mut encoder = LzwEncoder::new(config).expect("create lzw encoder all same");
        let compressed = encoder
            .encode(&original)
            .expect("lzw encode all same bytes");

        let decompressed = decoder
            .decode(&compressed, original.len())
            .expect("lzw decode all same bytes");

        assert_eq!(decompressed, original);
    }

    #[test]
    fn test_decode_into_matches_decode_on_short_buffer() {
        // A buffer smaller than the strip forces the final code to be
        // expanded partially; both entry points must agree.
        let config = LzwConfig::TIFF;
        let original = b"ABABABABABABABABABABABABABABABABAB".repeat(7);
        let mut encoder = LzwEncoder::new(config).expect("create encoder");
        let compressed = encoder.encode(&original).expect("encode");

        for limit in 0..original.len() {
            let mut decoder = LzwDecoder::new(config).expect("create decoder");
            let via_vec = decoder.decode(&compressed, limit).expect("decode vec");
            let mut buffer = vec![0u8; limit];
            let mut decoder = LzwDecoder::new(config).expect("create decoder");
            let written = decoder
                .decode_into(&compressed, &mut buffer)
                .expect("decode into");
            assert_eq!(written, limit, "limit {limit}");
            assert_eq!(via_vec, buffer, "limit {limit}");
            assert_eq!(&buffer[..], &original[..limit], "limit {limit}");
        }
    }

    #[test]
    fn test_truncated_stream_is_an_error() {
        let config = LzwConfig::TIFF;
        let original = vec![b'Q'; 4096];
        let mut encoder = LzwEncoder::new(config).expect("create encoder");
        let compressed = encoder.encode(&original).expect("encode");

        for cut in 1..compressed.len() {
            let mut decoder = LzwDecoder::new(config).expect("create decoder");
            let result = decoder.decode(&compressed[..cut], original.len());
            if let Ok(ref decoded) = result {
                // A short read may only succeed when it is a genuine prefix
                // of the original data (EOI cannot appear early here).
                assert!(
                    decoded.len() == original.len(),
                    "cut {cut} silently produced {} bytes",
                    decoded.len()
                );
            }
        }
    }
}
