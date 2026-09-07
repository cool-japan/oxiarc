//! Compression 7 on the write side: [`oxiarc_jpeg::Encoder`], configured the
//! way libtiff configures libjpeg.
//!
//! # What TIFF adds to the JPEG encoder
//!
//! Nothing about the coding itself — the DCT, the quantisation, the Huffman
//! coding and the box downsampling all live in `oxiarc-jpeg`, which is
//! byte-identical to `cjpeg -dct int`. What this module contributes is the
//! *mapping* from a chunk's TIFF context to
//! [`EncodeOptions`](oxiarc_jpeg::EncodeOptions), and it is the mapping that
//! makes the result a **TTN2** stream rather than a JFIF file:
//!
//! | TIFF | JPEG |
//! |---|---|
//! | `PhotometricInterpretation` = `YCbCr`, 3 channels | `ColorSpace::Ycbcr`, samples passed through, ids 1/2/3 |
//! | `RGB`, 3 channels | `ColorSpace::Rgb`, **no** colour transform, ids `R`/`G`/`B` |
//! | `Separated`, 4 channels | `ColorSpace::Cmyk`, no transform, ids `C`/`M`/`Y`/`K` |
//! | anything else, 1 channel | `ColorSpace::Luma`, id 1 |
//! | anything else, 3 or 4 channels | the same templates with ids 1.. (libjpeg's `JCS_UNKNOWN` layout) |
//! | `YCbCrSubSampling` (530) | the luma sampling factors, chroma always 1x1 |
//! | quality | `jpeg_quality_scaling` with `force_baseline`, as libtiff's `jpeg_set_quality(.., TRUE)` |
//!
//! The colour space is always set **explicitly**, never left to
//! [`InputColor::default_jpeg_color_space`](oxiarc_jpeg::InputColor::default_jpeg_color_space):
//! that would turn an RGB TIFF strip into a YCbCr frame, and a TIFF strip
//! carries no `Adobe` marker to say so, so the file would decode with its
//! colours transformed twice. `JFIF` and `Adobe` are suppressed for the same
//! reason — TTN2 keeps colour in `PhotometricInterpretation`.
//!
//! # Tag 347 and the chunks
//!
//! [`tables_blob`] is
//! [`Encoder::write_tables_only`](oxiarc_jpeg::Encoder::write_tables_only) and
//! produces `SOI DQT… DHT… EOI`, byte for byte what libtiff writes into
//! `JPEGTables` at the same quality (`tests/tiff_oracle_codecs.rs` compares
//! against `tiffcp -c jpeg`). [`encode_chunk`] is
//! [`Encoder::encode_scan_only`](oxiarc_jpeg::Encoder::encode_scan_only) when
//! those tables are shared and [`Encoder::encode`](oxiarc_jpeg::Encoder::encode)
//! when every chunk carries its own; both suppress the metadata markers.
//!
//! # What it refuses
//!
//! * anything but 8-bit samples, which is what `tiffcp -c jpeg` refuses too;
//! * a chunk with two channels, or more than four. JPEG's own colour-space
//!   templates cover 1, 3 and 4 components; libjpeg reaches a two-component
//!   frame only through `JCS_UNKNOWN`, which `oxiarc-jpeg` does not encode.
//!   Such a chunk is named in the error rather than silently losing a
//!   channel.

use oxiarc_jpeg::{
    ColorSpace, ComponentIds, EncodeOptions, Encoder, InputColor, JpegError, RestartInterval,
    Subsampling, TablesMode,
};

use super::CodecContext;
use crate::error::{Result, TiffError, UnsupportedError};
use crate::tags::{CompressionMethod, PhotometricInterpretation};

/// How one chunk of this image is handed to the JPEG encoder.
///
/// Built once per chunk (and once per page for tag 347) so that the tables in
/// the tag and the frames in the chunks cannot disagree.
#[derive(Clone, Debug)]
pub(super) struct Plan {
    /// The encoder configuration.
    options: EncodeOptions,
    /// How the chunk's interleaved samples are to be read.
    color: InputColor,
    /// Coded chunk width.
    width: u16,
    /// Coded chunk height.
    height: u16,
}

