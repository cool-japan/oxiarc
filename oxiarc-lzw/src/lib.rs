//! # OxiARC-LZW: Pure Rust LZW Compression
//!
//! This crate provides LZW (Lempel-Ziv-Welch) compression and decompression
//! with support for TIFF and GIF formats.
//!
//! ## Features
//!
//! - **Pure Rust**: No C dependencies, 100% safe Rust
//! - **TIFF LZW**: MSB-first bit order, early code change
//! - **GIF LZW**: LSB-first bit order, variable minimum code size
//! - **UNIX `compress` / `.Z`**: the [`z`] module — `1F 9D` header, 9-16 bit
//!   codes, block mode, LSB-first 8-code groups (new in 0.4.2)
//! - **Explicit bit order**: [`LzwBitOrder`] on [`LzwConfig`], so the generic
//!   entry points decode MSB-first *or* LSB-first streams (new in 0.4.2)
//! - **Bug Fix**: Fixes truncation bug found in weezl crate
//!
//! ## TIFF LZW Specification
//!
//! TIFF uses a specific variant of LZW compression (TIFF 6.0 §13,
//! libtiff/Pillow/GDAL-compatible):
//!
//! - **MSB-first bit order**: Bits are packed from most significant to least
//! - **9-12 bit codes**: Variable-length codes starting at 9 bits
//! - **Early code change**: Bit width increases one code earlier than standard
//! - **Clear codes**: Every strip begins with code 256 (ClearCode); the
//!   encoder emits another ClearCode and resets the table when it reaches
//!   entry 4094, and the decoder accepts ClearCode resets anywhere
//! - **EOI termination**: Streams end with code 257 (End of Information)
//!
//! ## Example
//!
//! ```rust
//! use oxiarc_lzw::{compress_tiff, decompress_tiff};
//!
//! let original = b"TOBEORNOTTOBEORTOBEORNOT";
//!
//! // Compress
//! let compressed = compress_tiff(original).expect("compress with TIFF LZW");
//!
//! // Decompress
//! let decompressed = decompress_tiff(&compressed, original.len()).expect("decompress the TIFF strip");
//!
//! assert_eq!(decompressed, original);
//! ```
//!
//! ## Zero-allocation TIFF strip decoding
//!
//! [`decompress_tiff_into`] decodes a TIFF LZW strip straight into a
//! caller-supplied buffer using the classical prefix/suffix code table, so
//! no allocation happens per decoded code (and none at all beyond the code
//! table itself):
//!
//! ```rust
//! use oxiarc_lzw::{compress_tiff, decompress_tiff_into};
//!
//! let strip = b"row0row0row1row1row2row2";
//! let compressed = compress_tiff(strip).expect("compress with TIFF LZW");
//!
//! let mut out = vec![0u8; strip.len()];
//! let written = decompress_tiff_into(&compressed, &mut out).expect("decode the strip");
//! assert_eq!(written, strip.len());
//! assert_eq!(&out[..written], strip);
//! ```
//!
//! ## Critical Bug Fix
//!
//! This implementation fixes a critical bug in the weezl crate where LZW
//! decompression would terminate early, truncating the output:
//!
//! ```rust
//! use oxiarc_lzw::{compress_tiff, decompress_tiff};
//!
//! // This test case fails with weezl (truncates to ~250 bytes)
//! // but works correctly with oxiarc-lzw (outputs full 310 bytes)
//! let original = b"This is a test of compression! ".repeat(10);
//! assert_eq!(original.len(), 310);
//!
//! let compressed = compress_tiff(&original).expect("compress with TIFF LZW");
//! let decompressed = decompress_tiff(&compressed, original.len()).expect("decompress the TIFF strip");
//!
//! // CRITICAL: No truncation!
//! assert_eq!(decompressed.len(), 310);
//! assert_eq!(decompressed, &original[..]);
//! ```

#![warn(missing_docs)]
#![warn(clippy::all)]
#![forbid(unsafe_code)]

mod bits;
mod bitstream_lsb;
mod bitstream_msb;
mod config;
mod decoder;
mod dictionary;
mod encoder;
mod error;
mod gif_lzw;
pub mod streaming;
pub mod z;

pub use config::{LzwBitOrder, LzwConfig};
pub use decoder::LzwDecoder;
pub use encoder::LzwEncoder;
pub use error::{LzwError, Result};
pub use gif_lzw::{gif_compress, gif_decompress};
pub use streaming::{LzwStreamDecoder, LzwStreamEncoder, LzwStreamMode};

