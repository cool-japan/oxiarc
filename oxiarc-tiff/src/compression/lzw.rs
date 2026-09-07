//! Compression 5: TIFF LZW.
//!
//! MSB-first codes that grow from 9 to 12 bits, a `ClearCode` of 256 and an
//! `EndOfInformation` of 257, exactly as TIFF 6.0 §13 and libtiff's
//! `LZWDecode` define them. The decode itself lives in
//! [`oxiarc_lzw::decompress_tiff_into`], which expands codes through a
//! prefix/suffix table straight into the caller's buffer: no per-code
//! allocation, and no intermediate `Vec` between the strip and the chunk
//! buffer.
//!
//! # The old-style code-width rule
//!
//! TIFF 6.0's own pseudo-code grows the code width one code *later* than
//! libtiff does. Practically every encoder follows libtiff (which is why
//! libtiff calls its own reader `LZWDecode` and the other one
//! `LZWDecodeCompat`), but pre-1993 files and a long tail of scanner firmware
//! exist. This module decodes with the standard rule, retries with
//! [`LzwConfig::TIFF_OLD_STYLE`] **when the standard rule errors**, and caches
//! the winning rule in the image's [`CodecState`](super::CodecState) so the
//! retry costs one strip rather than every strip.
//!
//! A *short* result is deliberately not a retry trigger: a strip whose data
//! ends before the geometry says it should is ordinary (libtiff warns and
//! keeps the partial rows), and `oxiarc-lzw`'s own measurements show an
//! old-style retry on such a strip fails while destroying bytes that were
//! already correct.
//!
//! ```
//! use oxiarc_tiff::compression::{decode_into, encode, CodecContext, CodecLevel};
//! use oxiarc_tiff::{CompressionMethod, Endian};
//!
//! let pixels = [1u8, 1, 1, 1, 2, 2, 2, 2];
//! let cx = CodecContext::new(CompressionMethod::Lzw, 8, 1, &[8], 1, Endian::Little);
//! let strip = encode(&pixels, &cx, CodecLevel::Default)?;
//! let mut out = [0u8; 8];
//! assert_eq!(decode_into(&strip, &mut out, &cx)?, 8);
//! assert_eq!(out, pixels);
//! # Ok::<(), oxiarc_tiff::TiffError>(())
//! ```

use oxiarc_lzw::{LzwConfig, compress_tiff, decompress_into, decompress_tiff_into};

use super::state::LzwMode;
use super::{CodecContext, codec_error};
use crate::error::Result;
use crate::tags::CompressionMethod;

/// Decodes one LZW strip or tile into `dst`.
///
/// # Errors
/// [`crate::FormatError::Codec`] when neither code-width rule explains the
/// stream; the *standard* rule's message is the one reported, because that is
/// the rule 99 % of files follow.
pub fn decode_into(src: &[u8], dst: &mut [u8], cx: &CodecContext<'_>) -> Result<usize> {
    match cx.state.map(super::CodecState::lzw_mode) {
        Some(LzwMode::OldStyle) => old_style(src, dst),
        Some(LzwMode::Standard) | None => standard(src, dst),
        Some(LzwMode::Undecided) => decide(src, dst, cx),
    }
}

/// Decodes with libtiff's rule and, if that fails, with TIFF 6.0's.
fn decide(src: &[u8], dst: &mut [u8], cx: &CodecContext<'_>) -> Result<usize> {
    let standard_error = match decompress_tiff_into(src, dst) {
        Ok(written) => {
            // Only a strip that filled the whole chunk proves the rule: a
            // short one is a short strip, whichever rule produced it.
            if written == dst.len() {
                if let Some(state) = cx.state {
                    state.set_lzw_mode(LzwMode::Standard);
                }
            }
            return Ok(written);
        }
        Err(error) => error,
    };
    // The retry decodes into scratch so a second failure cannot corrupt what
    // the first attempt already wrote (`oxiarc-lzw`'s documented recipe).
    let mut scratch = vec![0u8; dst.len()];
    match decompress_into(src, &mut scratch, LzwConfig::TIFF_OLD_STYLE) {
        Ok(written) => {
            dst.copy_from_slice(&scratch);
            if let Some(state) = cx.state {
                state.set_lzw_mode(LzwMode::OldStyle);
            }
            Ok(written)
        }
        Err(_) => Err(codec_error(CompressionMethod::Lzw, standard_error)),
    }
}

/// libtiff's rule: the code width grows one code early.
fn standard(src: &[u8], dst: &mut [u8]) -> Result<usize> {
    decompress_tiff_into(src, dst).map_err(|error| codec_error(CompressionMethod::Lzw, error))
}

