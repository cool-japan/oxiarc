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
//! # Three dialects, and how one strip is assigned to one of them
//!
//! Two deviations from the TIFF 6.0 §13 stream above exist in the wild, and
//! libtiff has a named reader for each:
//!
//! | dialect | packing | code-width change | libtiff reader |
//! |---|---|---|---|
//! | standard | MSB-first | early | `LZWDecode` |
//! | old-style | MSB-first | late | (none — see below) |
//! | compat | **LSB-first** | late | `LZWDecodeCompat` |
//!
//! The **compat** dialect is decided by a *sniff*, exactly as libtiff's
//! `LZWPreDecode` decides it: a standard strip opens with the `ClearCode`
//! (256) packed MSB-first into nine bits, so its first byte is `0x80`; the
//! same code packed LSB-first puts `0x00` in the first byte and its ninth bit
//! in bit 0 of the second. libtiff therefore tests that `raw[0]` is zero and
//! that bit 0 of `raw[1]` is set, switching decoders on it, and so does
//! [`is_compat_lsb`]. The test runs on the bytes *after* the `FillOrder`
//! reversal, which is where libtiff runs it too (LZW is not in
//! [`handles_fill_order`](super::handles_fill_order), so the chunk pipeline
//! has already reversed).
//!
//! The **old-style** dialect cannot be sniffed — it is byte-for-byte
//! indistinguishable from a standard strip until the first code that would
//! have changed width — so it is found by *retry*: decode with the standard
//! rule and, **when that errors**, retry with
//! [`LzwConfig::TIFF_OLD_STYLE`].
//!
//! Whichever dialect explains the strip is cached in the image's
//! [`CodecState`](super::CodecState), so the sniff and the failed attempt
//! cost one strip rather than every strip.
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

/// Whether `src` is one of libtiff's pre-1993 `LZWDecodeCompat` strips.
///
/// This is `tif_lzw.c`'s own two-byte test, transcribed: a strip whose first
/// byte is zero and whose second byte has bit 0 set cannot be a `ClearCode`
/// packed MSB-first (that is `0x80`), but is exactly one packed LSB-first.
/// libtiff needs `tif_rawcc >= 2` for the test to be meaningful, so a
/// shorter strip is never compat.
///
/// ```
/// use oxiarc_tiff::compression::lzw::is_compat_lsb;
///
/// // ClearCode (256) in nine LSB-first bits: 0x00, then bit 8 in bit 0.
/// assert!(is_compat_lsb(&[0x00, 0x01, 0x00]));
/// // The same code MSB-first opens the standard dialect.
/// assert!(!is_compat_lsb(&[0x80, 0x00, 0x00]));
/// // Two bytes are the minimum; one cannot decide anything.
/// assert!(!is_compat_lsb(&[0x00]));
/// ```
#[must_use]
pub fn is_compat_lsb(src: &[u8]) -> bool {
    match (src.first(), src.get(1)) {
        (Some(0), Some(second)) => second & 1 != 0,
        _ => false,
    }
}

/// Decodes one LZW strip or tile into `dst`.
///
/// # Errors
/// [`crate::FormatError::Codec`] when no dialect explains the stream; the
/// *standard* rule's message is the one reported, because that is the rule
/// 99 % of files follow.
pub fn decode_into(src: &[u8], dst: &mut [u8], cx: &CodecContext<'_>) -> Result<usize> {
    match cx.state.map(super::CodecState::lzw_mode) {
        Some(LzwMode::OldStyle) => old_style(src, dst),
        Some(LzwMode::CompatLsb) => compat_lsb(src, dst),
        Some(LzwMode::Standard) => standard(src, dst),
        Some(LzwMode::Undecided) | None => decide(src, dst, cx),
    }
}

/// Records `mode` on the image's state, when it has one.
fn remember(cx: &CodecContext<'_>, mode: LzwMode) {
    if let Some(state) = cx.state {
        state.set_lzw_mode(mode);
    }
}

