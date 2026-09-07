//! LZW decoder (decompression).
//!
//! The decoder walks the shared prefix/suffix table
//! ([`crate::dictionary::LzwDictionary`]) and expands every code directly
//! into the output buffer, so no allocation happens per decoded code. Two
//! output sinks share the same decode loop:
//!
//! * a growable [`Vec<u8>`] bounded by the caller's `expected_size`
//!   ([`LzwDecoder::decode`]), and
//! * a caller-supplied `&mut [u8]`
//!   ([`crate::decompress_tiff_into`]), which allocates nothing at all.

use crate::bits::LzwCodeReader;
use crate::bitstream_lsb::LsbBitReader;
use crate::bitstream_msb::MsbBitReader;
use crate::config::{LzwBitOrder, LzwConfig};
use crate::dictionary::LzwDictionary;
use crate::error::{LzwError, Result};

/// Upper bound on the output capacity reserved up-front in
/// [`LzwDecoder::decode`] (64 KiB).
///
/// `expected_size` may come from untrusted framing, so pre-reserving it
/// verbatim lets tiny malicious inputs force enormous allocations
/// (resource-exhaustion DoS). Reserving at most this much and letting the
/// `Vec` grow geometrically keeps allocation proportional to bytes actually
/// decoded while still avoiding realloc churn for typical TIFF strips.
const MAX_INITIAL_CAPACITY: usize = 64 * 1024;

/// Destination for decoded bytes.
///
/// The sink also carries the decode limit: the loop stops as soon as
/// [`LzwSink::space`] reaches zero, which is what makes both a bounded
/// `Vec` and a fixed slice work with one decode loop.
pub(crate) trait LzwSink {
    /// Bytes that may still be written before decoding must stop.
    fn space(&self) -> usize;

    /// Absolute offset of the next byte to be written, counted from the
    /// first byte this decode produced.
    fn position(&self) -> usize;

    /// Reserve exactly `len` bytes (`len <= self.space()`) and return them
    /// for the caller to fill.
    fn reserve(&mut self, len: usize) -> &mut [u8];

    /// Append `len` bytes copied from absolute offset `src` of the output
    /// produced so far. The caller guarantees `src + len <= position()`, so
    /// source and destination never overlap.
    fn copy_earlier(&mut self, src: usize, len: usize);
}

/// Sink that appends to a `Vec<u8>` until `limit` bytes have been produced.
pub(crate) struct VecSink<'a> {
    out: &'a mut Vec<u8>,
    limit: usize,
}

impl<'a> VecSink<'a> {
    pub(crate) fn new(out: &'a mut Vec<u8>, limit: usize) -> Self {
        Self { out, limit }
    }
}

impl LzwSink for VecSink<'_> {
    #[inline]
    fn space(&self) -> usize {
        self.limit.saturating_sub(self.out.len())
    }

    #[inline]
    fn position(&self) -> usize {
        self.out.len()
    }

    #[inline]
    fn reserve(&mut self, len: usize) -> &mut [u8] {
        let start = self.out.len();
        self.out.resize(start + len, 0);
        &mut self.out[start..]
    }

    #[inline]
    fn copy_earlier(&mut self, src: usize, len: usize) {
        self.out.extend_from_within(src..src + len);
    }
}

/// Sink that fills a caller-supplied slice, allocating nothing.
pub(crate) struct SliceSink<'a> {
    dst: &'a mut [u8],
    written: usize,
}

impl<'a> SliceSink<'a> {
    pub(crate) fn new(dst: &'a mut [u8]) -> Self {
        Self { dst, written: 0 }
    }

    /// Bytes written so far.
    pub(crate) fn written(&self) -> usize {
        self.written
    }
}

impl LzwSink for SliceSink<'_> {
    #[inline]
    fn space(&self) -> usize {
        self.dst.len() - self.written
    }

    #[inline]
    fn position(&self) -> usize {
        self.written
    }

    #[inline]
    fn reserve(&mut self, len: usize) -> &mut [u8] {
        let start = self.written;
        self.written += len;
        &mut self.dst[start..start + len]
    }

    #[inline]
    fn copy_earlier(&mut self, src: usize, len: usize) {
        let start = self.written;
        self.dst.copy_within(src..src + len, start);
        self.written += len;
    }
}

/// Strings at least this long are copied from their previous occurrence in
/// the output instead of being rebuilt byte by byte through the prefix
/// chain. Below it the chain walk wins, because the copy path costs an
/// extra table load and a `memcpy` call for a handful of bytes.
const COPY_BACK_THRESHOLD: usize = 16;

