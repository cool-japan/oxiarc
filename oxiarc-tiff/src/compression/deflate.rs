//! Compression 8 and 32946: zlib-wrapped DEFLATE.
//!
//! Both tag values mean the same thing on the wire. 32946 is Adobe's
//! registered "Deflate", 8 was the earlier PKZIP-flavoured registration, and
//! every writer in the wild (libtiff, GDAL, tifffile, Pillow) puts a plain
//! zlib stream in the strip either way. This crate reads both identically and
//! writes 32946.
//!
//! # Why not `zlib_decompress_into`
//!
//! [`oxiarc_deflate::zlib::zlib_decompress_into`] reads the Adler-32 from the
//! **last four bytes of the input slice**, which is only correct when the
//! slice is exactly one zlib stream. TIFF strips are routinely padded —
//! libtiff aligns every strip offset to an even byte, and a `StripByteCounts`
//! reconstructed from the geometry can only over-estimate — so this module
//! drives [`WrappedInflate`] with [`TrailingPolicy::Stop`] instead: the
//! trailer is read where the stream actually ends, and whatever follows is
//! ignored.
//!
//! # Reused state
//!
//! A fresh `WrappedInflate` allocates a 32 KiB history window. Deflate is the
//! most common compressed TIFF there is, so the machine is cached in the
//! image's [`CodecState`](super::CodecState) and `reset()` between chunks,
//! which keeps the window allocated. Without a state the codec still works; it
//! just allocates per chunk.
//!
//! The cache is a `Pool` (`compression::pool`), not a single slot: a worker
//! takes a machine out for the length of its chunk and puts it back after, so
//! a `rayon` decode of a Deflate page runs one machine per worker instead of
//! queueing every worker behind one mutex. `reset()` before every chunk is
//! what makes that safe — it clears the whole stream state, so a machine
//! carries nothing but its allocations from the chunk before.
//!
//! ```
//! use oxiarc_tiff::compression::{decode_into, encode, CodecContext, CodecLevel};
//! use oxiarc_tiff::{CompressionMethod, Endian};
//!
//! let pixels: Vec<u8> = (0..64u8).collect();
//! let cx = CodecContext::new(CompressionMethod::Deflate, 64, 1, &[8], 1, Endian::Little);
//! let strip = encode(&pixels, &cx, CodecLevel::Level(6))?;
//! let mut out = vec![0u8; pixels.len()];
//! assert_eq!(decode_into(&strip, &mut out, &cx)?, pixels.len());
//! assert_eq!(out, pixels);
//! # Ok::<(), oxiarc_tiff::TiffError>(())
//! ```

use oxiarc_core::traits::FlushMode;
use oxiarc_deflate::wrapper::{InflateWrapper, TrailingPolicy, WrappedInflate};
use oxiarc_deflate::{InflateStatus, zlib::zlib_compress};

use super::pool::Pool;
use super::{CodecContext, CodecLevel, codec_error};
use crate::error::Result;
use crate::tags::CompressionMethod;

/// The zlib level used when the caller asked for no particular effort.
const DEFAULT_LEVEL: u8 = 6;

/// Cached [`WrappedInflate`] machines, kept across the chunks of one image.
///
/// The `bool` records which `verify_checksum` setting each cached machine was
/// built with: that knob is a consuming builder, so a change of leniency
/// rebuilds rather than mutates. Leniency is fixed for the length of one
/// decode call, so the rebuild costs at most one machine per pool entry and
/// never thrashes.
#[derive(Default)]
pub(crate) struct InflateSlot(Pool<(bool, WrappedInflate)>);

impl core::fmt::Debug for InflateSlot {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let held = self.0.inspect(|entries| {
            entries
                .iter()
                .map(|(verify, _)| *verify)
                .collect::<Vec<_>>()
        });
        f.debug_tuple("InflateSlot").field(&held).finish()
    }
}

impl InflateSlot {
    /// Runs `body` on a machine configured for `verify`, reusing a pooled one
    /// when its configuration matches.
    fn with<T>(&self, verify: bool, body: impl FnOnce(&mut WrappedInflate) -> T) -> T {
        let mut stream = match self.0.take() {
            Some((cached, stream)) if cached == verify => stream,
            // Either the pool was empty or the machine in it was built for the
            // other leniency; a rebuilt machine replaces it.
            _ => build(verify),
        };
        // `reset()` before, not after: a machine put back mid-stream by a
        // failed chunk must not hand its state to the next one.
        stream.reset();
        let out = body(&mut stream);
        self.0.put((verify, stream));
        out
    }
}

/// A decoder for one TIFF strip: zlib framing, trailing bytes tolerated.
fn build(verify: bool) -> WrappedInflate {
    WrappedInflate::new(InflateWrapper::Zlib)
        .trailing_policy(TrailingPolicy::Stop)
        .verify_checksum(verify)
}

