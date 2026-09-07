//! Compression 7 (TTN2 JPEG) and 6 (old-style JPEG).
//!
//! # How TIFF and JPEG divide the work
//!
//! TTN2 keeps colour *out* of the JPEG stream: a strip carries no `JFIF` and
//! no `Adobe` marker, and `PhotometricInterpretation` says what the components
//! mean. This module therefore decodes with
//! [`DecodeOptions::raw_components`](oxiarc_jpeg::DecodeOptions::raw_components)
//! and lets the TIFF colour layer apply `YCbCrCoefficients` and
//! `ReferenceBlackWhite` — the one exception being a stream that *does* carry
//! an `APP14` transform the photometric does not account for, where the JPEG
//! layer has to undo its own transform first:
//!
//! | `PhotometricInterpretation` | `APP14` transform | what this module does |
//! |---|---|---|
//! | `YCbCr` (6) | any | raw components; TIFF applies its matrix |
//! | `RGB` (2) | absent or 0 | raw components (they are already RGB) |
//! | `RGB` (2) | 1 (YCbCr) | let the JPEG layer convert to RGB |
//! | `Separated` (5) | absent or 0 | raw components (CMYK, **never inverted**) |
//! | `Separated` (5) | 2 (YCCK) | let the JPEG layer convert to CMYK |
//! | `MinIsBlack`/`MinIsWhite`/palette | any | raw components |
//!
//! # Subsampling
//!
//! The `SOF` sampling factors are authoritative (TTN2), not
//! `YCbCrSubSampling` (530). A disagreement is
//! [`FormatError::JpegSubsamplingMismatch`] under
//! [`Leniency::Strict`](crate::Leniency::Strict) and the frame header wins
//! otherwise — a file with no tag 530 defaults to 2x2 in the TIFF model, so
//! rejecting every 4:4:4 stream in such a file would break real images.
//!
//! Because the JPEG decoder upsamples chroma while it renders, a JPEG chunk
//! produces **full-resolution interleaved components**, not TIFF subsampling
//! units. The chunk pipeline knows this through
//! [`expands_subsampling`](super::expands_subsampling) and skips its own
//! expansion step.
//!
//! ```
//! use oxiarc_tiff::compression::{decode_into, encode, CodecContext, CodecLevel};
//! use oxiarc_tiff::{CompressionMethod, Endian};
//!
//! // An 8x8 greyscale tile, encoded and read back.
//! let pixels: Vec<u8> = (0..64u32).map(|i| (i * 4) as u8).collect();
//! let cx = CodecContext::new(CompressionMethod::Jpeg, 8, 8, &[8], 1, Endian::Little);
//! let chunk = encode(&pixels, &cx, CodecLevel::Level(95))?;
//! let mut out = vec![0u8; pixels.len()];
//! assert_eq!(decode_into(&chunk, &mut out, &cx)?, pixels.len());
//! // JPEG is lossy: at quality 95 every sample lands close to the original.
//! for (got, want) in out.iter().zip(pixels.iter()) {
//!     assert!(got.abs_diff(*want) <= 12, "{got} vs {want}");
//! }
//! # Ok::<(), oxiarc_tiff::TiffError>(())
//! ```

mod encode;
mod ojpeg;
mod scan;

use oxiarc_jpeg::{DecodeOptions, TableSet, decode_abbreviated_into, decode_abbreviated_into_u16};

use super::{CodecContext, CodecLevel, codec_error};
use crate::error::{FormatError, Result, TiffError, UnsupportedError};
use crate::tags::{PhotometricInterpretation, SampleFormat};

/// The quality used when the caller asked for no particular effort.
///
/// libtiff's `JPEGQUALITY` default.
const DEFAULT_QUALITY: u8 = 75;