/// Runs one dialect into scratch, so a failure cannot corrupt bytes an
/// earlier attempt already wrote into `dst` (`oxiarc-lzw`'s documented
/// recipe for a fallback decode).
fn retry(src: &[u8], dst: &mut [u8], config: LzwConfig) -> Option<usize> {
    let mut scratch = vec![0u8; dst.len()];
    let written = decompress_into(src, &mut scratch, config).ok()?;
    dst.copy_from_slice(&scratch);
    Some(written)
}

/// Picks the dialect this strip is written in.
///
/// The compat sniff comes first because it is decisive at the byte level: a
/// strip that opens with an LSB-first `ClearCode` is not a standard strip, so
/// there is nothing to weigh. Only when it does not fire does the
/// standard-then-old-style retry run.
fn decide(src: &[u8], dst: &mut [u8], cx: &CodecContext<'_>) -> Result<usize> {
    if is_compat_lsb(src) {
        match decompress_into(src, dst, LzwConfig::TIFF_COMPAT_LSB) {
            Ok(written) => {
                remember(cx, LzwMode::CompatLsb);
                return Ok(written);
            }
            Err(compat_error) => {
                // The sniff can only be wrong for a strip that does not open
                // with a ClearCode at all, which libtiff mis-reads too. Give
                // the standard rule a chance rather than inheriting that bug,
                // through scratch so the failed attempt cannot show through.
                if let Some(written) = retry(src, dst, LzwConfig::TIFF) {
                    remember(cx, LzwMode::Standard);
                    return Ok(written);
                }
                return Err(codec_error(CompressionMethod::Lzw, compat_error));
            }
        }
    }
    let standard_error = match decompress_tiff_into(src, dst) {
        Ok(written) => {
            // Only a strip that filled the whole chunk proves the rule: a
            // short one is a short strip, whichever rule produced it.
            if written == dst.len() {
                remember(cx, LzwMode::Standard);
            }
            return Ok(written);
        }
        Err(error) => error,
    };
    match retry(src, dst, LzwConfig::TIFF_OLD_STYLE) {
        Some(written) => {
            remember(cx, LzwMode::OldStyle);
            Ok(written)
        }
        None => Err(codec_error(CompressionMethod::Lzw, standard_error)),
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

/// libtiff's `LZWDecodeCompat`: LSB-first packing, late code-width change.
fn compat_lsb(src: &[u8], dst: &mut [u8]) -> Result<usize> {
    decompress_into(src, dst, LzwConfig::TIFF_COMPAT_LSB)
        .map_err(|error| codec_error(CompressionMethod::Lzw, error))
}

/// Encodes one strip or tile.
///
/// Always the standard rule: `compress_tiff` emits the leading `ClearCode` and
/// re-clears at entry 4094, which is what libtiff, Pillow and GDAL all expect.
/// Neither deviant dialect is ever written — libtiff does not write them
/// either, and a file this crate produces has to be readable by every reader.
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
    fn a_compat_lsb_strip_is_sniffed_decoded_and_cached() {
        let data = payload();
        let strip = oxiarc_lzw::compress(&data, LzwConfig::TIFF_COMPAT_LSB).expect("compat encode");
        // The sniff's whole basis: an LSB-first ClearCode opens the strip.
        assert_eq!(strip.first(), Some(&0x00), "compat strips open with 0x00");
        assert_eq!(strip.get(1).map(|b| b & 1), Some(1), "bit 8 of the code");
        assert!(is_compat_lsb(&strip));

        let state = CodecState::new();
        let bits = [8u16];
        let cx = context(Some(&state), &bits);
        let mut out = vec![0u8; data.len()];
        assert_eq!(
            decode_into(&strip, &mut out, &cx).expect("compat decode"),
            data.len()
        );
        assert_eq!(out, data);
        assert!(state.lzw_is_compat_lsb(), "the dialect must be cached");
        assert!(!state.lzw_is_old_style(), "compat is not the MSB late rule");

        // A second strip now takes the cached branch: no sniff, no retry.
        let mut again = vec![0u8; data.len()];
        assert_eq!(
            decode_into(&strip, &mut again, &cx).expect("cached decode"),
            data.len()
        );
        assert_eq!(again, data);
    }

    #[test]
    fn a_compat_strip_is_not_decodable_by_the_other_two_dialects() {
        // Without the sniff this strip would be silently wrong or an error,
        // which is what makes the sniff load-bearing rather than cosmetic.
        let data = payload();
        let strip = oxiarc_lzw::compress(&data, LzwConfig::TIFF_COMPAT_LSB).expect("compat encode");
        let mut out = vec![0u8; data.len()];
        let standard = decompress_tiff_into(&strip, &mut out);
        assert!(
            standard.is_err() || out != data,
            "the standard rule must not reproduce a compat strip"
        );
        let mut out = vec![0u8; data.len()];
        let old = decompress_into(&strip, &mut out, LzwConfig::TIFF_OLD_STYLE);
        assert!(
            old.is_err() || out != data,
            "the MSB late rule must not reproduce a compat strip"
        );
    }

    #[test]
    fn the_sniff_is_libtiffs_two_byte_test_exactly() {
        // Hand-built prefixes, no encoder involved: only `0x00` followed by an
        // odd byte is compat, which is `tif_lzw.c`'s `LZWPreDecode` rule.
        assert!(is_compat_lsb(&[0x00, 0x01]));
        assert!(is_compat_lsb(&[0x00, 0xff, 0x12]));
        assert!(!is_compat_lsb(&[0x00, 0x02, 0x12]));
        assert!(!is_compat_lsb(&[0x00, 0x00]));
        assert!(!is_compat_lsb(&[0x80, 0x01]));
        assert!(!is_compat_lsb(&[0x00]));
        assert!(!is_compat_lsb(&[]));
    }

    #[test]
    fn a_standard_strip_is_never_sniffed_as_compat() {
        let data = payload();
        let strip = encode(&data).expect("encode");
        assert_eq!(strip.first(), Some(&0x80), "the MSB ClearCode");
        assert!(!is_compat_lsb(&strip));
        let state = CodecState::new();
        let bits = [8u16];
        let cx = context(Some(&state), &bits);
        let mut out = vec![0u8; data.len()];
        decode_into(&strip, &mut out, &cx).expect("decode");
        assert_eq!(out, data);
        assert!(!state.lzw_is_compat_lsb());
        assert_eq!(state.lzw_mode(), LzwMode::Standard);
    }

    #[test]
    fn a_hand_built_compat_strip_decodes_to_its_literals() {
        // ClearCode(256), 'A'(65), 'B'(66), 'C'(67), EOI(257), packed
        // LSB-first at nine bits each, assembled bit by bit here so the test
        // does not depend on this crate's own encoder.
        let mut bits: Vec<u8> = Vec::new();
        for code in [256u16, 65, 66, 67, 257] {
            for index in 0..9 {
                bits.push(((code >> index) & 1) as u8);
            }
        }
        let mut strip = vec![0u8; bits.len().div_ceil(8)];
        for (index, bit) in bits.iter().enumerate() {
            if *bit == 1 {
                strip[index / 8] |= 1 << (index % 8);
            }
        }
        assert!(is_compat_lsb(&strip), "{strip:02x?}");
        let state = CodecState::new();
        let bits_per_sample = [8u16];
        let cx = context(Some(&state), &bits_per_sample);
        let mut out = vec![0u8; 3];
        assert_eq!(decode_into(&strip, &mut out, &cx).expect("decode"), 3);
        assert_eq!(&out, b"ABC");
        assert!(state.lzw_is_compat_lsb());
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
