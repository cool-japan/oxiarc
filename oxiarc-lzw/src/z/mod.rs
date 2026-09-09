//! UNIX `compress(1)` / `.Z` container support (`Content-Encoding: compress`).
//!
//! `.Z` is the LZW variant produced by 4.3BSD `compress`, `ncompress` and
//! `gzip -Z`, and the coding HTTP calls `compress` / `x-compress`
//! (RFC 9110 §8.4.1.1). It is *not* TIFF LZW and *not* GIF LZW: it has its
//! own container header, its own code-width ceiling, its own table-reset
//! rule and its own bit-packing quirk, which is why it lives in a module of
//! its own instead of being another [`crate::LzwConfig`].
//!
//! # The format
//!
//! ```text
//! byte 0   0x1F  ┐ magic
//! byte 1   0x9D  ┘
//! byte 2   bit 7    block mode (code 256 resets the table)
//!          bits 5-6 reserved (ignored)
//!          bits 0-4 max_bits, 9..=16
//! byte 3.. LSB-first codes, 9 bits wide at first
//! ```
//!
//! * **No end-of-information code.** Decoding ends when the input does, so
//!   a truncated `.Z` decodes to a *prefix* and is not reported as an
//!   error. Both `gzip -dc` and `uncompress -c` behave this way; there is no
//!   in-band way to tell a truncated stream from a complete one.
//! * **Codes are written in groups of eight**, i.e. `n_bits` bytes at a
//!   time. When the table outgrows the current width, or when block mode
//!   resets it, the writer pads the current group out to eight codes and
//!   the reader skips the same padding. Get this wrong and streams decode
//!   correctly right up to the first width change.
//! * **Block mode** (every `compress(1)` in circulation sets it) makes code
//!   256 a table reset. The writer restarts allocation at 257; the reader
//!   restarts at 256 and burns that slot on the next code, which is what
//!   keeps the two in step. Without block mode, 256 is an ordinary entry.
//! * **The first code of a stream must be a literal byte**, in either mode.
//!   A leading CLEAR is [`crate::LzwError::InvalidCode`]`(256)`, not a reset
//!   of an already-initial table. This matches GNU `gzip`'s `unlzw.c`, whose
//!   `oldcode == -1` guard (`if (256 <= code) gzip_error("corrupt input.")`)
//!   runs *before* the CLEAR handling. Verified here against gzip 1.14 on
//!   GNU/Linux, which answers `gzip: <name>.Z: corrupt input.` and writes no
//!   output; on GNU systems `uncompress` is a link to `gunzip` and so gives
//!   the same answer. What a BSD-derived `uncompress` does with such a
//!   stream is not asserted either way — `tests/z_oracle.rs` reports it if
//!   one is on `PATH`. No `compress(1)` writes a leading CLEAR, so rejecting
//!   it costs no real stream and keeps crafted input off a decode path that
//!   every reference refuses.
//! * **`max_bits` is a table ceiling, not a code-width ceiling.** The
//!   reference widens the code as soon as `free_ent > maxcode`, and at
//!   `max_bits == 9` that check fires once more than it "should", so
//!   `compress -b 9` writes some **10-bit** codes (pinned by the
//!   CLI-produced `tests/data/z/widths_b09.Z`). A decoder therefore has to
//!   treat any code from `max_bits` upwards as corrupt input rather than as
//!   a table index — see [`crate::LzwError::InvalidCode`].
//! * **`max_bits` 9-16.** 12 is the TIFF/GIF ceiling; `compress` defaults to
//!   16. At that width the decoder's code table is about 193 KiB (a
//!   `prefix`/`suffix` pair over 65 536 entries) and the encoder's
//!   `(prefix, byte) -> code` index is about 768 KiB; both are measured by
//!   `tests/z_memory.rs`. The decoder's table is sized from the *header*, so
//!   a four-byte file can ask for the 16-bit one: the output limits below
//!   bound *output*, not table memory (this is the reference
//!   implementations' cost too).
//!
//! # Entry points
//!
//! | | one-shot | streaming |
//! |---|---|---|
//! | decode | [`decompress`], [`decompress_with_limit`], [`decompress_into`] | [`ZReader`] |
//! | encode | [`compress`], [`compress_with_block_mode`] | [`ZWriter`] |
//!
//! # Example
//!
//! ```rust
//! use oxiarc_lzw::z::{ZHeader, compress, decompress, decompress_with_limit};
//!
//! let original = b"UNIX compress round trip. ".repeat(64);
//! let stream = compress(&original, 16).expect("compress");
//!
//! // A real `.Z` file: `uncompress -c` and `gzip -dc` both accept this.
//! assert_eq!(&stream[..2], &[0x1F, 0x9D]);
//! assert_eq!(ZHeader::parse(&stream).expect("header").max_bits, 16);
//!
//! assert_eq!(decompress(&stream).expect("decompress"), original);
//!
//! // Untrusted input: bound the output, enforced while decoding.
//! assert!(decompress_with_limit(&stream, 8).is_err());
//! ```
//!
//! # Interoperability
//!
//! [`compress`] is byte-identical to `compress -b N -c` — not merely
//! decodable by it — for every width 9..=16 across the payload shapes in
//! `tests/z_oracle.rs`, and the decoder reproduces real `compress` output
//! byte for byte in the other direction. Two deliberate departures from the
//! reference C code:
//!
//! * on a **truncated** stream the reference decoders can emit one extra
//!   bogus code, because their final short read leaves the tail of a fixed
//!   `n_bits`-byte buffer holding the previous group; this decoder stops at
//!   the last code that lies wholly inside the data;
//! * **empty input** produces the three-byte header here (as `ncompress`
//!   does), where the `compress` on some platforms writes a zero-byte file
//!   that is not a `.Z` stream at all.