/// Decodes one TTN2 JPEG strip or tile into `dst`.
///
/// # Errors
/// [`FormatError::Codec`] for a stream the JPEG decoder rejects,
/// [`FormatError::JpegSubsamplingMismatch`] under
/// [`Leniency::Strict`](crate::Leniency::Strict), and
/// [`UnsupportedError::Conversion`] for a hierarchical frame.
pub fn decode_into(src: &[u8], dst: &mut [u8], cx: &CodecContext<'_>) -> Result<usize> {
    let facts = scan::scan(src);
    decode_stream(src, dst, cx, &facts, cx.jpeg_tables)
}

/// Decodes one old-style (compression 6) chunk into `dst`.
///
/// # Errors
/// [`UnsupportedError::OldJpeg`] when the 512-521 tags do not describe a
/// reconstructable frame, plus everything [`decode_into`] can report.
pub fn decode_old_into(src: &[u8], dst: &mut [u8], cx: &CodecContext<'_>) -> Result<usize> {
    ojpeg::decode_into(src, dst, cx)
}

/// Decodes a stream that is already assembled, with `tables` out of band.
fn decode_stream(
    src: &[u8],
    dst: &mut [u8],
    cx: &CodecContext<'_>,
    facts: &scan::FrameFacts,
    tables: Option<&[u8]>,
) -> Result<usize> {
    if !facts.has_frame {
        return Err(codec_error(
            cx.compression,
            "the chunk carries no SOF marker",
        ));
    }
    if !facts.is_decodable_frame() {
        return Err(TiffError::Unsupported(UnsupportedError::Conversion(
            "hierarchical JPEG (SOF5/6/7/13/14/15) has no reference decoder to match",
        )));
    }
    check_subsampling(cx, facts)?;
    let parsed = match tables {
        Some(bytes) => {
            Some(TableSet::parse(bytes).map_err(|error| codec_error(cx.compression, error))?)
        }
        None => None,
    };
    let transform = facts
        .adobe_transform
        .or_else(|| tables.map(scan::scan).and_then(|f| f.adobe_transform));
    let options = DecodeOptions {
        raw_components: !needs_jpeg_colour(cx.photometric, transform),
        tolerate_truncated: !cx.leniency.is_strict(),
        ..DecodeOptions::default()
    };

    if facts.precision > 8 {
        return decode_wide(src, dst, cx, facts, parsed.as_ref(), &options);
    }

    let components = facts.components.len().max(1);
    let frame_width = usize::from(facts.width);
    let frame_height = usize::from(facts.height);
    let row = frame_width
        .checked_mul(components)
        .ok_or(TiffError::IntOverflow)?;
    let need = row
        .checked_mul(frame_height)
        .ok_or(TiffError::IntOverflow)?;

    if frame_width == cx.width && need <= dst.len() {
        // The common case: the frame is exactly the chunk, so the decoder
        // writes straight into the chunk buffer.
        let info = decode_abbreviated_into(parsed.as_ref(), src, &options, dst)
            .map_err(|error| codec_error(cx.compression, error))?;
        let produced = usize::from(info.width)
            .saturating_mul(usize::from(info.height))
            .saturating_mul(info.output_components().max(1));
        return Ok(produced.min(dst.len()));
    }

    // A frame that disagrees with the chunk geometry: decode into scratch and
    // copy the overlap row by row so a wrong `ImageWidth` cannot shear the
    // image. `need` comes straight out of the `SOF` marker, so it is guarded
    // like every other file-driven allocation -- a strip claiming
    // 65535x65535x4 would otherwise ask for 17 GiB before decoding a byte.
    let mut scratch = crate::limits::Limits::checked_alloc::<u8>(need, cx.max_scratch_bytes)?;
    let info = decode_abbreviated_into(parsed.as_ref(), src, &options, &mut scratch)
        .map_err(|error| codec_error(cx.compression, error))?;
    let out_row = cx
        .width
        .checked_mul(components)
        .ok_or(TiffError::IntOverflow)?;
    let rows = frame_height.min(usize::from(info.height)).min(cx.height);
    let take = row.min(out_row);
    for y in 0..rows {
        let (Some(source), Some(target)) = (
            scratch.get(y * row..y * row + take),
            dst.get_mut(y * out_row..y * out_row + take),
        ) else {
            break;
        };
        target.copy_from_slice(source);
    }
    Ok(rows.saturating_mul(out_row).min(dst.len()))
}