/// Maps a chunk's context onto the JPEG encoder's options.
///
/// # Errors
/// [`UnsupportedError::BitsPerSample`] for anything but 8-bit samples, and
/// [`UnsupportedError::Conversion`] for a channel count JPEG's colour-space
/// templates do not cover.
pub(super) fn plan(cx: &CodecContext<'_>, quality: u8) -> Result<Plan> {
    let width = u16::try_from(cx.width).map_err(|_| TiffError::IntOverflow)?;
    let height = u16::try_from(cx.height).map_err(|_| TiffError::IntOverflow)?;
    if cx.bits_per_sample.iter().any(|bits| *bits != 8) {
        return Err(TiffError::Unsupported(UnsupportedError::BitsPerSample(
            cx.bits_per_sample.to_vec(),
        )));
    }
    let count = usize::from(cx.samples_per_pixel).max(1);
    let (color, space, ids) = match (cx.photometric, count) {
        (PhotometricInterpretation::YCbCr, 3) => {
            (InputColor::Ycbcr, ColorSpace::Ycbcr, ComponentIds::Auto)
        }
        (PhotometricInterpretation::Rgb, 3) => {
            (InputColor::Rgb, ColorSpace::Rgb, ComponentIds::Auto)
        }
        (PhotometricInterpretation::Separated, 4) => {
            (InputColor::Cmyk, ColorSpace::Cmyk, ComponentIds::Auto)
        }
        (_, 1) => (InputColor::Luma, ColorSpace::Luma, ComponentIds::Auto),
        // libjpeg's `JCS_UNKNOWN` layout: sequential identifiers, one
        // quantisation and Huffman slot, no subsampling and no colour
        // transform. The `Rgb` and `Cmyk` templates carry exactly that shape,
        // so only the identifiers have to be overridden.
        (_, 3) => (InputColor::Rgb, ColorSpace::Rgb, ComponentIds::Sequential),
        (_, 4) => (InputColor::Cmyk, ColorSpace::Cmyk, ComponentIds::Sequential),
        (_, other) => {
            return Err(TiffError::Unsupported(UnsupportedError::Conversion(
                match other {
                    2 => "a two-channel JPEG frame needs libjpeg's JCS_UNKNOWN layout",
                    _ => "JPEG frames carry at most four components",
                },
            )));
        }
    };
    let mut options = EncodeOptions::tiff_strip(quality.clamp(1, 100));
    options.jpeg_color_space = Some(space);
    options.component_ids = ids;
    // libtiff calls `jpeg_set_quality(.., TRUE)`: the quantisers stay in
    // `1..=255` and the frame stays baseline even at quality 1.
    options.force_baseline = true;
    options.subsampling = subsampling(cx, space);
    options.restart_interval = match cx.jpeg_restart_rows {
        0 => RestartInterval::None,
        rows => RestartInterval::McuRows(rows),
    };
    Ok(Plan {
        options,
        color,
        width,
        height,
    })
}

/// The sampling factors this chunk's frame carries.
///
/// Only a `YCbCr` frame is ever subsampled — an RGB or CMYK frame has no
/// chroma to decimate, and libtiff never subsamples one either. The factors
/// are named per component rather than through
/// [`Subsampling::S420`](oxiarc_jpeg::Subsampling::S420) and friends, because
/// `YCbCrSubSampling` (530) may legally hold combinations those names do not
/// cover (4x2, for one).
fn subsampling(cx: &CodecContext<'_>, space: ColorSpace) -> Subsampling {
    if space != ColorSpace::Ycbcr {
        return Subsampling::S444;
    }
    let h = u8::try_from(cx.ycbcr_subsampling.0).unwrap_or(1).max(1);
    let v = u8::try_from(cx.ycbcr_subsampling.1).unwrap_or(1).max(1);
    Subsampling::Custom([(h, v), (1, 1), (1, 1), (1, 1)])
}

/// The `JPEGTables` (347) blob for this plan: `SOI`, the tables, `EOI`.
///
/// # Errors
/// [`crate::FormatError::Codec`] if the table set cannot be built — which the
/// TIFF options never provoke, since they select neither optimized Huffman
/// tables nor a process that generates its own.
pub(super) fn tables_blob(plan: &Plan) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut encoder = Encoder::with_options(&mut out, plan.options.clone());
    encoder
        .write_tables_only(TablesMode::BOTH, plan.color)
        .map_err(jpeg_error)?;
    Ok(out)
}

/// Encodes one chunk.
///
/// `abbreviated` selects the TTN2 shape whose tables live in tag 347; the
/// alternative writes them into every chunk, which is what libtiff does with
/// `JPEGTablesMode = 0`.
///
/// # Errors
/// [`crate::FormatError::Codec`] if the encoder rejects the samples — most
/// plausibly [`JpegError::BufferTooSmall`] for a chunk buffer that does not
/// hold `width * height * channels` samples.
pub(super) fn encode_chunk(src: &[u8], plan: &Plan, abbreviated: bool) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut encoder = Encoder::with_options(&mut out, plan.options.clone());
    if abbreviated {
        encoder
            .encode_scan_only(src, plan.width, plan.height, plan.color)
            .map_err(jpeg_error)?;
    } else {
        encoder
            .encode(src, plan.width, plan.height, plan.color)
            .map_err(jpeg_error)?;
    }
    Ok(out)
}