/// TIFF 6.0's rule: the code width grows one code late.
fn old_style(src: &[u8], dst: &mut [u8]) -> Result<usize> {
    decompress_into(src, dst, LzwConfig::TIFF_OLD_STYLE)
        .map_err(|error| codec_error(CompressionMethod::Lzw, error))
}

/// Encodes one strip or tile.
///
/// Always the standard rule: `compress_tiff` emits the leading `ClearCode` and
/// re-clears at entry 4094, which is what libtiff, Pillow and GDAL all expect.
///
/// # Errors
/// [`crate::FormatError::Codec`] if the encoder rejects the input.
pub fn encode(src: &[u8]) -> Result<Vec<u8>> {
    compress_tiff(src).map_err(|error| codec_error(CompressionMethod::Lzw, error))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::byteorder::Endian;
    use crate::compression::CodecState;

    fn context<'a>(state: Option<&'a CodecState>, bits: &'a [u16]) -> CodecContext<'a> {
        let mut cx = CodecContext::new(CompressionMethod::Lzw, 64, 1, bits, 1, Endian::Little);
        cx.state = state;
        cx
    }

    /// A payload long enough that the code width has to grow, which is the
    /// only place the two rules disagree.
    fn payload() -> Vec<u8> {
        let mut data = Vec::new();
        for i in 0..600u32 {
            data.push((i % 251) as u8);
            data.push((i / 7 % 253) as u8);
        }
        data
    }

    #[test]
    fn standard_streams_round_trip() {
        let data = payload();
        let strip = encode(&data).expect("encode");
        let bits = [8u16];
        let cx = context(None, &bits);
        let mut out = vec![0u8; data.len()];
        assert_eq!(
            decode_into(&strip, &mut out, &cx).expect("decode"),
            data.len()
        );
        assert_eq!(out, data);
    }

    #[test]
    fn an_old_style_strip_is_decoded_and_the_rule_is_cached() {
        let data = payload();
        let strip = oxiarc_lzw::compress(&data, LzwConfig::TIFF_OLD_STYLE).expect("old style");
        let state = CodecState::new();
        let bits = [8u16];
        let cx = context(Some(&state), &bits);
        let mut out = vec![0u8; data.len()];
        let written = decode_into(&strip, &mut out, &cx).expect("old-style decode");
        assert_eq!(written, data.len());
        assert_eq!(out, data);
        assert!(state.lzw_is_old_style(), "the rule must be cached");

        // A second strip now decodes without the failed standard attempt.
        let mut again = vec![0u8; data.len()];
        assert_eq!(
            decode_into(&strip, &mut again, &cx).expect("cached decode"),
            data.len()
        );
        assert_eq!(again, data);
    }

    #[test]
    fn a_standard_strip_that_fills_the_chunk_caches_the_standard_rule() {
        let data = payload();
        let strip = encode(&data).expect("encode");
        let state = CodecState::new();
        let bits = [8u16];
        let cx = context(Some(&state), &bits);
        let mut out = vec![0u8; data.len()];
        decode_into(&strip, &mut out, &cx).expect("decode");
        assert!(!state.lzw_is_old_style());
        assert_eq!(state.lzw_mode(), LzwMode::Standard);
    }

    #[test]
    fn a_short_strip_does_not_decide_the_rule() {
        // Encode less data than the chunk geometry calls for: the decode is
        // short, and a short decode must not pin the image to a rule.
        let data = payload();
        let strip = encode(&data).expect("encode");
        let state = CodecState::new();
        let bits = [8u16];
        let cx = context(Some(&state), &bits);
        let mut out = vec![0u8; data.len() + 32];
        let written = decode_into(&strip, &mut out, &cx).expect("short decode");
        assert_eq!(written, data.len());
        assert_eq!(state.lzw_mode(), LzwMode::Undecided);
    }

    #[test]
    fn garbage_is_reported_as_a_codec_error() {
        let bits = [8u16];
        let cx = context(None, &bits);
        let mut out = vec![0u8; 64];
        let err = decode_into(&[0xff; 8], &mut out, &cx).expect_err("invalid codes");
        assert!(err.to_string().contains("compression 5"), "{err}");
    }

    #[test]
    fn a_truncated_strip_is_an_error_not_silence() {
        let data = payload();
        let strip = encode(&data).expect("encode");
        let cut = &strip[..strip.len() / 2];
        let bits = [8u16];
        let cx = context(None, &bits);
        let mut out = vec![0u8; data.len()];
        assert!(decode_into(cut, &mut out, &cx).is_err());
    }
}