/// Decodes a frame whose precision is 9..=16 bits.
fn decode_wide(
    src: &[u8],
    dst: &mut [u8],
    cx: &CodecContext<'_>,
    facts: &scan::FrameFacts,
    tables: Option<&TableSet>,
    options: &DecodeOptions,
) -> Result<usize> {
    let components = facts.components.len().max(1);
    let samples = usize::from(facts.width)
        .checked_mul(usize::from(facts.height))
        .and_then(|n| n.checked_mul(components))
        .ok_or(TiffError::IntOverflow)?;
    // `samples` is `SOF` width x height x components: guarded for the same
    // reason as the 8-bit scratch above, and at two bytes a sample the
    // unguarded worst case is twice as large.
    let mut wide = crate::limits::Limits::checked_alloc::<u16>(samples, cx.max_scratch_bytes)?;
    decode_abbreviated_into_u16(tables, src, options, &mut wide)
        .map_err(|error| codec_error(cx.compression, error))?;

    let bits = cx.bits_per_sample.first().copied().unwrap_or(16);
    let per_row = usize::from(facts.width).saturating_mul(components);
    let row_bytes = crate::sample::packed_row_bytes(cx.bits_per_sample, per_row) as usize;
    let mut native = vec![0u8; per_row * 2];
    let mut produced = 0usize;
    for y in 0..usize::from(facts.height) {
        let Some(source) = wide.get(y * per_row..(y + 1) * per_row) else {
            break;
        };
        for (slot, value) in native.chunks_exact_mut(2).zip(source.iter()) {
            // `pack_row` reads native-endian slots; the packer then writes the
            // file's layout.
            slot.copy_from_slice(&value.to_ne_bytes());
        }
        let Some(target) = dst.get_mut(y * row_bytes..(y + 1) * row_bytes) else {
            break;
        };
        if bits == 16 {
            for (slot, value) in target.chunks_exact_mut(2).zip(source.iter()) {
                slot.copy_from_slice(&cx.endian.put_u16(*value));
            }
        } else {
            crate::sample::pack_row(
                &native,
                cx.bits_per_sample,
                per_row,
                crate::sample::SampleType::resolve(bits, SampleFormat::Uint)?,
                target,
            )?;
        }
        produced += row_bytes;
    }
    Ok(produced.min(dst.len()))
}

/// Whether the JPEG layer must undo its own colour transform.
fn needs_jpeg_colour(photometric: PhotometricInterpretation, transform: Option<u8>) -> bool {
    match photometric {
        // TIFF owns the YCbCr matrix through tags 529 and 532.
        PhotometricInterpretation::YCbCr => false,
        PhotometricInterpretation::Rgb => transform == Some(1),
        PhotometricInterpretation::Separated => transform == Some(2),
        _ => false,
    }
}

/// Compares the frame's sampling factors with `YCbCrSubSampling`.
fn check_subsampling(cx: &CodecContext<'_>, facts: &scan::FrameFacts) -> Result<()> {
    if cx.photometric != PhotometricInterpretation::YCbCr || facts.components.len() < 3 {
        return Ok(());
    }
    let (h, v) = facts.max_sampling();
    let declared = cx.ycbcr_subsampling;
    if (u16::from(h), u16::from(v)) != declared && cx.leniency.is_strict() {
        return Err(TiffError::Format(FormatError::JpegSubsamplingMismatch {
            declared,
            coded: (u16::from(h), u16::from(v)),
        }));
    }
    Ok(())
}