/// Decompress LZW-compressed data with the given configuration.
///
/// # Parameters
///
/// - `data`: LZW-compressed input
/// - `expected_size`: Expected size of decompressed output
/// - `config`: LZW configuration (TIFF or GIF)
///
/// # Returns
///
/// Decompressed byte sequence.
///
/// # Example
///
/// ```rust
/// use oxiarc_lzw::{decompress, compress, LzwConfig};
///
/// let original = b"Hello, World!";
/// let compressed = compress(original, LzwConfig::TIFF).expect("compress");
/// let decompressed = decompress(&compressed, original.len(), LzwConfig::TIFF).expect("decompress");
/// assert_eq!(decompressed, original);
/// ```
pub fn decompress(data: &[u8], expected_size: usize, config: LzwConfig) -> Result<Vec<u8>> {
    let mut decoder = LzwDecoder::new(config)?;
    decoder.decode(data, expected_size)
}

/// Compress data with LZW using the given configuration.
///
/// # Parameters
///
/// - `data`: Uncompressed input
/// - `config`: LZW configuration (TIFF or GIF)
///
/// # Returns
///
/// LZW-compressed byte sequence.
///
/// # Example
///
/// ```rust
/// use oxiarc_lzw::{compress, LzwConfig};
///
/// let data = b"TOBEORNOTTOBEORTOBEORNOT";
/// let compressed = compress(data, LzwConfig::TIFF).expect("compress");
/// assert!(compressed.len() < data.len());
/// ```
pub fn compress(data: &[u8], config: LzwConfig) -> Result<Vec<u8>> {
    let mut encoder = LzwEncoder::new(config)?;
    encoder.encode(data)
}

/// Decompress TIFF LZW data (convenience function).
///
/// This is equivalent to `decompress(data, expected_size, LzwConfig::TIFF)`.
///
/// # Parameters
///
/// - `data`: TIFF LZW-compressed input
/// - `expected_size`: Expected size of decompressed output
///
/// # Returns
///
/// Decompressed byte sequence.
///
/// # Example
///
/// ```rust
/// use oxiarc_lzw::{compress_tiff, decompress_tiff};
///
/// let original = b"This is a TIFF LZW test";
/// let compressed = compress_tiff(original).expect("compress with TIFF LZW");
/// let decompressed = decompress_tiff(&compressed, original.len()).expect("decompress the TIFF strip");
/// assert_eq!(decompressed, original);
/// ```
pub fn decompress_tiff(data: &[u8], expected_size: usize) -> Result<Vec<u8>> {
    decompress(data, expected_size, LzwConfig::TIFF)
}

/// Decompress LZW data directly into a caller-supplied buffer.
///
/// This is the allocation-free decode entry point: codes are expanded
/// through the prefix/suffix code table straight into `dst`, so decoding a
/// TIFF strip costs no per-code allocation and no intermediate `Vec`.
///
/// # Parameters
///
/// - `src`: LZW-compressed input, packed in `config.bit_order`
/// - `dst`: output buffer; decoding stops once it is full
/// - `config`: LZW configuration (see [`LzwConfig::TIFF`],
///   [`LzwConfig::TIFF_OLD_STYLE`] and [`LzwConfig::TIFF_COMPAT_LSB`])
///
/// # Bit order
///
/// `config.bit_order` selects the packing (new in 0.4.2):
/// [`LzwConfig::TIFF`] and [`LzwConfig::TIFF_OLD_STYLE`] are MSB-first,
/// [`LzwConfig::GIF`] and [`LzwConfig::TIFF_COMPAT_LSB`] are LSB-first.
///
/// Before 0.4.2 `LzwConfig` had no such field and `LzwConfig::GIF` was
/// *literally the same value* as `LzwConfig::TIFF_OLD_STYLE`, so passing it
/// here silently decoded MSB-first. That is fixed: the two constants are now
/// different configurations and each decodes only streams packed its way.
/// Callers that were relying on the old (accidental) MSB behaviour of
/// `LzwConfig::GIF` must pass `LzwConfig::TIFF_OLD_STYLE` instead.
///
/// This entry point still is **not** the GIF 89a codec: real GIF image data
/// carries its own `minimum_code_size` and sub-block framing and belongs in
/// [`gif_decompress`]. For UNIX `compress(1)` (`.Z`) data use the [`z`]
/// module, which additionally implements that format's container header and
/// 8-code group alignment.
///
/// # Returns
///
/// The number of bytes written to `dst`. A value smaller than `dst.len()`
/// means the stream ended (EOI) before the buffer was filled.
///
/// # Errors
///
/// - [`LzwError::UnexpectedEof`] if the input runs out before `dst` is full
///   and before an EOI code is read (a truncated strip is never reported as
///   success),
/// - [`LzwError::InvalidCode`] for a code outside the current table,
/// - [`LzwError::InvalidBitWidth`] if `config` is out of range.
///
/// Any input left over once `dst` is full is ignored, matching libtiff's
/// `LZWDecode`.
///
/// # Example
///
/// ```rust
/// use oxiarc_lzw::{compress, decompress_into, LzwConfig};
///
/// let original = b"Hello, World! Hello, World!";
/// let compressed = compress(original, LzwConfig::TIFF).expect("compress");
///
/// let mut out = vec![0u8; original.len()];
/// let written = decompress_into(&compressed, &mut out, LzwConfig::TIFF).expect("decode into the buffer");
/// assert_eq!(&out[..written], original);
/// ```
pub fn decompress_into(src: &[u8], dst: &mut [u8], config: LzwConfig) -> Result<usize> {
    let mut decoder = LzwDecoder::new(config)?;
    decoder.decode_into(src, dst)
}