mod decode;
mod encode;
mod header;
mod io;
mod sink;

pub use header::{MAGIC, MAX_MAX_BITS, MIN_MAX_BITS, ZHeader};
pub use io::{ZReader, ZWriter};

use crate::error::Result;
use decode::ZDecoder;
use encode::ZEncoder;
use sink::{SliceZSink, VecZSink};

/// Decompress a complete `.Z` stream.
///
/// # Bombs
///
/// The output is unbounded: `.Z` carries no uncompressed size, and a few
/// kilobytes of 16-bit codes can expand to hundreds of megabytes. Use
/// [`decompress_with_limit`] (or [`ZReader::with_max_output`]) for anything
/// that did not come from a trusted source.
///
/// # Truncation
///
/// A truncated stream decodes to a prefix and returns `Ok`, because the
/// format has no end-of-information marker — the same thing `gzip -dc` does.
/// Callers that need to know a body was complete must get that from the
/// transport (`Content-Length`, chunked-encoding terminator, ...).
///
/// # Errors
///
/// - [`crate::LzwError::ZInvalidMagic`] / [`crate::LzwError::ZTruncatedHeader`]
///   / [`crate::LzwError::ZUnsupportedMaxBits`] for a header that is not a
///   `.Z` header this crate can decode,
/// - [`crate::LzwError::InvalidCode`] for a code outside the table (corrupt
///   input).
///
/// # Example
///
/// ```rust
/// use oxiarc_lzw::z::{compress, decompress};
///
/// let stream = compress(b"hello .Z", 16).expect("compress");
/// assert_eq!(decompress(&stream).expect("decompress"), b"hello .Z");
/// ```
pub fn decompress(data: &[u8]) -> Result<Vec<u8>> {
    decode_stream(data, usize::MAX)
}

/// Decompress a complete `.Z` stream, refusing to produce more than
/// `max_output` bytes.
///
/// The limit is checked **while** decoding, as each code is expanded, so a
/// decompression bomb is stopped at the limit instead of after it has
/// already been materialised.
///
/// # Errors
///
/// [`crate::LzwError::OutputLimitExceeded`] once the stream passes
/// `max_output`, plus everything [`decompress`] can return.
///
/// # Example
///
/// ```rust
/// use oxiarc_lzw::z::{compress, decompress_with_limit};
///
/// let bomb = compress(&vec![0u8; 1 << 20], 16).expect("compress");
/// assert!(bomb.len() < 8 * 1024);
/// assert!(decompress_with_limit(&bomb, 64 * 1024).is_err());
/// assert_eq!(
///     decompress_with_limit(&bomb, 1 << 20).expect("within the limit").len(),
///     1 << 20
/// );
/// ```
pub fn decompress_with_limit(data: &[u8], max_output: usize) -> Result<Vec<u8>> {
    decode_stream(data, max_output)
}