/// Builds the frame layout one chunk of this image needs.
fn layout(cx: &CodecContext<'_>) -> Result<encode::Layout> {
    let width = u16::try_from(cx.width).map_err(|_| TiffError::IntOverflow)?;
    let height = u16::try_from(cx.height).map_err(|_| TiffError::IntOverflow)?;
    let count = usize::from(cx.samples_per_pixel).max(1);
    if count > 4 {
        return Err(TiffError::Unsupported(UnsupportedError::Conversion(
            "JPEG frames carry at most four components",
        )));
    }
    if cx.bits_per_sample.iter().any(|bits| *bits != 8) {
        return Err(TiffError::Unsupported(UnsupportedError::BitsPerSample(
            cx.bits_per_sample.to_vec(),
        )));
    }
    let component = |id: u8, h: u8, v: u8, chroma: bool| encode::Component {
        id,
        h,
        v,
        quant: u8::from(chroma),
        dc: u8::from(chroma),
        ac: u8::from(chroma),
    };
    let components = match (cx.photometric, count) {
        (PhotometricInterpretation::YCbCr, 3) => {
            let h = u8::try_from(cx.ycbcr_subsampling.0.max(1)).unwrap_or(1);
            let v = u8::try_from(cx.ycbcr_subsampling.1.max(1)).unwrap_or(1);
            vec![
                component(1, h, v, false),
                component(2, 1, 1, true),
                component(3, 1, 1, true),
            ]
        }
        (PhotometricInterpretation::Rgb, 3) => vec![
            component(b'R', 1, 1, false),
            component(b'G', 1, 1, false),
            component(b'B', 1, 1, false),
        ],
        (PhotometricInterpretation::Separated, 4) => vec![
            component(b'C', 1, 1, false),
            component(b'M', 1, 1, false),
            component(b'Y', 1, 1, false),
            component(b'K', 1, 1, false),
        ],
        _ => (0..count)
            .map(|index| component((index + 1) as u8, 1, 1, false))
            .collect(),
    };
    Ok(encode::Layout {
        width,
        height,
        components,
    })
}

/// The `JPEGTables` (347) blob for this image, or `None` when the caller asked
/// for self-contained chunks.
///
/// The writer calls this once per page and hands the result back through
/// [`CodecContext::jpeg_tables`] for every chunk, so the tables in the tag are
/// by construction the tables the chunks were coded with.
///
/// # Errors
/// The same set as [`encode`].
pub fn shared_tables(cx: &CodecContext<'_>, level: CodecLevel) -> Result<Vec<u8>> {
    let layout = layout(cx)?;
    let tables = encode::table_set(quality(level), &layout);
    Ok(encode::tables_blob(&tables))
}

/// The quality one [`CodecLevel`] selects.
fn quality(level: CodecLevel) -> u8 {
    match level {
        CodecLevel::Level(value) => value.clamp(1, 100) as u8,
        _ => DEFAULT_QUALITY,
    }
}