/// Decompress a TIFF LZW strip/tile directly into a caller-supplied buffer.
///
/// Equivalent to `decompress_into(src, dst, LzwConfig::TIFF)`: MSB-first
/// 9-12 bit codes, TIFF early code change, ClearCode/EOI handling per TIFF
/// 6.0 §13 and libtiff.
///
/// # Returns
///
/// The number of bytes written to `dst`.
///
/// # Errors
///
/// See [`decompress_into`].
///
/// # Old-style writers (no early code-width change)
///
/// Some encoders follow TIFF 6.0's own pseudo-code and grow the code width
/// one code later than libtiff does; [`LzwConfig::TIFF_OLD_STYLE`] decodes
/// those. Fall back to it **only when the standard rule returns an error**,
/// and decode the retry into a scratch buffer so a failed retry cannot
/// clobber a good result:
///
/// ```rust
/// use oxiarc_lzw::{compress, decompress_into, decompress_tiff_into, LzwConfig};
///
/// # fn decode_strip(strip: &[u8], out: &mut [u8]) -> oxiarc_lzw::Result<usize> {
/// let written = match decompress_tiff_into(strip, out) {
///     Ok(n) => n,
///     Err(standard_err) => {
///         let mut scratch = vec![0u8; out.len()];
///         match decompress_into(strip, &mut scratch, LzwConfig::TIFF_OLD_STYLE) {
///             Ok(n) => {
///                 out.copy_from_slice(&scratch);
///                 n
///             }
///             // Neither rule explains the strip: report the first failure.
///             Err(_) => return Err(standard_err),
///         }
///     }
/// };
/// # Ok(written)
/// # }
/// let original = b"old-style TIFF LZW strip; long enough that the code \
///                  width has to grow, which is the only place the two \
///                  rules disagree at all. AAAAAAAAAABBBBBBBBBBCCCCCCCCCC";
/// let original = original.repeat(20);
/// let strip = compress(&original, LzwConfig::TIFF_OLD_STYLE).expect("compress");
/// let mut out = vec![0u8; original.len()];
/// let written = decode_strip(&strip, &mut out).expect("decode the strip");
/// assert_eq!(&out[..written], &original[..]);
/// ```
///
/// Two rules that this recipe deliberately does **not** apply, because both
/// corrupt real images:
///
/// * A short return (`n < dst.len()`) is a **short strip**, not a reason to
///   retry: a strip whose data ends before the geometry says it should is
///   ordinary (libtiff warns and keeps the partial row), while an old-style
///   retry on such a strip fails and destroys the bytes already decoded.
/// * Caching the winning configuration per image is worth it (the retry
///   then costs one strip), but the *first* strip must still be decoded by
///   this recipe rather than by sniffing the stream: unlike libtiff's
///   LSB-first `LZWDecodeCompat` case, an early-change decoder cannot tell
///   an old-style stream from a standard one by looking at its first bytes.
///
/// One case no return-value heuristic can catch: an old-style stream can
/// occasionally fill `dst` completely under the standard rule with wrong
/// bytes (measured at roughly 2 % of random old-style strips in
/// `tests/old_style_fallback.rs`). A caller that *knows* a file is old-style should
/// select [`LzwConfig::TIFF_OLD_STYLE`] outright instead of relying on the
/// fallback.
///
/// # Example
///
/// ```rust
/// use oxiarc_lzw::{compress_tiff, decompress_tiff_into};
///
/// let original = b"TOBEORNOTTOBEORTOBEORNOT";
/// let compressed = compress_tiff(original).expect("compress with TIFF LZW");
///
/// let mut out = vec![0u8; original.len()];
/// let written = decompress_tiff_into(&compressed, &mut out).expect("decode the strip");
/// assert_eq!(written, original.len());
/// assert_eq!(&out[..], original);
/// ```
pub fn decompress_tiff_into(src: &[u8], dst: &mut [u8]) -> Result<usize> {
    decompress_into(src, dst, LzwConfig::TIFF)
}

