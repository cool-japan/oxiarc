//! Compression 50000: Zstandard.
//!
//! The value libtiff registered for `ZSTD` (and the one GDAL writes for
//! `COMPRESS=ZSTD`). Each strip or tile is one complete zstd frame, decoded
//! straight into the chunk buffer with no intermediate `Vec`, and the encode
//! is one frame per chunk.
//!
//! A file-level cap is not this codec's job: the frame is bounded by the size
//! of `dst`, and the TIFF pipeline charges every chunk against its
//! [`OutputBudget`](crate::OutputBudget).
//!
//! # Reused state
//!
//! A fresh [`ZstdStream`] allocates a sliding window, a literals table and a
//! sequence buffer per frame, and a TIFF page is one frame **per strip**. The
//! decoder is therefore cached in the image's
//! [`CodecState`](super::CodecState) and `reset()` between chunks, which keeps
//! every one of those allocations. Without a state the codec still works; it
//! just builds a decoder per chunk, exactly as
//! [`oxiarc_zstd::decompress_into`] does.
//!
//! The cached decoder is configured to match that free function *exactly* —
//! `with_multi_frame(false)` so trailing padding after a strip's frame is
//! never read as a second frame, an unrestricted window ceiling, and an
//! output budget of `dst.len()` refreshed per chunk — so a reused decode and
//! a fresh one cannot diverge. `tests/codec_reuse.rs` asserts that over a
//! corpus rather than leaving it to inspection.
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

use oxiarc_core::traits::FlushMode;
use oxiarc_zstd::{ZstdStatus, ZstdStream};

use super::pool::Pool;
use super::{CodecContext, CodecLevel, codec_error};
use crate::error::Result;
use crate::tags::CompressionMethod;

/// The level libtiff uses when `ZSTD_LEVEL` is not set.
const DEFAULT_LEVEL: i32 = 9;

/// Cached [`ZstdStream`]s, kept across the chunks of one image.
///
/// A [`Pool`] rather than a single slot, deliberately: the lock is held only
/// while a decoder is taken and put back, never across the decode itself, so
/// a parallel decode of a ZSTD page keeps one decoder per worker instead of
/// serialising every worker behind one. Nothing crosses between chunks except
/// the allocations — [`ZstdStream::reset`] clears all state, including the
/// sticky fault latch — so which decoder a chunk gets cannot change what it
/// decodes to.
#[derive(Default)]
pub(crate) struct ZstdSlot(Pool<ZstdStream>);

impl core::fmt::Debug for ZstdSlot {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let held = self.0.inspect(<[ZstdStream]>::len);
        f.debug_tuple("ZstdSlot").field(&held).finish()
    }
}

impl ZstdSlot {
    /// Runs `body` on a decoder configured for a chunk of `cap` bytes.
    fn with<T>(&self, cap: u64, body: impl FnOnce(&mut ZstdStream) -> T) -> T {
        let mut stream = self.0.take().unwrap_or_else(build);
        // Reset first: it clears the sticky fault latch a previous corrupt
        // strip may have left, and it keeps every buffer allocated.
        stream.reset();
        // The budget is the only knob that changes per chunk (the last strip
        // of a page is shorter), and it is a consuming builder, hence the
        // move out of the pool and back.
        let mut stream = stream.with_max_output(cap);
        let out = body(&mut stream);
        self.0.put(stream);
        out
    }
}

/// A decoder configured exactly like [`oxiarc_zstd::decompress_into`]'s.
fn build() -> ZstdStream {
    ZstdStream::new()
        // `usize::MAX`: a strip's window is bounded by the chunk, not by the
        // 8 MiB default, and `decompress_into` lifts the ceiling the same way.
        .with_max_window(usize::MAX)
        // One frame per strip. Anything after it is TIFF padding, never a
        // second frame.
        .with_multi_frame(false)
}

/// Drives one complete frame of `src` into `dst`.
///
/// A transcription of [`oxiarc_zstd::decompress_into`]'s loop, over a decoder
/// the caller owns rather than one built per call.
fn run(stream: &mut ZstdStream, src: &[u8], dst: &mut [u8]) -> Result<usize> {
    let mut written = 0usize;
    let mut pos = 0usize;
    loop {
        let (input, output) = match (src.get(pos..), dst.get_mut(written..)) {
            (Some(input), Some(output)) => (input, output),
            _ => return Ok(written),
        };
        let progress = stream
            .decode(input, output, FlushMode::Finish)
            .map_err(|error| codec_error(CompressionMethod::Zstd, error))?;
        pos += progress.consumed;
        written += progress.produced;
        match progress.status {
            ZstdStatus::StreamEnd => return Ok(written),
            ZstdStatus::NeedOutput => {
                return Err(codec_error(
                    CompressionMethod::Zstd,
                    format_args!(
                        "the frame regenerates more than the {} bytes this chunk holds",
                        dst.len()
                    ),
                ));
            }
            // `NeedInput`, and anything a later `oxiarc-zstd` adds to this
            // `#[non_exhaustive]` enum: go round again, but never spin.
            _ => {
                if progress.consumed == 0 && progress.produced == 0 {
                    return Err(codec_error(
                        CompressionMethod::Zstd,
                        "zstd decoder made no progress",
                    ));
                }
            }
        }
    }
}

/// Decodes one zstd frame into `dst`.
///
/// # Errors
/// [`crate::FormatError::Codec`] for a corrupt or truncated frame (under
/// [`Leniency::Lenient`](crate::Leniency::Lenient) a truncated frame yields
/// whatever decoded, so a damaged last strip still shows).
pub fn decode_into(src: &[u8], dst: &mut [u8], cx: &CodecContext<'_>) -> Result<usize> {
    let cap = dst.len() as u64;
    let decoded = match cx.state {
        Some(state) => state.zstd().with(cap, |stream| run(stream, src, dst)),
        None => run(&mut build().with_max_output(cap), src, dst),
    };
    match decoded {
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
            Err(error)
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
