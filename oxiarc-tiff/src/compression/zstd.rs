//! Compression 50000: Zstandard.
//!
//! The value libtiff registered for `ZSTD` (and the one GDAL writes for
//! `COMPRESS=ZSTD`). Each strip or tile is one complete zstd frame, so the
//! decode is [`oxiarc_zstd::decompress_into`] straight into the chunk buffer
//! with no intermediate `Vec`, and the encode is one frame per chunk.
//!
//! A file-level cap is not this codec's job: the frame is bounded by the size
//! of `dst`, and the TIFF pipeline charges every chunk against its
//! [`OutputBudget`](crate::OutputBudget).
//!
//! ```
//! use oxiarc_tiff::compression::{decode_into, encode, CodecContext, CodecLevel};
//! use oxiarc_tiff::{CompressionMethod, Endian};
//!
//! let pixels: Vec<u8> = (0..128u8).map(|i| i / 4).collect();
//! let cx = CodecContext::new(CompressionMethod::Zstd, 128, 1, &[8], 1, Endian::Little);
//! let chunk = encode(&pixels, &cx, CodecLevel::Level(3))?;
//! let mut out = vec![0u8; pixels.len()];
//! assert_eq!(decode_into(&chunk, &mut out, &cx)?, pixels.len());
//! assert_eq!(out, pixels);
//! # Ok::<(), oxiarc_tiff::TiffError>(())
//! ```

use super::{CodecContext, CodecLevel, codec_error};
use crate::error::Result;
use crate::tags::CompressionMethod;

/// The level libtiff uses when `ZSTD_LEVEL` is not set.
const DEFAULT_LEVEL: i32 = 9;

/// Decodes one zstd frame into `dst`.
///
/// # Errors
/// [`crate::FormatError::Codec`] for a corrupt or truncated frame (under
/// [`Leniency::Lenient`](crate::Leniency::Lenient) a truncated frame yields
/// whatever decoded, so a damaged last strip still shows).
pub fn decode_into(src: &[u8], dst: &mut [u8], cx: &CodecContext<'_>) -> Result<usize> {
    match oxiarc_zstd::decompress_into(src, dst) {
        Ok(written) => Ok(written),
        Err(error) => {
            if cx.leniency.is_lenient() {
                // Salvage: decode with an explicit cap and keep the prefix.
                if let Ok(partial) = oxiarc_zstd::decompress_with_limit(src, dst.len()) {
                    let take = partial.len().min(dst.len());
                    if let (Some(slot), Some(bytes)) = (dst.get_mut(..take), partial.get(..take)) {
                        slot.copy_from_slice(bytes);
                        return Ok(take);
                    }
                }
                return Ok(0);
            }
            Err(codec_error(CompressionMethod::Zstd, error))
        }
    }
}

/// Encodes one strip or tile as a single zstd frame.
///
/// # Errors
/// [`crate::FormatError::Codec`] if the encoder fails.
pub fn encode(src: &[u8], level: CodecLevel) -> Result<Vec<u8>> {
    let level = match level {
        CodecLevel::Level(value) => value.clamp(1, 22),
        _ => DEFAULT_LEVEL,
    };
    oxiarc_zstd::encode_all(src, level).map_err(|error| codec_error(CompressionMethod::Zstd, error))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::byteorder::Endian;
    use crate::limits::Leniency;

    fn payload() -> Vec<u8> {
        (0..8192u32).map(|i| (i / 17 % 251) as u8).collect()
    }

    fn context(bits: &[u16]) -> CodecContext<'_> {
        CodecContext::new(CompressionMethod::Zstd, 8192, 1, bits, 1, Endian::Little)
    }

    #[test]
    fn round_trips_at_every_level_shape() {
        let data = payload();
        let bits = [8u16];
        let cx = context(&bits);
        for level in [
            CodecLevel::Default,
            CodecLevel::Level(1),
            CodecLevel::Level(19),
            CodecLevel::Level(-3),
        ] {
            let chunk = encode(&data, level).expect("encode");
            let mut out = vec![0u8; data.len()];
            assert_eq!(
                decode_into(&chunk, &mut out, &cx).expect("decode"),
                data.len()
            );
            assert_eq!(out, data);
        }
    }

    #[test]
    fn a_corrupt_frame_is_an_error() {
        let data = payload();
        let bits = [8u16];
        let cx = context(&bits);
        let mut chunk = encode(&data, CodecLevel::Default).expect("encode");
        for byte in chunk.iter_mut().skip(8).take(24) {
            *byte ^= 0x5a;
        }
        let mut out = vec![0u8; data.len()];
        assert!(decode_into(&chunk, &mut out, &cx).is_err());
    }

    #[test]
    fn a_truncated_frame_never_panics_and_is_bounded() {
        let data = payload();
        let bits = [8u16];
        let chunk = encode(&data, CodecLevel::Default).expect("encode");
        let mut lenient = context(&bits);
        lenient.leniency = Leniency::Lenient;
        for cut in [1usize, 4, 16, chunk.len() / 3, chunk.len() - 1] {
            let piece = chunk.get(..cut).unwrap_or_default();
            let mut out = vec![0u8; data.len()];
            let written = decode_into(piece, &mut out, &lenient).expect("lenient salvage");
            assert!(written <= data.len());
            let strict = context(&bits);
            let mut out = vec![0u8; data.len()];
            assert!(decode_into(piece, &mut out, &strict).is_err());
        }
    }
}