/// Compress data with TIFF LZW (convenience function).
///
/// This is equivalent to `compress(data, LzwConfig::TIFF)`.
///
/// # Parameters
///
/// - `data`: Uncompressed input
///
/// # Returns
///
/// TIFF LZW-compressed byte sequence.
///
/// # Example
///
/// ```rust
/// use oxiarc_lzw::compress_tiff;
///
/// let data = b"This is a TIFF LZW test";
/// let compressed = compress_tiff(data).expect("compress with TIFF LZW");
/// assert!(!compressed.is_empty());
/// ```
pub fn compress_tiff(data: &[u8]) -> Result<Vec<u8>> {
    compress(data, LzwConfig::TIFF)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_roundtrip_tiff() {
        let original = b"TOBEORNOTTOBEORTOBEORNOT";
        let compressed = compress_tiff(original).expect("tiff compress roundtrip");
        let decompressed =
            decompress_tiff(&compressed, original.len()).expect("tiff decompress roundtrip");
        assert_eq!(decompressed, original);
    }

    #[test]
    fn test_310_byte_no_truncation() {
        // THE CRITICAL TEST - must output 310 bytes!
        let original = b"This is a test of compression! ".repeat(10);
        assert_eq!(original.len(), 310);

        let compressed = compress_tiff(&original).expect("tiff compress 310 bytes");
        let decompressed =
            decompress_tiff(&compressed, original.len()).expect("tiff decompress 310 bytes");

        // MUST be 310 bytes (weezl truncates to ~250)
        assert_eq!(
            decompressed.len(),
            310,
            "Must not truncate! Expected 310 bytes"
        );
        assert_eq!(decompressed, &original[..]);
    }

    #[test]
    fn test_empty_input() {
        let original = b"";
        let compressed = compress_tiff(original).expect("tiff compress empty");
        let decompressed = decompress_tiff(&compressed, 0).expect("tiff decompress empty");
        assert_eq!(decompressed, original);
    }

    #[test]
    fn test_single_byte() {
        let original = b"A";
        let compressed = compress_tiff(original).expect("tiff compress single byte");
        let decompressed =
            decompress_tiff(&compressed, original.len()).expect("tiff decompress single byte");
        assert_eq!(decompressed, original);
    }

    #[test]
    fn test_repeating_pattern() {
        let original = vec![b'X'; 1000];
        let compressed = compress_tiff(&original).expect("tiff compress repeating pattern");

        // Highly repetitive - should compress well
        assert!(compressed.len() < original.len() / 2);

        let decompressed = decompress_tiff(&compressed, original.len())
            .expect("tiff decompress repeating pattern");
        assert_eq!(decompressed, original);
    }

    #[test]
    fn test_all_byte_values() {
        // FIXED: This test now passes with the decoder bit-width synchronization fix
        let original: Vec<u8> = (0..=255).collect();
        let compressed = compress_tiff(&original).expect("tiff compress all bytes");
        let decompressed =
            decompress_tiff(&compressed, original.len()).expect("tiff decompress all bytes");
        assert_eq!(decompressed, original);
    }

    #[test]
    fn test_large_input() {
        let original = b"The quick brown fox jumps over the lazy dog. ".repeat(100);
        let compressed = compress_tiff(&original).expect("tiff compress large input");
        let decompressed =
            decompress_tiff(&compressed, original.len()).expect("tiff decompress large input");
        assert_eq!(decompressed, original);
    }
}
