//! Compression 34925: LZMA2, framed as one complete `.xz` stream per chunk.
//!
//! libtiff's `LZMACodec` calls `lzma_stream_encoder`, so every strip or tile
//! carries a whole `.xz` container: the 12-byte stream header (magic
//! `FD 37 7A 58 5A 00`, stream flags, CRC-32), one block whose filter chain is
//! LZMA2, the index and the 12-byte footer. libtiff selects
//! `LZMA_CHECK_NONE`, which is what this module writes too; the reader accepts
//! all four check types (`None`, CRC-32, CRC-64, SHA-256) because other
//! writers use them.
//!
//! The framing itself lives in [`oxiarc_lzma::xz`] — the same code
//! `oxiarc-archive` uses for `.xz` files — so a TIFF strip and an `.xz` file
//! cannot drift apart.
//!
//! # Reused state
//!
//! [`XzDecoder`] keeps the LZMA2 dictionary of the last stream it decoded and
//! hands it to the next one whose declared dictionary size matches, so a page
//! of a thousand strips grows the dictionary `Vec` once instead of a thousand
//! times. The decoder is cached in the image's
//! [`CodecState`](super::CodecState); without a state the codec still works,
//! it just builds one per chunk, which is exactly what the free function
//! [`oxiarc_lzma::xz::decompress_into`] does (it *is*
//! `XzDecoder::new().decompress_into(..)`). Reuse is therefore
//! behaviour-identical by construction, and `oxiarc-lzma`'s own reuse-safety
//! predicate refuses to carry state into a block that does not open with a
//! full reset; `tests/codec_reuse.rs` asserts the byte identity here as well.
//!
//! ```
//! use oxiarc_tiff::compression::{decode_into, encode, CodecContext, CodecLevel};
//! use oxiarc_tiff::{CompressionMethod, Endian};
//!
//! let pixels: Vec<u8> = (0..256u32).map(|i| (i % 7) as u8).collect();
//! let cx = CodecContext::new(CompressionMethod::Lzma, 256, 1, &[8], 1, Endian::Little);
//! let chunk = encode(&pixels, &cx, CodecLevel::Level(6))?;
//! assert_eq!(&chunk[..6], &[0xFD, b'7', b'z', b'X', b'Z', 0x00]);
//! let mut out = vec![0u8; pixels.len()];
//! assert_eq!(decode_into(&chunk, &mut out, &cx)?, pixels.len());
//! assert_eq!(out, pixels);
//! # Ok::<(), oxiarc_tiff::TiffError>(())
//! ```

use oxiarc_lzma::xz::{self, XzDecoder};

use super::pool::Pool;
use super::{CodecContext, CodecLevel, codec_error};
use crate::error::Result;
use crate::tags::CompressionMethod;

/// libtiff's default `LZMA_PRESET`.
const DEFAULT_PRESET: u8 = 6;

/// Cached [`XzDecoder`]s, kept across the chunks of one image.
///
/// A [`Pool`] rather than a single slot, for the same reason the zstd one is:
/// the lock is held only while a decoder is taken and put back, so a parallel
/// decode of an LZMA page keeps one decoder per worker instead of serialising
/// every worker behind one.
#[derive(Default)]
pub(crate) struct XzSlot(Pool<XzDecoder>);

impl core::fmt::Debug for XzSlot {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let held = self.0.inspect(<[XzDecoder]>::len);
        f.debug_tuple("XzSlot").field(&held).finish()
    }
}

impl XzSlot {
    /// Decodes one complete `.xz` stream through a pooled decoder.
    ///
    /// No `reset()`: carrying the dictionary allocation from the previous
    /// strip is the entire point, and `oxiarc-lzma` decides for itself
    /// whether a given block may reuse the *state* behind it.
    fn decompress_into(&self, src: &[u8], dst: &mut [u8]) -> Result<usize> {
        let mut decoder = self.0.take().unwrap_or_default();
        let out = decoder
            .decompress_into(src, dst)
            .map_err(|error| codec_error(CompressionMethod::Lzma, error));
        self.0.put(decoder);
        out
    }
}