/// Decodes one Deflate strip or tile into `dst`.
///
/// The Adler-32 is verified unless the reader is running
/// [`Leniency::Lenient`](crate::Leniency::Lenient). A strip that decodes to
/// fewer bytes than the geometry calls for is reported by its return value,
/// not by an error — the pipeline decides whether a short last strip is
/// acceptable.
///
/// # Errors
/// [`crate::FormatError::Codec`] for a corrupt stream, a bad checksum or a
/// stream that ends mid-block (unless lenient, where the bytes decoded so far
/// are returned).
pub fn decode_into(src: &[u8], dst: &mut [u8], cx: &CodecContext<'_>) -> Result<usize> {
    let verify = !cx.leniency.is_lenient();
    let lenient = cx.leniency.is_lenient();
    match cx.state {
        Some(state) => state
            .inflate()
            .with(verify, |stream| run(stream, src, dst, lenient)),
        None => run(&mut build(verify), src, dst, lenient),
    }
}

/// Drives one complete member of `src` into `dst`.
fn run(stream: &mut WrappedInflate, src: &[u8], dst: &mut [u8], lenient: bool) -> Result<usize> {
    let mut in_pos = 0usize;
    let mut out_pos = 0usize;
    let mut finished = false;
    while in_pos < src.len() && out_pos < dst.len() {
        let input = src.get(in_pos..).unwrap_or(&[]);
        let Some(output) = dst.get_mut(out_pos..) else {
            break;
        };
        match stream.inflate(input, output, FlushMode::None) {
            Ok(progress) => {
                in_pos += progress.consumed;
                out_pos += progress.produced;
                if progress.status == InflateStatus::StreamEnd {
                    finished = true;
                    break;
                }
                if progress.consumed == 0 && progress.produced == 0 {
                    break;
                }
            }
            Err(error) => {
                if lenient {
                    return Ok(out_pos);
                }
                return Err(codec_error(CompressionMethod::Deflate, error));
            }
        }
    }
    // The stream had no more input: `Finish` turns "ended mid-member" into an
    // error rather than a silent short read.
    if !finished && out_pos < dst.len() {
        let Some(output) = dst.get_mut(out_pos..) else {
            return Ok(out_pos);
        };
        match stream.inflate(&[], output, FlushMode::Finish) {
            Ok(progress) => out_pos += progress.produced,
            Err(error) => {
                if !lenient {
                    return Err(codec_error(CompressionMethod::Deflate, error));
                }
            }
        }
    }
    Ok(out_pos)
}

