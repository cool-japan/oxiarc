//! The zlib rules PNG adds on top of RFC 1950, and a bounded one-shot
//! inflate for the compressed ancillary chunks.
//!
//! ISO/IEC 15948 clause 10.3 is stricter than RFC 1950: the compression method
//! must be DEFLATE, the window must be at most 32768 bytes (`CINFO <= 7`) and a
//! preset dictionary is **forbidden**. A generic zlib reader accepts all three
//! violations, so the checks live here and are applied before the first byte
//! reaches the inflater.

use oxiarc_core::traits::FlushMode;
use oxiarc_deflate::{InflateStatus, InflateWrapper, TrailingPolicy, WrappedInflate};

use crate::error::{DecodingError, FormatErrorKind};

/// Validate the two-byte zlib header of an image-data stream against the extra
/// rules PNG imposes.
///
/// ```
/// use oxiarc_png::zlib::validate_zlib_header;
/// assert!(validate_zlib_header(0x78, 0x9c).is_ok());
/// // CINFO 8 asks for a 64 KiB window, which PNG forbids.
/// assert!(validate_zlib_header(0x88, 0x1d).is_err());
/// // FDICT set: preset dictionaries are forbidden in IDAT/fdAT.
/// assert!(validate_zlib_header(0x78, 0xbb).is_err());
/// ```
pub fn validate_zlib_header(cmf: u8, flg: u8) -> Result<(), DecodingError> {
    let cm = cmf & 0x0F;
    if cm != 8 {
        return Err(FormatErrorKind::InvalidZlibCompressionMethod { cm }.into());
    }
    let cinfo = cmf >> 4;
    if cinfo > 7 {
        return Err(FormatErrorKind::InvalidZlibWindowSize { cinfo }.into());
    }
    if flg & 0x20 != 0 {
        return Err(FormatErrorKind::ZlibPresetDictionaryForbidden.into());
    }
    if (u16::from(cmf) * 256 + u16::from(flg)) % 31 != 0 {
        return Err(FormatErrorKind::InvalidZlibHeaderCheck.into());
    }
    Ok(())
}

/// Inflate a complete zlib stream, refusing to produce more than `limit` bytes.
///
/// Used for `iCCP`, `zTXt` and `iTXt` payloads, all of which are attacker
/// controlled and none of which has a size declared up front. The limit is
/// enforced *inside* the inflater, so a compression bomb never allocates.
///
/// ```
/// use oxiarc_png::zlib::inflate_zlib_capped;
/// let compressed = oxiarc_deflate::zlib_compress(b"profile bytes", 6).expect("compress");
/// let out = inflate_zlib_capped(&compressed, 1024).expect("inflate");
/// assert_eq!(out, b"profile bytes");
/// assert!(inflate_zlib_capped(&compressed, 4).is_err());
/// ```
pub fn inflate_zlib_capped(data: &[u8], limit: usize) -> Result<Vec<u8>, DecodingError> {
    if data.len() >= 2 {
        validate_zlib_header(data[0], data[1])?;
    }
    // The bound is enforced here rather than through the inflater's own
    // `with_max_output`, so that hitting it is always reported as
    // `LimitsExceeded` instead of as a latched stream fault. Memory stays
    // bounded either way: the inflater can never write more than `scratch`.
    let mut inflate = WrappedInflate::new(InflateWrapper::Zlib)
        .verify_checksum(false)
        .trailing_policy(TrailingPolicy::Stop);
    let mut out: Vec<u8> = Vec::new();
    let mut scratch = vec![0u8; 8192];
    let mut consumed = 0usize;
    loop {
        let progress = inflate
            .inflate(&data[consumed..], &mut scratch, FlushMode::Finish)
            .map_err(|err| {
                DecodingError::from(FormatErrorKind::CorruptFlateStream {
                    err: err.to_string(),
                })
            })?;
        consumed += progress.consumed;
        if progress.produced > 0 {
            if out.len() + progress.produced > limit {
                return Err(DecodingError::LimitsExceeded);
            }
            out.extend_from_slice(&scratch[..progress.produced]);
        }
        match progress.status {
            InflateStatus::StreamEnd => return Ok(out),
            InflateStatus::NeedOutput => {
                if progress.produced == 0 && progress.consumed == 0 {
                    return Err(FormatErrorKind::CorruptFlateStream {
                        err: "inflate made no progress".to_string(),
                    }
                    .into());
                }
            }
            _ => {
                // `NeedInput` (and any future status) with no progress means
                // the stream ended in the middle of a member.
                if progress.consumed == 0 && progress.produced == 0 {
                    return Err(FormatErrorKind::CorruptFlateStream {
                        err: "compressed stream ended prematurely".to_string(),
                    }
                    .into());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_every_conformant_header() {
        // All 8 legal CINFO values with FLEVEL 0..=3 and the check bits fixed up.
        for cinfo in 0..=7u8 {
            let cmf = (cinfo << 4) | 8;
            for flevel in 0..4u8 {
                let mut flg = flevel << 6;
                let rem = (u16::from(cmf) * 256 + u16::from(flg)) % 31;
                if rem != 0 {
                    flg += (31 - rem) as u8;
                }
                assert!(
                    validate_zlib_header(cmf, flg).is_ok(),
                    "cmf={cmf:#04x} flg={flg:#04x}"
                );
            }
        }
    }

    #[test]
    fn rejects_the_three_png_specific_violations() {
        assert!(matches!(
            validate_zlib_header(0x79, 0x9b).unwrap_err().format_kind(),
            Some(FormatErrorKind::InvalidZlibCompressionMethod { cm: 9 })
        ));
        assert!(matches!(
            validate_zlib_header(0x88, 0x1d).unwrap_err().format_kind(),
            Some(FormatErrorKind::InvalidZlibWindowSize { cinfo: 8 })
        ));
        assert!(matches!(
            validate_zlib_header(0x78, 0xbb).unwrap_err().format_kind(),
            Some(FormatErrorKind::ZlibPresetDictionaryForbidden)
        ));
        assert!(matches!(
            validate_zlib_header(0x78, 0x9d).unwrap_err().format_kind(),
            Some(FormatErrorKind::InvalidZlibHeaderCheck)
        ));
    }

    #[test]
    fn capped_inflate_round_trips_and_bounds() {
        let payload = vec![b'a'; 100_000];
        let compressed = oxiarc_deflate::zlib_compress(&payload, 9).expect("compress");
        assert!(compressed.len() < 1000, "bomb ratio is what we are testing");
        let out = inflate_zlib_capped(&compressed, 200_000).expect("inflate");
        assert_eq!(out, payload);
        assert!(matches!(
            inflate_zlib_capped(&compressed, 4096),
            Err(DecodingError::LimitsExceeded)
        ));
    }

    #[test]
    fn capped_inflate_rejects_truncated_and_corrupt_streams() {
        let compressed = oxiarc_deflate::zlib_compress(b"hello", 6).expect("compress");
        let mut truncated = compressed.clone();
        truncated.truncate(compressed.len() / 2);
        assert!(inflate_zlib_capped(&truncated, 1024).is_err());
        assert!(inflate_zlib_capped(&[], 1024).is_err());
        assert!(inflate_zlib_capped(&[0x78], 1024).is_err());
        // A valid zlib header followed by a block with the reserved BTYPE 3.
        let reserved_btype = [0x78u8, 0x01, 0x07];
        assert!(validate_zlib_header(0x78, 0x01).is_ok());
        assert!(inflate_zlib_capped(&reserved_btype, 1024).is_err());
    }
}