/// Encodes one chunk as a TTN2 JPEG datastream.
///
/// When [`CodecContext::jpeg_tables`] is set the chunk is *abbreviated*: it
/// carries `SOI SOF0 SOS … EOI` and the quantisation and Huffman tables live
/// in tag 347, exactly as `tiffcp -c jpeg` writes them. Otherwise the tables
/// are written inline in every chunk.
///
/// # Errors
/// [`UnsupportedError::BitsPerSample`] for anything but 8-bit samples and
/// [`UnsupportedError::Conversion`] for more than four channels.
pub fn encode(src: &[u8], cx: &CodecContext<'_>, level: CodecLevel) -> Result<Vec<u8>> {
    let layout = layout(cx)?;
    let tables = encode::table_set(quality(level), &layout);
    let blob;
    let inline = if cx.jpeg_tables.is_some() {
        None
    } else {
        blob = encode::tables_blob(&tables);
        // The blob is a complete tables-only stream; the chunk needs its body.
        blob.get(2..blob.len().saturating_sub(2))
    };
    Ok(encode::encode_chunk(src, &layout, &tables, inline))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::byteorder::Endian;
    use crate::limits::Leniency;
    use crate::tags::CompressionMethod;

    fn context<'a>(
        photometric: PhotometricInterpretation,
        width: usize,
        height: usize,
        bits: &'a [u16],
        spp: u16,
    ) -> CodecContext<'a> {
        let mut cx = CodecContext::new(
            CompressionMethod::Jpeg,
            width,
            height,
            bits,
            spp,
            Endian::Little,
        );
        cx.photometric = photometric;
        cx
    }

    fn ramp(width: usize, height: usize, spp: usize) -> Vec<u8> {
        (0..width * height * spp)
            .map(|i| ((i * 7 / spp.max(1)) % 251) as u8)
            .collect()
    }

    fn assert_close(got: &[u8], want: &[u8], tolerance: u8, label: &str) {
        assert_eq!(got.len(), want.len(), "{label}");
        for (index, (a, b)) in got.iter().zip(want.iter()).enumerate() {
            assert!(
                a.abs_diff(*b) <= tolerance,
                "{label}: sample {index} is {a}, expected about {b}"
            );
        }
    }

    #[test]
    fn greyscale_round_trips() {
        let bits = [8u16];
        let cx = context(PhotometricInterpretation::BlackIsZero, 32, 16, &bits, 1);
        let pixels = ramp(32, 16, 1);
        let chunk = encode(&pixels, &cx, CodecLevel::Level(95)).expect("encode");
        let mut out = vec![0u8; pixels.len()];
        assert_eq!(
            decode_into(&chunk, &mut out, &cx).expect("decode"),
            pixels.len()
        );
        assert_close(&out, &pixels, 14, "gray");
    }

    #[test]
    fn rgb_round_trips_without_a_colour_transform() {
        let bits = [8u16, 8, 8];
        let cx = context(PhotometricInterpretation::Rgb, 24, 16, &bits, 3);
        let pixels = ramp(24, 16, 3);
        let chunk = encode(&pixels, &cx, CodecLevel::Level(95)).expect("encode");
        // The frame must declare libjpeg's RGB component identifiers.
        let facts = scan::scan(&chunk);
        assert_eq!(
            facts.components.iter().map(|c| c.id).collect::<Vec<_>>(),
            vec![b'R', b'G', b'B']
        );
        let mut out = vec![0u8; pixels.len()];
        assert_eq!(
            decode_into(&chunk, &mut out, &cx).expect("decode"),
            pixels.len()
        );
        assert_close(&out, &pixels, 16, "rgb");
    }

    #[test]
    fn subsampled_ycbcr_round_trips_through_the_frame_header() {
        let bits = [8u16, 8, 8];
        let mut cx = context(PhotometricInterpretation::YCbCr, 32, 16, &bits, 3);
        cx.ycbcr_subsampling = (2, 2);
        let pixels = ramp(32, 16, 3);
        let chunk = encode(&pixels, &cx, CodecLevel::Level(90)).expect("encode");
        let facts = scan::scan(&chunk);
        assert_eq!(facts.max_sampling(), (2, 2));
        assert_eq!(facts.components.len(), 3);
        let mut out = vec![0u8; pixels.len()];
        // The decode is full-resolution: chroma is upsampled by the JPEG
        // layer, which is what `expands_subsampling` promises the pipeline.
        assert_eq!(
            decode_into(&chunk, &mut out, &cx).expect("decode"),
            pixels.len()
        );
    }

    #[test]
    fn shared_tables_produce_an_abbreviated_chunk() {
        let bits = [8u16];
        let mut cx = context(PhotometricInterpretation::BlackIsZero, 16, 8, &bits, 1);
        let tables = shared_tables(&cx, CodecLevel::Level(80)).expect("tables");
        assert_eq!(tables.get(..2), Some(&[0xFF, 0xD8][..]));
        assert_eq!(tables.get(2..4), Some(&[0xFF, 0xDB][..]));
        cx.jpeg_tables = Some(&tables);
        let pixels = ramp(16, 8, 1);
        let chunk = encode(&pixels, &cx, CodecLevel::Level(80)).expect("encode");
        // Abbreviated: the frame header follows `SOI` directly.
        assert_eq!(chunk.get(2..4), Some(&[0xFF, 0xC0][..]));
        let mut out = vec![0u8; pixels.len()];
        assert_eq!(
            decode_into(&chunk, &mut out, &cx).expect("decode"),
            pixels.len()
        );
        assert_close(&out, &pixels, 20, "abbreviated");
        // Without the tables the same chunk cannot be decoded.
        let mut bare = context(PhotometricInterpretation::BlackIsZero, 16, 8, &bits, 1);
        bare.jpeg_tables = None;
        let mut out = vec![0u8; pixels.len()];
        assert!(decode_into(&chunk, &mut out, &bare).is_err());
    }

    #[test]
    fn a_chunk_without_a_frame_header_is_a_named_error() {
        let bits = [8u16];
        let cx = context(PhotometricInterpretation::BlackIsZero, 8, 8, &bits, 1);
        let mut out = [0u8; 64];
        let err = decode_into(&[0xFF, 0xD8, 0xFF, 0xD9], &mut out, &cx).expect_err("no SOF");
        assert!(err.to_string().contains("SOF"), "{err}");
    }

    #[test]
    fn a_hierarchical_frame_is_refused_by_name() {
        let bits = [8u16];
        let cx = context(PhotometricInterpretation::BlackIsZero, 8, 8, &bits, 1);
        let data = [
            0xFF, 0xD8, 0xFF, 0xC5, 0x00, 0x0B, 0x08, 0x00, 0x08, 0x00, 0x08, 0x01, 0x01, 0x11,
            0x00,
        ];
        let mut out = [0u8; 64];
        let err = decode_into(&data, &mut out, &cx).expect_err("hierarchical");
        assert!(err.to_string().contains("hierarchical"), "{err}");
    }

    #[test]
    fn a_subsampling_disagreement_is_strict_only() {
        let bits = [8u16, 8, 8];
        let pixels = ramp(16, 16, 3);
        let mut writer = context(PhotometricInterpretation::YCbCr, 16, 16, &bits, 3);
        writer.ycbcr_subsampling = (1, 1);
        let chunk = encode(&pixels, &writer, CodecLevel::Default).expect("encode");

        let mut lenient = context(PhotometricInterpretation::YCbCr, 16, 16, &bits, 3);
        lenient.ycbcr_subsampling = (2, 2);
        let mut out = vec![0u8; pixels.len()];
        assert!(decode_into(&chunk, &mut out, &lenient).is_ok());

        let mut strict = lenient.clone();
        strict.leniency = Leniency::Strict;
        let mut out = vec![0u8; pixels.len()];
        let err = decode_into(&chunk, &mut out, &strict).expect_err("mismatch");
        assert!(matches!(
            err,
            TiffError::Format(FormatError::JpegSubsamplingMismatch { .. })
        ));
    }

    #[test]
    fn garbage_never_panics() {
        let bits = [8u16];
        let cx = context(PhotometricInterpretation::BlackIsZero, 8, 8, &bits, 1);
        let pixels = ramp(8, 8, 1);
        let chunk = encode(&pixels, &cx, CodecLevel::Default).expect("encode");
        for cut in 0..chunk.len() {
            let mut out = [0u8; 64];
            let _ = decode_into(&chunk[..cut], &mut out, &cx);
        }
        for seed in 0..64u32 {
            let mut noise = chunk.clone();
            for (index, byte) in noise.iter_mut().enumerate() {
                if index as u32 % (seed + 3) == 0 {
                    *byte ^= 0x5A;
                }
            }
            let mut out = [0u8; 64];
            let _ = decode_into(&noise, &mut out, &cx);
        }
    }

    #[test]
    fn deep_samples_and_wide_frames_are_refused_on_write() {
        let bits = [16u16];
        let cx = context(PhotometricInterpretation::BlackIsZero, 8, 8, &bits, 1);
        assert!(matches!(
            encode(&[0u8; 128], &cx, CodecLevel::Default),
            Err(TiffError::Unsupported(UnsupportedError::BitsPerSample(_)))
        ));
        let bits = [8u16; 5];
        let cx = context(PhotometricInterpretation::Rgb, 8, 8, &bits, 5);
        assert!(matches!(
            encode(&[0u8; 320], &cx, CodecLevel::Default),
            Err(TiffError::Unsupported(UnsupportedError::Conversion(_)))
        ));
    }
}