/// Core LZW decode loop, shared by every entry point.
///
/// Decoding stops when the sink is full, when the EOI code is read, or with
/// an error. Running out of input while the sink still has space is an
/// error ([`LzwError::UnexpectedEof`]) — a truncated stream is never
/// reported as success.
pub(crate) fn decode_into_sink<S: LzwSink, R: LzwCodeReader>(
    dict: &mut LzwDictionary,
    reader: &mut R,
    sink: &mut S,
) -> Result<()> {
    dict.reset();

    let clear_code = dict.clear_code();
    let eoi_code = dict.eoi_code();
    let uses_clear_code = dict.config().use_clear_code;
    let mut prev_code: Option<u16> = None;

    while sink.space() > 0 {
        let code = reader.read_code(dict.current_bits())?;

        if code == clear_code {
            // TIFF 6.0 mandates a ClearCode at the start of every strip and
            // again whenever the encoder's table reaches entry 4094;
            // encoders may also emit one at any other point (libtiff's
            // compression-ratio checkpoint resets), so accept it anywhere.
            if !uses_clear_code {
                return Err(LzwError::InvalidClearCode {
                    position: reader.code_bits_read(),
                });
            }
            dict.reset();
            prev_code = None;
            continue;
        }

        if code == eoi_code {
            break;
        }

        match prev_code {
            None => {
                // The first code after a reset must already be in the table.
                if u32::from(code) >= dict.next_code() {
                    return Err(LzwError::InvalidCode(code));
                }
            }
            Some(prev) => {
                if u32::from(code) < dict.next_code() {
                    // Ordinary case: the new entry is prev ++ first(code).
                    if !dict.is_full() {
                        let byte = dict.first_byte(code);
                        dict.add_entry_decode(prev, byte)?;
                    }
                } else if u32::from(code) == dict.next_code() {
                    // KwKwK: the code being read is the entry we are about
                    // to create, so create it first and then emit it.
                    if dict.is_full() {
                        return Err(LzwError::InvalidCode(code));
                    }
                    let byte = dict.first_byte(prev);
                    dict.add_entry_decode(prev, byte)?;
                } else {
                    return Err(LzwError::InvalidCode(code));
                }
            }
        }

        // Expand the code straight into the sink. When the string is longer
        // than the space left, only its leading bytes are written and the
        // loop then exits — the same result the previous implementation
        // produced by expanding fully and truncating afterwards.
        let full_len = dict.entry_len(code) as usize;
        let write_len = full_len.min(sink.space());
        if write_len > 0 {
            let start = sink.position();
            let copied = write_len >= COPY_BACK_THRESHOLD
                && copy_from_output(dict, code, write_len, full_len, sink);
            if !copied {
                let out = sink.reserve(write_len);
                dict.expand(code, out);
            }
            if write_len == full_len {
                dict.set_output_offset(code, start);
            }
        }

        prev_code = Some(code);
    }

    Ok(())
}

/// Try to produce `code`'s bytes by copying them from earlier in the output
/// instead of walking its prefix chain one byte at a time.
///
/// Two shapes cover almost every long string LZW produces:
///
/// * the whole string is already in the output (a repeated code), or
/// * its *parent* is — which is exactly the KwKwK / "growing run" shape a
///   flat image region produces, where every code is one byte longer than
///   the code emitted just before it.
///
/// Returns `false` when neither applies and the caller must walk the chain.
#[inline]
fn copy_from_output<S: LzwSink>(
    dict: &LzwDictionary,
    code: u16,
    write_len: usize,
    full_len: usize,
    sink: &mut S,
) -> bool {
    if let Some(source) = dict.output_offset(code) {
        sink.copy_earlier(source, write_len);
        return true;
    }
    let parent = dict.prefix_of(code);
    let Some(source) = dict.output_offset(parent) else {
        return false;
    };
    // string(code) == string(parent) ++ suffix(code), and string(parent) is
    // known to be present in full at `source`.
    let parent_len = full_len - 1;
    let copy_len = write_len.min(parent_len);
    if copy_len > 0 {
        sink.copy_earlier(source, copy_len);
    }
    if write_len > copy_len {
        let tail = sink.reserve(write_len - copy_len);
        // Exactly one byte can remain: the entry's own suffix.
        if let Some(slot) = tail.first_mut() {
            *slot = dict.suffix_byte(code);
        }
    }
    true
}

/// LZW decoder for decompression.
#[derive(Debug)]
pub struct LzwDecoder {
    /// Dictionary for code lookup.
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
        // Clamp the up-front reservation: `expected_size` is untrusted (see
        // MAX_INITIAL_CAPACITY). The Vec grows on demand beyond this.
        let mut output = Vec::with_capacity(expected_size.min(MAX_INITIAL_CAPACITY));
        let mut sink = VecSink::new(&mut output, expected_size);
        match self.dict.config().bit_order {
            LzwBitOrder::Msb => {
                let mut reader = MsbBitReader::new(input);
                decode_into_sink(&mut self.dict, &mut reader, &mut sink)?;
            }
            LzwBitOrder::Lsb => {
                let mut reader = LsbBitReader::new(input);
                decode_into_sink(&mut self.dict, &mut reader, &mut sink)?;
            }
        }
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
        let mut sink = SliceSink::new(dst);
        match self.dict.config().bit_order {
            LzwBitOrder::Msb => {
                let mut reader = MsbBitReader::new(input);
                decode_into_sink(&mut self.dict, &mut reader, &mut sink)?;
            }
            LzwBitOrder::Lsb => {
                let mut reader = LsbBitReader::new(input);
                decode_into_sink(&mut self.dict, &mut reader, &mut sink)?;
            }
        }
        Ok(sink.written())
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