/// Reports a JPEG encoder failure as this crate's codec error.
fn jpeg_error(error: JpegError) -> TiffError {
    super::super::codec_error(CompressionMethod::Jpeg, error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::byteorder::Endian;

    /// The component identifiers of the frame header, found by walking the
    /// marker segments rather than by searching for `FF C0` (which can occur
    /// inside a `DQT` payload).
    fn frame_component_ids(stream: &[u8]) -> Vec<u8> {
        let mut pos = 2usize; // past SOI
        while pos + 3 < stream.len() {
            assert_eq!(stream[pos], 0xFF, "marker expected at {pos}");
            let marker = stream[pos + 1];
            let length = usize::from(u16::from_be_bytes([stream[pos + 2], stream[pos + 3]]));
            if marker == 0xC0 || marker == 0xC1 {
                let count = usize::from(stream[pos + 9]);
                return (0..count).map(|i| stream[pos + 10 + i * 3]).collect();
            }
            pos += 2 + length;
        }
        panic!("no frame header in the stream");
    }

    fn context<'a>(
        photometric: PhotometricInterpretation,
        bits: &'a [u16],
        spp: u16,
    ) -> CodecContext<'a> {
        let mut cx = CodecContext::new(CompressionMethod::Jpeg, 16, 16, bits, spp, Endian::Little);
        cx.photometric = photometric;
        cx
    }

    #[test]
    fn an_rgb_chunk_is_never_colour_transformed() {
        let bits = [8u16; 3];
        let cx = context(PhotometricInterpretation::Rgb, &bits, 3);
        let chunk_plan = plan(&cx, 75).expect("plan");
        assert_eq!(chunk_plan.options.jpeg_color_space, Some(ColorSpace::Rgb));
        assert_eq!(chunk_plan.color, InputColor::Rgb);
        // The frame header must carry libjpeg's RGB identifiers, which are
        // the only colour signal a TTN2 strip has.
        let chunk = encode_chunk(&vec![7u8; 16 * 16 * 3], &chunk_plan, false).expect("encode");
        assert_eq!(frame_component_ids(&chunk), b"RGB".to_vec());
    }

    #[test]
    fn a_separated_chunk_keeps_cmyk_identifiers() {
        let bits = [8u16; 4];
        let cx = context(PhotometricInterpretation::Separated, &bits, 4);
        let chunk_plan = plan(&cx, 75).expect("plan");
        assert_eq!(chunk_plan.options.jpeg_color_space, Some(ColorSpace::Cmyk));
        let chunk = encode_chunk(&vec![9u8; 16 * 16 * 4], &chunk_plan, false).expect("encode");
        assert_eq!(frame_component_ids(&chunk), b"CMYK".to_vec());
    }

    #[test]
    fn an_unknown_three_channel_photometric_gets_sequential_identifiers() {
        let bits = [8u16; 3];
        let cx = context(PhotometricInterpretation::CieLab, &bits, 3);
        let chunk_plan = plan(&cx, 75).expect("plan");
        assert_eq!(chunk_plan.options.component_ids, ComponentIds::Sequential);
        let chunk = encode_chunk(&vec![3u8; 16 * 16 * 3], &chunk_plan, false).expect("encode");
        assert_eq!(frame_component_ids(&chunk), vec![1, 2, 3]);
    }

    #[test]
    fn subsampling_follows_tag_530_only_for_ycbcr() {
        let bits = [8u16; 3];
        let mut cx = context(PhotometricInterpretation::YCbCr, &bits, 3);
        cx.ycbcr_subsampling = (2, 1);
        let chunk_plan = plan(&cx, 75).expect("plan");
        assert_eq!(
            chunk_plan.options.subsampling,
            Subsampling::Custom([(2, 1), (1, 1), (1, 1), (1, 1)])
        );
        // The same tag on an RGB image means nothing: there is no chroma.
        let mut rgb = context(PhotometricInterpretation::Rgb, &bits, 3);
        rgb.ycbcr_subsampling = (2, 2);
        assert_eq!(
            plan(&rgb, 75).expect("plan").options.subsampling,
            Subsampling::S444
        );
    }

    #[test]
    fn a_restart_row_count_becomes_a_dri_segment() {
        let bits = [8u16];
        let mut cx = context(PhotometricInterpretation::BlackIsZero, &bits, 1);
        cx.jpeg_restart_rows = 1;
        let chunk_plan = plan(&cx, 75).expect("plan");
        assert_eq!(
            chunk_plan.options.restart_interval,
            RestartInterval::McuRows(1)
        );
        let chunk = encode_chunk(&vec![5u8; 16 * 16], &chunk_plan, false).expect("encode");
        assert!(
            chunk.windows(2).any(|w| w == [0xFF, 0xDD]),
            "a restart interval must write DRI"
        );
    }

    #[test]
    fn deep_samples_and_two_channel_chunks_are_named() {
        let deep = [16u16];
        let cx = context(PhotometricInterpretation::BlackIsZero, &deep, 1);
        assert!(matches!(
            plan(&cx, 75),
            Err(TiffError::Unsupported(UnsupportedError::BitsPerSample(_)))
        ));

        let bits = [8u16; 2];
        let cx = context(PhotometricInterpretation::BlackIsZero, &bits, 2);
        let error = plan(&cx, 75).expect_err("two channels");
        assert!(
            error.to_string().contains("JCS_UNKNOWN"),
            "the refusal must name the reason: {error}"
        );
    }

    #[test]
    fn the_quality_is_clamped_rather_than_rejected() {
        let bits = [8u16];
        let cx = context(PhotometricInterpretation::BlackIsZero, &bits, 1);
        assert_eq!(plan(&cx, 0).expect("low").options.quality, 1);
        assert_eq!(plan(&cx, 200).expect("high").options.quality, 100);
        assert!(plan(&cx, 50).expect("mid").options.force_baseline);
    }
}