/// Decodes one `.xz` stream into `dst`.
///
/// # Errors
/// [`crate::FormatError::Codec`] for a corrupt or truncated stream.
pub fn decode_into(src: &[u8], dst: &mut [u8], cx: &CodecContext<'_>) -> Result<usize> {
    let decoded = match cx.state {
        Some(state) => state.xz().decompress_into(src, dst),
        None => xz::decompress_into(src, dst)
            .map_err(|error| codec_error(CompressionMethod::Lzma, error)),
    };
    match decoded {
        Ok(written) => Ok(written),
        Err(error) => {
            if cx.leniency.is_lenient() {
                if let Ok(partial) = xz::decompress_with_limit(src, dst.len()) {
                    let take = partial.len().min(dst.len());
                    if let (Some(slot), Some(bytes)) = (dst.get_mut(..take), partial.get(..take)) {
                        slot.copy_from_slice(bytes);
                        return Ok(take);
                    }
                }
                return Ok(0);
            }
            Err(error)
        }
    }
}

/// Encodes one strip or tile as a complete `.xz` stream.
///
/// # Errors
/// [`crate::FormatError::Codec`] if the encoder fails.
pub fn encode(src: &[u8], level: CodecLevel) -> Result<Vec<u8>> {
    let preset = match level {
        CodecLevel::Level(value) => value.clamp(0, 9) as u8,
        _ => DEFAULT_PRESET,
    };
    xz::compress(src, preset).map_err(|error| codec_error(CompressionMethod::Lzma, error))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::byteorder::Endian;
    use crate::limits::Leniency;

    fn payload() -> Vec<u8> {
        (0..4096u32).map(|i| (i / 11 % 241) as u8).collect()
    }

    fn context(bits: &[u16]) -> CodecContext<'_> {
        CodecContext::new(CompressionMethod::Lzma, 4096, 1, bits, 1, Endian::Little)
    }

    #[test]
    fn round_trips_and_writes_a_real_xz_container() {
        let data = payload();
        let bits = [8u16];
        let cx = context(&bits);
        for level in [
            CodecLevel::Default,
            CodecLevel::Level(0),
            CodecLevel::Level(9),
        ] {
            let chunk = encode(&data, level).expect("encode");
            assert_eq!(
                chunk.get(..6),
                Some(&[0xFD, b'7', b'z', b'X', b'Z', 0x00][..])
            );
            assert_eq!(chunk.get(chunk.len() - 2..), Some(&b"YZ"[..]));
            let mut out = vec![0u8; data.len()];
            assert_eq!(
                decode_into(&chunk, &mut out, &cx).expect("decode"),
                data.len()
            );
            assert_eq!(out, data);
        }
    }

    #[test]
    fn a_truncated_stream_is_an_error_and_lenient_never_panics() {
        let data = payload();
        let bits = [8u16];
        let chunk = encode(&data, CodecLevel::Default).expect("encode");
        let mut lenient = context(&bits);
        lenient.leniency = Leniency::Lenient;
        for cut in [1usize, 12, 40, chunk.len() / 2, chunk.len() - 1] {
            let piece = chunk.get(..cut).unwrap_or_default();
            let mut out = vec![0u8; data.len()];
            assert!(decode_into(piece, &mut out, &context(&bits)).is_err());
            let mut out = vec![0u8; data.len()];
            let written = decode_into(piece, &mut out, &lenient).expect("lenient");
            assert!(written <= data.len());
        }
    }

    #[test]
    fn a_corrupt_block_is_reported() {
        let data = payload();
        let bits = [8u16];
        let cx = context(&bits);
        let mut chunk = encode(&data, CodecLevel::Default).expect("encode");
        for byte in chunk.iter_mut().skip(20).take(16) {
            *byte ^= 0x33;
        }
        let mut out = vec![0u8; data.len()];
        assert!(decode_into(&chunk, &mut out, &cx).is_err());
    }
}