/// Encodes one strip or tile as a complete zlib stream.
///
/// # Errors
/// [`crate::FormatError::Codec`] if the deflater fails.
pub fn encode(src: &[u8], level: CodecLevel) -> Result<Vec<u8>> {
    let level = match level {
        CodecLevel::Level(value) => value.clamp(0, 9) as u8,
        _ => DEFAULT_LEVEL,
    };
    zlib_compress(src, level).map_err(|error| codec_error(CompressionMethod::Deflate, error))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::byteorder::Endian;
    use crate::compression::CodecState;
    use crate::limits::Leniency;

    fn payload() -> Vec<u8> {
        (0..4096u32).map(|i| (i / 13 % 256) as u8).collect()
    }

    fn context<'a>(state: Option<&'a CodecState>, bits: &'a [u16]) -> CodecContext<'a> {
        let mut cx =
            CodecContext::new(CompressionMethod::Deflate, 4096, 1, bits, 1, Endian::Little);
        cx.state = state;
        cx
    }

    #[test]
    fn round_trips_through_the_cached_machine() {
        let data = payload();
        let bits = [8u16];
        let cx = context(None, &bits);
        let strip = encode(&data, CodecLevel::Level(9)).expect("encode");
        let state = CodecState::new();
        let cx_state = context(Some(&state), &bits);
        for _ in 0..3 {
            let mut out = vec![0u8; data.len()];
            assert_eq!(
                decode_into(&strip, &mut out, &cx_state).expect("decode"),
                data.len()
            );
            assert_eq!(out, data);
        }
        let mut out = vec![0u8; data.len()];
        assert_eq!(
            decode_into(&strip, &mut out, &cx).expect("stateless"),
            data.len()
        );
        assert_eq!(out, data);
    }

    #[test]
    fn trailing_padding_is_ignored() {
        let data = payload();
        let bits = [8u16];
        let cx = context(None, &bits);
        let mut strip = encode(&data, CodecLevel::Default).expect("encode");
        // libtiff pads strips so every offset stays even; a recovered
        // StripByteCounts can over-estimate by much more than that.
        strip.extend_from_slice(&[0u8; 16]);
        strip.extend_from_slice(&[0xab; 5]);
        let mut out = vec![0u8; data.len()];
        assert_eq!(
            decode_into(&strip, &mut out, &cx).expect("padded"),
            data.len()
        );
        assert_eq!(out, data);
    }

    #[test]
    fn a_corrupt_checksum_is_an_error_unless_lenient() {
        let data = payload();
        let bits = [8u16];
        let mut strip = encode(&data, CodecLevel::Default).expect("encode");
        let last = strip.len() - 1;
        strip[last] ^= 0xff;

        let cx = context(None, &bits);
        let mut out = vec![0u8; data.len()];
        assert!(decode_into(&strip, &mut out, &cx).is_err());

        let mut lenient = context(None, &bits);
        lenient.leniency = Leniency::Lenient;
        let mut out = vec![0u8; data.len()];
        assert_eq!(
            decode_into(&strip, &mut out, &lenient).expect("lenient"),
            data.len()
        );
        assert_eq!(out, data);
    }

    #[test]
    fn a_truncated_stream_errors_and_lenient_keeps_the_prefix() {
        let data = payload();
        let bits = [8u16];
        let strip = encode(&data, CodecLevel::Default).expect("encode");
        let cut = strip.get(..strip.len() / 2).unwrap_or_default();

        let cx = context(None, &bits);
        let mut out = vec![0u8; data.len()];
        assert!(decode_into(cut, &mut out, &cx).is_err());

        let mut lenient = context(None, &bits);
        lenient.leniency = Leniency::Lenient;
        let mut out = vec![0u8; data.len()];
        let written = decode_into(cut, &mut out, &lenient).expect("partial");
        assert!(written > 0 && written < data.len());
        assert_eq!(out.get(..written), data.get(..written));
    }

    #[test]
    fn an_over_long_stream_stops_at_the_chunk_size() {
        let data = payload();
        let bits = [8u16];
        let cx = context(None, &bits);
        let strip = encode(&data, CodecLevel::Default).expect("encode");
        let mut out = vec![0u8; data.len() / 2];
        assert_eq!(
            decode_into(&strip, &mut out, &cx).expect("clipped"),
            out.len()
        );
    }

    #[test]
    fn levels_are_clamped_not_rejected() {
        let data = payload();
        let bits = [8u16];
        let cx = context(None, &bits);
        for level in [
            CodecLevel::Level(-5),
            CodecLevel::Level(42),
            CodecLevel::Default,
        ] {
            let strip = encode(&data, level).expect("encode");
            let mut out = vec![0u8; data.len()];
            assert_eq!(
                decode_into(&strip, &mut out, &cx).expect("decode"),
                data.len()
            );
            assert_eq!(out, data);
        }
    }

    #[test]
    fn the_pool_reports_the_machines_it_holds() {
        let slot = InflateSlot::default();
        assert_eq!(format!("{slot:?}"), "InflateSlot([])");
        slot.with(true, |_| ());
        assert_eq!(format!("{slot:?}"), "InflateSlot([true])");
        // A different verify setting rebuilds rather than reuses, and the
        // stale machine is not kept alongside the new one.
        slot.with(false, |stream| assert!(!stream.is_finished()));
        assert_eq!(format!("{slot:?}"), "InflateSlot([false])");
        // Back to the first setting: one machine again, rebuilt again.
        slot.with(true, |_| ());
        assert_eq!(format!("{slot:?}"), "InflateSlot([true])");
    }

    #[test]
    fn two_borrowers_get_two_machines() {
        // The pool property the `rayon` path depends on: a machine that is
        // out on loan is not handed to a second caller.
        let slot = InflateSlot::default();
        slot.with(true, |_| {
            slot.with(true, |_| ());
        });
        assert_eq!(
            format!("{slot:?}"),
            "InflateSlot([true, true])",
            "the nested borrow built its own machine and both came back"
        );
    }

    #[test]
    fn a_pooled_decode_matches_an_unpooled_one() {
        let data = payload();
        let strip = encode(&data, CodecLevel::Default).expect("encode");
        let state = CodecState::new();
        let bits = [8u16];
        let pooled = context(Some(&state), &bits);
        let fresh = context(None, &bits);
        for _ in 0..4 {
            let mut with_pool = vec![0u8; data.len()];
            let mut without = vec![0u8; data.len()];
            assert_eq!(
                decode_into(&strip, &mut with_pool, &pooled).expect("pooled"),
                data.len()
            );
            assert_eq!(
                decode_into(&strip, &mut without, &fresh).expect("fresh"),
                data.len()
            );
            assert_eq!(with_pool, without);
            assert_eq!(with_pool, data);
        }
    }
}