/// Decompress a complete `.Z` stream into a caller-supplied buffer,
/// allocating nothing for the output.
///
/// Returns the number of bytes written. A stream that produces more than
/// `dst` can hold is [`crate::LzwError::BufferTooSmall`], never a silent
/// truncation: `.Z` carries no uncompressed size, so a short buffer means
/// the caller's estimate was wrong rather than that the stream ended.
///
/// The decoder's own code table (about 193 KiB at 16 bits, less at narrower
/// widths) is still allocated; only the *output* is caller-owned.
///
/// # Errors
///
/// [`crate::LzwError::BufferTooSmall`] plus everything [`decompress`] can
/// return.
///
/// # Example
///
/// ```rust
/// use oxiarc_lzw::z::{compress, decompress_into};
///
/// let original = b"decoded straight into a buffer";
/// let stream = compress(original, 16).expect("compress");
///
/// let mut out = vec![0u8; original.len()];
/// let written = decompress_into(&stream, &mut out).expect("decompress");
/// assert_eq!(&out[..written], original);
///
/// let mut small = [0u8; 4];
/// assert!(decompress_into(&stream, &mut small).is_err());
/// ```
pub fn decompress_into(src: &[u8], dst: &mut [u8]) -> Result<usize> {
    let header = ZHeader::parse(src)?;
    let mut decoder = ZDecoder::new(header);
    let mut sink = SliceZSink::new(dst);
    decoder.decode(&src[ZHeader::LEN..], &mut sink)?;
    Ok(sink.written())
}

/// Compress `data` into a block-mode `.Z` stream with `max_bits`-wide
/// codes.
///
/// This is `compress -b max_bits`, byte for byte: the output of this
/// function is identical to the reference tool's for every width 9..=16
/// (pinned by `tests/z_oracle.rs`). `max_bits` 16 is the reference default
/// and the best ratio; smaller widths exist for readers that cannot handle
/// wide codes.
///
/// Empty input produces the three-byte header, which is a valid empty `.Z`
/// stream.
///
/// # Errors
///
/// [`crate::LzwError::ZUnsupportedMaxBits`] unless `9 <= max_bits <= 16`.
///
/// # Example
///
/// ```rust
/// use oxiarc_lzw::z::{compress, decompress};
///
/// let original = b"TOBEORNOTTOBEORTOBEORNOT".repeat(40);
/// let stream = compress(&original, 16).expect("compress");
/// assert!(stream.len() < original.len());
/// assert_eq!(decompress(&stream).expect("decompress"), original);
/// ```
pub fn compress(data: &[u8], max_bits: u8) -> Result<Vec<u8>> {
    compress_with_block_mode(data, max_bits, true)
}

/// Compress `data` with explicit control over the block-mode flag.
///
/// Block mode (the default, and what every `compress(1)` in circulation
/// writes) lets the encoder reset the code table mid-stream when the
/// compression ratio degrades. Clearing it produces the pre-4.3BSD dialect
/// in which code 256 is an ordinary table entry — still legal, still
/// decodable by `uncompress`, and occasionally required by very old
/// readers.
///
/// # Errors
///
/// [`crate::LzwError::ZUnsupportedMaxBits`] unless `9 <= max_bits <= 16`.
///
/// # Example
///
/// ```rust
/// use oxiarc_lzw::z::{ZHeader, compress_with_block_mode, decompress};
///
/// let original = b"no mid-stream table resets in this one".repeat(8);
/// let stream = compress_with_block_mode(&original, 12, false).expect("compress");
/// assert!(!ZHeader::parse(&stream).expect("header").block_mode);
/// assert_eq!(decompress(&stream).expect("decompress"), original);
/// ```
pub fn compress_with_block_mode(data: &[u8], max_bits: u8, block_mode: bool) -> Result<Vec<u8>> {
    let header = ZHeader::new(max_bits, block_mode)?;
    let mut out = Vec::with_capacity(ZHeader::LEN + data.len() / 2 + 16);
    out.extend_from_slice(&header.to_bytes());
    let mut encoder = ZEncoder::new(header);
    encoder.push(data, &mut out);
    encoder.finish(&mut out);
    Ok(out)
}

/// Shared body of [`decompress`] and [`decompress_with_limit`].
fn decode_stream(data: &[u8], limit: usize) -> Result<Vec<u8>> {
    let header = ZHeader::parse(data)?;
    let mut decoder = ZDecoder::new(header);
    let mut out = Vec::new();
    let mut sink = VecZSink::new(&mut out, limit);
    decoder.decode(&data[ZHeader::LEN..], &mut sink)?;
    Ok(out)
}
