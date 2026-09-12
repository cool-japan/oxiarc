//! The geometry and colour model of one TIFF image (one IFD).
//!
//! [`ImageInfo`] is the single place where the tag soup becomes numbers the
//! decoder can trust: it applies every spec default, cross-checks the counts,
//! and computes strip/tile geometry with the padding rules TIFF 6.0 defines
//! (strips are clipped at the bottom, tiles are always coded full size).
//!
//! ```
//! use oxiarc_tiff::image::{ChunkType, ColorType};
//!
//! assert_eq!(ColorType::Rgb(8).samples_per_pixel(), 3);
//! assert_eq!(ColorType::Rgba(16).bit_depth(), 16);
//! assert_eq!(ChunkType::Strip.to_string(), "strip");
//! ```

use crate::byteorder::Endian;
use crate::error::{FormatError, LimitError, Result, TiffError, UnsupportedError};
use crate::ifd::{Directory, IfdPointer, Rational, Value, ValueSource};
use crate::limits::{Leniency, Limits, Warning, Warnings};
use std::sync::Arc;

use crate::compression::{CodecState, OldJpegParams};
use crate::sample::{SampleType, packed_row_bytes};
use crate::tags::{
    CompressionMethod, ExtraSamples, FillOrder, Orientation, PhotometricInterpretation,
    PlanarConfiguration, Predictor, ResolutionUnit, SampleFormat, T4Options, T6Options, Tag,
    YCbCrPositioning,
};

mod types;

pub use types::{ChunkGeometry, ChunkType, ColorType, ImageLayout, Rect};

/// Everything the decoder needs to know about one image.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct ImageInfo {
    /// The byte order every multi-byte field, including the pixel data, is
    /// stored in. Carried here so the decode pipeline does not have to thread
    /// the file header through every call.
    pub endian: Endian,
    /// `ImageWidth` (256).
    pub width: u32,
    /// `ImageLength` (257).
    pub height: u32,
    /// `SamplesPerPixel` (277).
    pub samples_per_pixel: u16,
    /// `BitsPerSample` (258), one entry per channel.
    pub bits_per_sample: Vec<u16>,
    /// `SampleFormat` (339), one entry per channel.
    pub sample_format: Vec<SampleFormat>,
    /// `PhotometricInterpretation` (262).
    pub photometric: PhotometricInterpretation,
    /// `PlanarConfiguration` (284).
    pub planar: PlanarConfiguration,
    /// `Compression` (259).
    pub compression: CompressionMethod,
    /// `Predictor` (317).
    pub predictor: Predictor,
    /// `FillOrder` (266).
    pub fill_order: FillOrder,
    /// `Orientation` (274); exposed, never applied.
    pub orientation: Orientation,
    /// Strip or tile layout.
    pub chunks: ChunkGeometry,
    /// `ColorMap` (320), `3 * 2^bits` 16-bit entries.
    pub color_map: Option<Vec<u16>>,
    /// `ExtraSamples` (338).
    pub extra_samples: Vec<ExtraSamples>,
    /// `YCbCrSubSampling` (530); defaults to `(2, 2)`.
    pub ycbcr_subsampling: (u16, u16),
    /// `YCbCrCoefficients` (529); defaults to the TIFF 6.0 values.
    pub ycbcr_coefficients: [f64; 3],
    /// `YCbCrPositioning` (531).
    pub ycbcr_positioning: YCbCrPositioning,
    /// `ReferenceBlackWhite` (532), six rationals.
    pub reference_black_white: Option<[Rational; 6]>,
    /// `T4Options` (292).
    pub t4_options: T4Options,
    /// `T6Options` (293).
    pub t6_options: T6Options,
    /// `JPEGTables` (347).
    pub jpeg_tables: Option<Vec<u8>>,
    /// The old-style JPEG (compression 6) parameter tags, with every table the
    /// tags point at already loaded. `None` for every other compression.
    pub old_jpeg: Option<OldJpegParams>,
    /// `XResolution`, `YResolution` and `ResolutionUnit`.
    pub resolution: (Option<Rational>, Option<Rational>, ResolutionUnit),
    /// `SubIFDs` (330).
    pub sub_ifds: Vec<IfdPointer>,
    /// `ExifIFD` (34665).
    pub exif_ifd: Option<IfdPointer>,
    /// `GPSIFD` (34853).
    pub gps_ifd: Option<IfdPointer>,
    /// `InteroperabilityIFD` (40965).
    pub interop_ifd: Option<IfdPointer>,
    /// Decisions and scratch shared by every chunk of this image.
    ///
    /// Cloning an `ImageInfo` shares the state rather than copying it, so a
    /// parallel decode of the same image reuses one LZW rule and one inflate
    /// window.
    pub codec_state: Arc<CodecState>,
}

/// Reads a scalar tag, falling back to the spec default.
fn scalar_u64<S: ValueSource>(
    dir: &Directory,
    tag: Tag,
    source: &mut S,
    default: Option<u64>,
) -> Result<Option<u64>> {
    match dir.get(tag) {
        Some(entry) => {
            let value = source.load(entry)?;
            Ok(value.first_u64().or(default))
        }
        None => Ok(default),
    }
}

/// Reads a tag as a vector of `u64`s.
fn vector_u64<S: ValueSource>(
    dir: &Directory,
    tag: Tag,
    source: &mut S,
) -> Result<Option<Vec<u64>>> {
    match dir.get(tag) {
        Some(entry) => Ok(source.load(entry)?.as_u64_vec()),
        None => Ok(None),
    }
}

/// Loads the TIFF 6.0 §22 old-style JPEG tags (512-521).
///
/// Tags 519/520/521 are arrays of **file offsets**, one per component, not the
/// tables themselves — the single most awkward thing about compression 6, and
/// the reason [`ValueSource::read_raw`] exists. An offset that does not resolve
/// produces a warning and an empty slot rather than an error: the codec decides
/// whether the flavour it is looking at can live without that table.
fn read_old_jpeg_params<S: ValueSource>(
    dir: &Directory,
    source: &mut S,
    warnings: &mut Warnings,
) -> Result<OldJpegParams> {
    let proc = scalar_u64(dir, Tag::JpegProc, source, Some(1))?.unwrap_or(1) as u16;
    let restart_interval =
        scalar_u64(dir, Tag::JpegRestartInterval, source, Some(0))?.unwrap_or(0) as u16;
    let interchange_offset = scalar_u64(dir, Tag::JpegInterchangeFormat, source, None)?;
    let interchange_len = scalar_u64(dir, Tag::JpegInterchangeFormatLength, source, None)?;
    let interchange = match (interchange_offset, interchange_len) {
        (Some(offset), Some(len)) if offset > 0 && len > 0 => source.read_raw(offset, len)?,
        _ => None,
    };
    let q_tables =
        load_old_jpeg_tables(dir, Tag::JpegQTables, source, warnings, TableShape::Quant)?;
    let dc_tables = load_old_jpeg_tables(
        dir,
        Tag::JpegDcTables,
        source,
        warnings,
        TableShape::Huffman,
    )?;
    let ac_tables = load_old_jpeg_tables(
        dir,
        Tag::JpegAcTables,
        source,
        warnings,
        TableShape::Huffman,
    )?;
    let shorts = |tag: Tag, source: &mut S| -> Result<Vec<u16>> {
        Ok(vector_u64(dir, tag, source)?
            .unwrap_or_default()
            .into_iter()
            .map(|v| v as u16)
            .collect())
    };
    let lossless_predictors = shorts(Tag::JpegLosslessPredictors, source)?;
    let point_transform = shorts(Tag::JpegPointTransforms, source)?;
    Ok(OldJpegParams {
        proc,
        interchange,
        restart_interval,
        q_tables,
        dc_tables,
        ac_tables,
        lossless_predictors,
        point_transform,
    })
}

/// What an old-style JPEG table offset points at.
#[derive(Clone, Copy, PartialEq, Eq)]
enum TableShape {
    /// 64 bytes of 8-bit quantisation values, in zig-zag order.
    Quant,
    /// 16 `BITS` counts followed by `sum(BITS)` `HUFFVAL` bytes.
    Huffman,
}

/// Resolves one of the three table-offset tags into loaded table bytes.
fn load_old_jpeg_tables<S: ValueSource>(
    dir: &Directory,
    tag: Tag,
    source: &mut S,
    warnings: &mut Warnings,
    shape: TableShape,
) -> Result<Vec<Vec<u8>>> {
    let Some(offsets) = vector_u64(dir, tag, source)? else {
        return Ok(Vec::new());
    };
    let mut tables = Vec::with_capacity(offsets.len().min(8));
    for offset in offsets.into_iter().take(8) {
        let bytes = match shape {
            TableShape::Quant => source.read_raw(offset, 64)?,
            TableShape::Huffman => match source.read_raw(offset, 16)? {
                Some(bits) => {
                    let total: u64 = bits.iter().map(|count| u64::from(*count)).sum();
                    source.read_raw(offset, 16 + total)?
                }
                None => None,
            },
        };
        match bytes {
            Some(bytes) => tables.push(bytes),
            None => {
                warnings.push(Warning::SpecViolation {
                    message: format!(
                        "old-style JPEG table offset {offset} (tag {tag}) is unusable"
                    ),
                });
                tables.push(Vec::new());
            }
        }
    }
    Ok(tables)
}

impl ImageInfo {
    /// Builds the model from a parsed directory.
    ///
    /// Every guard in [`Limits`] is applied before anything is sized from a
    /// number in the file, and every spec default is applied here rather than
    /// scattered through the decoder.
    ///
    /// # Errors
    /// [`FormatError`] for structural problems, [`UnsupportedError`] for
    /// features this build cannot decode, and [`LimitError`] when a guard
    /// fires.
    #[allow(clippy::too_many_arguments)]
    pub fn from_directory<S: ValueSource>(
        dir: &Directory,
        source: &mut S,
        limits: &Limits,
        leniency: Leniency,
        warnings: &mut Warnings,
        file_len: u64,
        endian: Endian,
    ) -> Result<Self> {
        let width_raw = scalar_u64(dir, Tag::ImageWidth, source, None)?.ok_or(
            TiffError::Format(FormatError::RequiredTagNotFound(Tag::ImageWidth)),
        )?;
        let height_raw = scalar_u64(dir, Tag::ImageLength, source, None)?.ok_or(
            TiffError::Format(FormatError::RequiredTagNotFound(Tag::ImageLength)),
        )?;
        let width = limits.check_dimension(width_raw)?;
        let height = limits.check_dimension(height_raw)?;
        if width == 0 || height == 0 {
            return Err(TiffError::Format(FormatError::ZeroDimension {
                width,
                height,
            }));
        }

        let samples_per_pixel =
            u16::try_from(scalar_u64(dir, Tag::SamplesPerPixel, source, Some(1))?.unwrap_or(1))
                .map_err(|_| TiffError::IntOverflow)?;
        if samples_per_pixel == 0 {
            return Err(TiffError::Format(FormatError::InconsistentBitsPerSample {
                expected: 0,
                got: 0,
            }));
        }

        let mut bits_per_sample = vector_u64(dir, Tag::BitsPerSample, source)?
            .map(|v| v.into_iter().map(|b| b as u16).collect::<Vec<u16>>())
            .unwrap_or_else(|| vec![1; samples_per_pixel as usize]);
        if bits_per_sample.len() != samples_per_pixel as usize {
            if bits_per_sample.len() == 1 && !leniency.is_strict() {
                let bits = bits_per_sample.first().copied().unwrap_or(1);
                bits_per_sample = vec![bits; samples_per_pixel as usize];
                warnings.push(Warning::SpecViolation {
                    message: format!(
                        "BitsPerSample had one entry for {samples_per_pixel} samples; broadcast"
                    ),
                });
            } else {
                return Err(TiffError::Format(FormatError::InconsistentBitsPerSample {
                    expected: samples_per_pixel,
                    got: bits_per_sample.len(),
                }));
            }
        }
        for bits in &bits_per_sample {
            if *bits == 0 || *bits > 64 {
                return Err(TiffError::Unsupported(UnsupportedError::BitsPerSample(
                    bits_per_sample.clone(),
                )));
            }
        }

        let sample_format = match vector_u64(dir, Tag::SampleFormat, source)? {
            Some(values) => {
                let mut formats: Vec<SampleFormat> = values
                    .into_iter()
                    .map(|v| SampleFormat::from_u16(v as u16))
                    .collect();
                if formats.len() == 1 {
                    let first = formats.first().copied().unwrap_or(SampleFormat::Uint);
                    formats = vec![first; samples_per_pixel as usize];
                }
                formats.resize(samples_per_pixel as usize, SampleFormat::Uint);
                for format in &formats {
                    if matches!(format, SampleFormat::Void) || format.is_unknown() {
                        warnings.push(Warning::SpecViolation {
                            message: format!("SampleFormat {format} treated as unsigned integer"),
                        });
                    }
                }
                formats
            }
            None => vec![SampleFormat::Uint; samples_per_pixel as usize],
        };

        let compression = CompressionMethod::from_u16(
            scalar_u64(dir, Tag::Compression, source, Some(1))?.unwrap_or(1) as u16,
        );
        let predictor = Predictor::from_u16(
            scalar_u64(dir, Tag::Predictor, source, Some(1))?.unwrap_or(1) as u16,
        );
        let fill_order = FillOrder::from_u16(
            scalar_u64(dir, Tag::FillOrder, source, Some(1))?.unwrap_or(1) as u16,
        );
        let orientation = Orientation::from_u16(
            scalar_u64(dir, Tag::Orientation, source, Some(1))?.unwrap_or(1) as u16,
        );
        let planar = PlanarConfiguration::from_u16(
            scalar_u64(dir, Tag::PlanarConfiguration, source, Some(1))?.unwrap_or(1) as u16,
        );

        let color_map = match dir.get(Tag::ColorMap) {
            Some(entry) => source
                .load(entry)?
                .as_u64_vec()
                .map(|v| v.into_iter().map(|x| x as u16).collect::<Vec<u16>>()),
            None => None,
        };

        let photometric = match scalar_u64(dir, Tag::PhotometricInterpretation, source, None)? {
            Some(value) => PhotometricInterpretation::from_u16(value as u16),
            None => {
                if leniency.is_strict() {
                    return Err(TiffError::Format(FormatError::RequiredTagNotFound(
                        Tag::PhotometricInterpretation,
                    )));
                }
                let inferred = if color_map.is_some() {
                    PhotometricInterpretation::Palette
                } else if samples_per_pixel >= 3 {
                    PhotometricInterpretation::Rgb
                } else {
                    PhotometricInterpretation::BlackIsZero
                };
                warnings.push(Warning::AssumedDefault {
                    tag: Tag::PhotometricInterpretation.to_u16(),
                    reason: "PhotometricInterpretation absent; inferred from the other tags",
                });
                inferred
            }
        };

        let extra_samples = vector_u64(dir, Tag::ExtraSamples, source)?
            .map(|v| {
                v.into_iter()
                    .map(|x| ExtraSamples::from_u16(x as u16))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        let ycbcr_subsampling = match vector_u64(dir, Tag::YCbCrSubSampling, source)? {
            Some(values) => {
                let h = values.first().copied().unwrap_or(2) as u16;
                let v = values.get(1).copied().unwrap_or(2) as u16;
                if !matches!(h, 1 | 2 | 4) || !matches!(v, 1 | 2 | 4) {
                    return Err(TiffError::Format(FormatError::IllegalSubsampling(h, v)));
                }
                (h, v)
            }
            None => (2, 2),
        };
        let ycbcr_coefficients = match dir.get(Tag::YCbCrCoefficients) {
            Some(entry) => {
                let values = source.load(entry)?.as_f64_vec().unwrap_or_default();
                [
                    values.first().copied().unwrap_or(0.299),
                    values.get(1).copied().unwrap_or(0.587),
                    values.get(2).copied().unwrap_or(0.114),
                ]
            }
            None => [0.299, 0.587, 0.114],
        };
        let ycbcr_positioning = YCbCrPositioning::from_u16(
            scalar_u64(dir, Tag::YCbCrPositioning, source, Some(1))?.unwrap_or(1) as u16,
        );
        let reference_black_white = match dir.get(Tag::ReferenceBlackWhite) {
            Some(entry) => {
                let value = source.load(entry)?;
                match value.as_rational_vec() {
                    Some(pairs) if pairs.len() >= 6 => {
                        let mut out = [Rational::default(); 6];
                        for (slot, pair) in out.iter_mut().zip(pairs.iter()) {
                            *slot = *pair;
                        }
                        Some(out)
                    }
                    _ => match value.as_f64_vec() {
                        Some(values) if values.len() >= 6 => {
                            let mut out = [Rational::default(); 6];
                            for (slot, v) in out.iter_mut().zip(values.iter()) {
                                *slot = Rational {
                                    num: *v as u32,
                                    den: 1,
                                };
                            }
                            Some(out)
                        }
                        _ => None,
                    },
                }
            }
            None => None,
        };

        let t4_options = T4Options::from_u32(
            scalar_u64(dir, Tag::T4Options, source, Some(0))?.unwrap_or(0) as u32,
        );
        let t6_options = T6Options::from_u32(
            scalar_u64(dir, Tag::T6Options, source, Some(0))?.unwrap_or(0) as u32,
        );
        if (t4_options.to_u32() != 0 || t6_options.to_u32() != 0)
            && !matches!(
                compression,
                CompressionMethod::CcittRle
                    | CompressionMethod::CcittFax3
                    | CompressionMethod::CcittFax4
                    | CompressionMethod::CcittRleWord
            )
        {
            warnings.push(Warning::IgnoredTag {
                tag: Tag::T4Options.to_u16(),
                reason: "T4/T6Options are only meaningful with a CCITT compression",
            });
        }

        let jpeg_tables = match dir.get(Tag::JpegTables) {
            Some(entry) => {
                if !matches!(
                    compression,
                    CompressionMethod::Jpeg | CompressionMethod::OldJpeg
                ) {
                    warnings.push(Warning::IgnoredTag {
                        tag: Tag::JpegTables.to_u16(),
                        reason: "JPEGTables is only meaningful with a JPEG compression",
                    });
                }
                source.load(entry)?.as_bytes().map(<[u8]>::to_vec)
            }
            None => None,
        };

        let old_jpeg = if compression == CompressionMethod::OldJpeg {
            Some(read_old_jpeg_params(dir, source, warnings)?)
        } else {
            for tag in [Tag::JpegProc, Tag::JpegQTables, Tag::JpegAcTables] {
                if dir.contains(tag) {
                    warnings.push(Warning::IgnoredTag {
                        tag: tag.to_u16(),
                        reason: "the 512-521 tags are only meaningful with compression 6",
                    });
                }
            }
            None
        };

        let resolution = {
            let x = match dir.get(Tag::XResolution) {
                Some(entry) => source
                    .load(entry)?
                    .as_rational_vec()
                    .and_then(|v| v.first().copied()),
                None => None,
            };
            let y = match dir.get(Tag::YResolution) {
                Some(entry) => source
                    .load(entry)?
                    .as_rational_vec()
                    .and_then(|v| v.first().copied()),
                None => None,
            };
            let unit = ResolutionUnit::from_u16(
                scalar_u64(dir, Tag::ResolutionUnit, source, Some(2))?.unwrap_or(2) as u16,
            );
            (x, y, unit)
        };

        let sub_ifds = match dir.get(Tag::SubIfds) {
            Some(entry) => crate::ifd::read_ifd_pointers(entry, source, leniency)?,
            None => Vec::new(),
        };
        let single_pointer = |tag: Tag, source: &mut S| -> Result<Option<IfdPointer>> {
            match dir.get(tag) {
                Some(entry) => Ok(crate::ifd::read_ifd_pointers(entry, source, leniency)?
                    .first()
                    .copied()),
                None => Ok(None),
            }
        };
        let exif_ifd = single_pointer(Tag::ExifIfd, source)?;
        let gps_ifd = single_pointer(Tag::GpsIfd, source)?;
        let interop_ifd = single_pointer(Tag::InteroperabilityIfd, source)?;

        let chunks = Self::read_geometry(
            dir,
            source,
            limits,
            leniency,
            warnings,
            width,
            height,
            samples_per_pixel,
            &bits_per_sample,
            planar,
            photometric,
            ycbcr_subsampling,
            file_len,
        )?;

        let info = Self {
            endian,
            width,
            height,
            samples_per_pixel,
            bits_per_sample,
            sample_format,
            photometric,
            planar,
            compression,
            predictor,
            fill_order,
            orientation,
            chunks,
            color_map,
            extra_samples,
            ycbcr_subsampling,
            ycbcr_coefficients,
            ycbcr_positioning,
            reference_black_white,
            t4_options,
            t6_options,
            jpeg_tables,
            old_jpeg,
            resolution,
            sub_ifds,
            exif_ifd,
            gps_ifd,
            interop_ifd,
            codec_state: Arc::new(CodecState::new()),
        };
        info.validate(limits, leniency, warnings)?;
        Ok(info)
    }

    #[allow(clippy::too_many_arguments)]
    fn read_geometry<S: ValueSource>(
        dir: &Directory,
        source: &mut S,
        limits: &Limits,
        leniency: Leniency,
        warnings: &mut Warnings,
        width: u32,
        height: u32,
        samples_per_pixel: u16,
        bits_per_sample: &[u16],
        planar: PlanarConfiguration,
        photometric: PhotometricInterpretation,
        subsampling: (u16, u16),
        file_len: u64,
    ) -> Result<ChunkGeometry> {
        let tiled = dir.contains(Tag::TileOffsets) || dir.contains(Tag::TileWidth);
        let planes = if planar == PlanarConfiguration::Planar {
            u64::from(samples_per_pixel)
        } else {
            1
        };

        if tiled {
            let tile_width = scalar_u64(dir, Tag::TileWidth, source, None)?.ok_or(
                TiffError::Format(FormatError::RequiredTagNotFound(Tag::TileWidth)),
            )? as u32;
            let tile_length = scalar_u64(dir, Tag::TileLength, source, None)?.ok_or(
                TiffError::Format(FormatError::RequiredTagNotFound(Tag::TileLength)),
            )? as u32;
            if tile_width == 0 || tile_length == 0 {
                return Err(TiffError::Format(FormatError::ZeroTileDimension {
                    width: tile_width,
                    length: tile_length,
                }));
            }
            if tile_width % 16 != 0 || tile_length % 16 != 0 {
                warnings.push(Warning::SpecViolation {
                    message: format!(
                        "tile size {tile_width}x{tile_length} is not a multiple of 16"
                    ),
                });
            }
            let across = u64::from(width.div_ceil(tile_width));
            let down = u64::from(height.div_ceil(tile_length));
            let expected = across
                .checked_mul(down)
                .and_then(|v| v.checked_mul(planes))
                .ok_or(TiffError::IntOverflow)?;
            limits.check_chunk_count(expected)?;
            // Only derive the per-chunk sizes when `TileByteCounts` is really
            // missing: the derivation is O(chunk count) in both time and
            // memory, and `max_chunks` alone would let a 200-byte file drive
            // tens of megabytes of `Vec<u64>` on every uncompressed image.
            let counts = vector_u64(dir, Tag::TileByteCounts, source)?;
            let fallback = if counts.is_some() {
                None
            } else {
                Self::uncompressed_chunk_sizes(
                    dir,
                    source,
                    limits,
                    expected,
                    |_| (tile_width, tile_length),
                    samples_per_pixel,
                    bits_per_sample,
                    planar,
                    photometric,
                    subsampling,
                    across.saturating_mul(down).max(1),
                )?
            };
            let (offsets, byte_counts) = Self::read_chunk_tables(
                dir,
                source,
                leniency,
                warnings,
                Tag::TileOffsets,
                Tag::TileByteCounts,
                expected,
                counts,
                fallback,
                file_len,
            )?;
            Ok(ChunkGeometry::Tiles {
                tile_width,
                tile_length,
                offsets,
                byte_counts,
            })
        } else {
            let rows_per_strip =
                scalar_u64(dir, Tag::RowsPerStrip, source, Some(u64::from(u32::MAX)))?
                    .unwrap_or(u64::from(u32::MAX));
            let rows_per_strip = if rows_per_strip == 0 {
                if leniency.is_strict() {
                    return Err(TiffError::Format(FormatError::InconsistentSizes {
                        tag: Tag::RowsPerStrip,
                        expected: 1,
                        got: 0,
                    }));
                }
                warnings.push(Warning::AssumedDefault {
                    tag: Tag::RowsPerStrip.to_u16(),
                    reason: "RowsPerStrip 0 treated as the whole image",
                });
                u32::MAX
            } else {
                u32::try_from(rows_per_strip.min(u64::from(u32::MAX)))
                    .map_err(|_| TiffError::IntOverflow)?
            };
            let per_plane = u64::from(height.div_ceil(rows_per_strip));
            let expected = per_plane
                .checked_mul(planes)
                .ok_or(TiffError::IntOverflow)?;
            limits.check_chunk_count(expected)?;
            // See the tiled branch: derived only when the tag is absent.
            let counts = vector_u64(dir, Tag::StripByteCounts, source)?;
            let fallback = if counts.is_some() {
                None
            } else {
                Self::uncompressed_chunk_sizes(
                    dir,
                    source,
                    limits,
                    expected,
                    |within| {
                        let row = (within as u32).saturating_mul(rows_per_strip);
                        (width, rows_per_strip.min(height.saturating_sub(row)))
                    },
                    samples_per_pixel,
                    bits_per_sample,
                    planar,
                    photometric,
                    subsampling,
                    per_plane.max(1),
                )?
            };
            let (offsets, byte_counts) = Self::read_chunk_tables(
                dir,
                source,
                leniency,
                warnings,
                Tag::StripOffsets,
                Tag::StripByteCounts,
                expected,
                counts,
                fallback,
                file_len,
            )?;
            Ok(ChunkGeometry::Strips {
                rows_per_strip,
                offsets,
                byte_counts,
            })
        }
    }

    /// Per-chunk byte counts for uncompressed data, used to recover a missing
    /// `StripByteCounts` / `TileByteCounts` (edge case E1).
    ///
    /// Returns `None` for compressed data, where the sizes cannot be derived.
    /// Only called when the byte-count tag is really absent: the table is
    /// `8 * chunk count` bytes derived entirely from numbers in the file, so it
    /// is charged against [`Limits::intermediate_buffer_size`] as well as
    /// [`Limits::max_chunks`].
    #[allow(clippy::too_many_arguments)]
    fn uncompressed_chunk_sizes<S: ValueSource, F: Fn(u64) -> (u32, u32)>(
        dir: &Directory,
        source: &mut S,
        limits: &Limits,
        expected: u64,
        coded_dimensions: F,
        samples_per_pixel: u16,
        bits_per_sample: &[u16],
        planar: PlanarConfiguration,
        photometric: PhotometricInterpretation,
        subsampling: (u16, u16),
        chunks_per_plane: u64,
    ) -> Result<Option<Vec<u64>>> {
        let compression = CompressionMethod::from_u16(
            scalar_u64(dir, Tag::Compression, source, Some(1))?.unwrap_or(1) as u16,
        );
        if compression != CompressionMethod::None {
            return Ok(None);
        }
        if photometric == PhotometricInterpretation::YCbCr
            && planar == PlanarConfiguration::Chunky
            && subsampling != (1, 1)
        {
            return Ok(None);
        }
        let count = usize::try_from(expected).map_err(|_| TiffError::IntOverflow)?;
        limits.check_intermediate(
            expected
                .checked_mul(core::mem::size_of::<u64>() as u64)
                .ok_or(TiffError::IntOverflow)?,
        )?;
        let mut sizes = Vec::with_capacity(count.min(1 << 20));
        for index in 0..expected {
            let plane = u16::try_from(index / chunks_per_plane.max(1)).unwrap_or(0);
            let (coded_w, coded_h) = coded_dimensions(index % chunks_per_plane.max(1));
            let bits: Vec<u16> = if planar == PlanarConfiguration::Planar {
                vec![
                    bits_per_sample
                        .get(plane as usize)
                        .copied()
                        .or_else(|| bits_per_sample.first().copied())
                        .unwrap_or(8),
                ]
            } else {
                bits_per_sample.to_vec()
            };
            let per_pixel = if planar == PlanarConfiguration::Planar {
                1u64
            } else {
                u64::from(samples_per_pixel)
            };
            let samples = u64::from(coded_w)
                .checked_mul(per_pixel)
                .ok_or(TiffError::IntOverflow)?;
            let samples = usize::try_from(samples).map_err(|_| TiffError::IntOverflow)?;
            let row = packed_row_bytes(&bits, samples);
            sizes.push(
                row.checked_mul(u64::from(coded_h))
                    .ok_or(TiffError::IntOverflow)?,
            );
        }
        Ok(Some(sizes))
    }

    #[allow(clippy::too_many_arguments)]
    fn read_chunk_tables<S: ValueSource>(
        dir: &Directory,
        source: &mut S,
        leniency: Leniency,
        warnings: &mut Warnings,
        offsets_tag: Tag,
        counts_tag: Tag,
        expected: u64,
        declared_counts: Option<Vec<u64>>,
        uncompressed_sizes: Option<Vec<u64>>,
        file_len: u64,
    ) -> Result<(Vec<u64>, Vec<u64>)> {
        let offsets = vector_u64(dir, offsets_tag, source)?.ok_or(TiffError::Format(
            FormatError::RequiredTagNotFound(offsets_tag),
        ))?;
        let counts = match declared_counts {
            Some(counts) => counts,
            None => {
                match uncompressed_sizes {
                    Some(sizes) if sizes.len() >= offsets.len() => {
                        warnings.push(Warning::AssumedDefault {
                            tag: counts_tag.to_u16(),
                            reason: "byte counts absent; derived from the uncompressed geometry",
                        });
                        sizes.get(..offsets.len()).unwrap_or(&[]).to_vec()
                    }
                    _ if offsets.len() == 1 => {
                        // A single compressed chunk can only be assumed to run
                        // to the end of the file.
                        let start = offsets.first().copied().unwrap_or(0);
                        warnings.push(Warning::AssumedDefault {
                            tag: counts_tag.to_u16(),
                            reason: "byte counts absent; single chunk assumed to run to EOF",
                        });
                        vec![file_len.saturating_sub(start)]
                    }
                    _ => return Err(TiffError::Format(FormatError::StripByteCountsMissing)),
                }
            }
        };

        if offsets.len() != counts.len() {
            if leniency.is_strict() {
                return Err(TiffError::Format(FormatError::ChunkCountMismatch {
                    offsets: offsets.len(),
                    counts: counts.len(),
                }));
            }
            warnings.push(Warning::SpecViolation {
                message: format!(
                    "{} has {} entries but {} has {}",
                    offsets_tag.name(),
                    offsets.len(),
                    counts_tag.name(),
                    counts.len()
                ),
            });
            let n = offsets.len().min(counts.len());
            let offsets = offsets.get(..n).unwrap_or(&[]).to_vec();
            let counts = counts.get(..n).unwrap_or(&[]).to_vec();
            return Ok((offsets, counts));
        }

        if offsets.len() as u64 != expected {
            if leniency.is_strict() {
                return Err(TiffError::Format(FormatError::InconsistentSizes {
                    tag: offsets_tag,
                    expected,
                    got: offsets.len() as u64,
                }));
            }
            warnings.push(Warning::SpecViolation {
                message: format!(
                    "{} has {} entries, geometry calls for {expected}",
                    offsets_tag.name(),
                    offsets.len()
                ),
            });
        }
        Ok((offsets, counts))
    }

    /// Cross-checks the geometry once every field is populated.
    fn validate(&self, limits: &Limits, leniency: Leniency, warnings: &mut Warnings) -> Result<()> {
        if let Some(map) = &self.color_map {
            let bits = self.bits_per_sample.first().copied().unwrap_or(8);
            let expected = 3usize << bits.min(16);
            if map.len() != expected {
                if leniency.is_strict() {
                    return Err(TiffError::Format(FormatError::ColorMapWrongLength {
                        expected,
                        got: map.len(),
                    }));
                }
                warnings.push(Warning::SpecViolation {
                    message: format!("ColorMap has {} entries, expected {expected}", map.len()),
                });
            }
        }
        if self.samples_per_pixel > 3 && self.extra_samples.is_empty() {
            warnings.push(Warning::SpecViolation {
                message: format!(
                    "{} samples per pixel without ExtraSamples; extras treated as unspecified",
                    self.samples_per_pixel
                ),
            });
        }
        // The whole decoded image must fit inside the configured budget.
        let total = self.expected_total_bytes()?;
        limits.check_image_bytes(total)?;
        Ok(())
    }

    /// Strips or tiles.
    #[must_use]
    pub fn chunk_type(&self) -> ChunkType {
        self.chunks.chunk_type()
    }

    /// Number of chunks across the image; 1 for strips.
    #[must_use]
    pub fn chunks_across(&self) -> u32 {
        match &self.chunks {
            ChunkGeometry::Strips { .. } => 1,
            ChunkGeometry::Tiles { tile_width, .. } => self.width.div_ceil(*tile_width),
        }
    }

    /// Number of chunks down the image.
    #[must_use]
    pub fn chunks_down(&self) -> u32 {
        match &self.chunks {
            ChunkGeometry::Strips { rows_per_strip, .. } => self.height.div_ceil(*rows_per_strip),
            ChunkGeometry::Tiles { tile_length, .. } => self.height.div_ceil(*tile_length),
        }
    }

    /// Number of planes: `SamplesPerPixel` for planar images, else 1.
    #[must_use]
    pub fn plane_count(&self) -> u16 {
        if self.planar == PlanarConfiguration::Planar {
            self.samples_per_pixel
        } else {
            1
        }
    }

    /// Chunks in one plane.
    #[must_use]
    pub fn chunks_per_plane(&self) -> u64 {
        u64::from(self.chunks_across()) * u64::from(self.chunks_down())
    }

    /// Total number of chunks, including plane multiplicity.
    #[must_use]
    pub fn chunk_count(&self) -> u64 {
        self.chunks_per_plane() * u64::from(self.plane_count())
    }

    /// The plane a chunk index belongs to.
    #[must_use]
    pub fn chunk_plane(&self, index: u64) -> u16 {
        let per_plane = self.chunks_per_plane().max(1);
        u16::try_from(index / per_plane).unwrap_or(0)
    }

    /// The nominal chunk size: `(width, rows_per_strip)` or the tile size.
    #[must_use]
    pub fn chunk_dimensions(&self) -> (u32, u32) {
        match &self.chunks {
            ChunkGeometry::Strips { rows_per_strip, .. } => {
                (self.width, (*rows_per_strip).min(self.height))
            }
            ChunkGeometry::Tiles {
                tile_width,
                tile_length,
                ..
            } => (*tile_width, *tile_length),
        }
    }

    /// The dimensions a chunk is *coded* at.
    ///
    /// Strips are clipped at the bottom of the image; tiles are always coded
    /// full size and zero padded.
    ///
    /// # Errors
    /// [`crate::UsageError::ChunkIndexOutOfRange`] for an out-of-range index.
    pub fn chunk_coded_dimensions(&self, index: u64) -> Result<(u32, u32)> {
        let count = self.chunk_count();
        if index >= count {
            return Err(TiffError::Usage(
                crate::error::UsageError::ChunkIndexOutOfRange { index, count },
            ));
        }
        let within = index % self.chunks_per_plane().max(1);
        Ok(match &self.chunks {
            ChunkGeometry::Strips { rows_per_strip, .. } => {
                let row = u32::try_from(within)
                    .ok()
                    .and_then(|v| v.checked_mul(*rows_per_strip))
                    .unwrap_or(self.height);
                let rows = (*rows_per_strip).min(self.height.saturating_sub(row));
                (self.width, rows)
            }
            ChunkGeometry::Tiles {
                tile_width,
                tile_length,
                ..
            } => (*tile_width, *tile_length),
        })
    }

    /// The dimensions of the *valid* data inside a chunk.
    ///
    /// Identical to [`Self::chunk_coded_dimensions`] for strips; clipped to the
    /// image for edge tiles.
    ///
    /// # Errors
    /// [`crate::UsageError::ChunkIndexOutOfRange`] for an out-of-range index.
    pub fn chunk_data_dimensions(&self, index: u64) -> Result<(u32, u32)> {
        let (coded_w, coded_h) = self.chunk_coded_dimensions(index)?;
        match &self.chunks {
            ChunkGeometry::Strips { .. } => Ok((coded_w, coded_h)),
            ChunkGeometry::Tiles { .. } => {
                let (x, y) = self.chunk_origin(index)?;
                Ok((
                    coded_w.min(self.width.saturating_sub(x)),
                    coded_h.min(self.height.saturating_sub(y)),
                ))
            }
        }
    }

    /// The image-space origin of a chunk.
    ///
    /// # Errors
    /// [`crate::UsageError::ChunkIndexOutOfRange`] for an out-of-range index.
    pub fn chunk_origin(&self, index: u64) -> Result<(u32, u32)> {
        let count = self.chunk_count();
        if index >= count {
            return Err(TiffError::Usage(
                crate::error::UsageError::ChunkIndexOutOfRange { index, count },
            ));
        }
        let within = index % self.chunks_per_plane().max(1);
        Ok(match &self.chunks {
            ChunkGeometry::Strips { rows_per_strip, .. } => {
                let row = u32::try_from(within)
                    .ok()
                    .and_then(|v| v.checked_mul(*rows_per_strip))
                    .unwrap_or(0);
                (0, row)
            }
            ChunkGeometry::Tiles {
                tile_width,
                tile_length,
                ..
            } => {
                let across = u64::from(self.chunks_across().max(1));
                let col = u32::try_from(within % across).unwrap_or(0);
                let row = u32::try_from(within / across).unwrap_or(0);
                (
                    col.saturating_mul(*tile_width),
                    row.saturating_mul(*tile_length),
                )
            }
        })
    }

    /// The bit depths that apply to one chunk of the given plane.
    #[must_use]
    pub fn plane_bits(&self, plane: u16) -> Vec<u16> {
        if self.planar == PlanarConfiguration::Planar {
            vec![
                self.bits_per_sample
                    .get(plane as usize)
                    .copied()
                    .or_else(|| self.bits_per_sample.first().copied())
                    .unwrap_or(8),
            ]
        } else {
            self.bits_per_sample.clone()
        }
    }

    /// Channels carried by one chunk of the given plane.
    #[must_use]
    pub fn plane_samples_per_pixel(&self) -> u16 {
        if self.planar == PlanarConfiguration::Planar {
            1
        } else {
            self.samples_per_pixel
        }
    }

    /// `true` when the pixel data is stored in YCbCr subsampling units.
    #[must_use]
    pub fn is_subsampled(&self) -> bool {
        self.photometric == PhotometricInterpretation::YCbCr
            && self.planar == PlanarConfiguration::Chunky
            && self.ycbcr_subsampling != (1, 1)
    }

    /// Bytes one packed row of a chunk occupies, including the row padding.
    ///
    /// # Errors
    /// [`TiffError::IntOverflow`] when the product does not fit.
    pub fn chunk_row_bytes(&self, coded_width: u32, plane: u16) -> Result<u64> {
        let bits = self.plane_bits(plane);
        let per_pixel = u64::from(self.plane_samples_per_pixel());
        let samples = u64::from(coded_width)
            .checked_mul(per_pixel)
            .ok_or(TiffError::IntOverflow)?;
        let samples = usize::try_from(samples).map_err(|_| TiffError::IntOverflow)?;
        Ok(packed_row_bytes(&bits, samples))
    }

    /// Bytes a decoded (but still packed and file-ordered) chunk occupies.
    ///
    /// # Errors
    /// [`crate::UsageError::ChunkIndexOutOfRange`] for a bad index,
    /// [`LimitError::DecodingBufferSize`] when the chunk is too large, and
    /// [`UnsupportedError::Subsampling`] for a subsampled layout this crate
    /// cannot size.
    pub fn chunk_packed_len(&self, index: u64, limits: &Limits) -> Result<usize> {
        let (coded_w, coded_h) = self.chunk_coded_dimensions(index)?;
        let plane = self.chunk_plane(index);
        let bytes = if self.is_subsampled() {
            let (h, v) = self.ycbcr_subsampling;
            let bits = self.bits_per_sample.first().copied().unwrap_or(8);
            if bits % 8 != 0 {
                return Err(TiffError::Unsupported(UnsupportedError::Subsampling(h, v)));
            }
            let width_bytes = u64::from(bits / 8);
            let units_across = u64::from(coded_w.div_ceil(u32::from(h)));
            let unit_rows = u64::from(coded_h.div_ceil(u32::from(v)));
            let per_unit = u64::from(h) * u64::from(v) + 2;
            units_across
                .checked_mul(unit_rows)
                .and_then(|v| v.checked_mul(per_unit))
                .and_then(|v| v.checked_mul(width_bytes))
                .ok_or(TiffError::IntOverflow)?
        } else {
            let row = self.chunk_row_bytes(coded_w, plane)?;
            row.checked_mul(u64::from(coded_h))
                .ok_or(TiffError::IntOverflow)?
        };
        limits.check_decoding_buffer(bytes)
    }

    /// Native slot type shared by every channel.
    ///
    /// # Errors
    /// [`UnsupportedError::MixedSampleFormats`] or
    /// [`UnsupportedError::MixedBitDepths`] when the channels disagree.
    pub fn sample_type(&self) -> Result<SampleType> {
        let first_format = self
            .sample_format
            .first()
            .copied()
            .unwrap_or(SampleFormat::Uint);
        if self.sample_format.iter().any(|f| *f != first_format) {
            return Err(TiffError::Unsupported(UnsupportedError::MixedSampleFormats));
        }
        let first_bits = self.bits_per_sample.first().copied().unwrap_or(8);
        let widest = self
            .bits_per_sample
            .iter()
            .copied()
            .max()
            .unwrap_or(first_bits);
        SampleType::resolve(widest, first_format)
    }

    /// The uniform bit depth, if every channel shares one.
    #[must_use]
    pub fn uniform_bit_depth(&self) -> Option<u16> {
        let first = self.bits_per_sample.first().copied()?;
        self.bits_per_sample
            .iter()
            .all(|b| *b == first)
            .then_some(first)
    }

    /// Bits in one chunky pixel.
    #[must_use]
    pub fn bits_per_pixel(&self) -> u32 {
        self.bits_per_sample.iter().map(|b| u32::from(*b)).sum()
    }

    /// A coarse description of the decoded pixel layout.
    ///
    /// Channels with different bit depths report
    /// [`ColorType::Multiband`] carrying the widest depth.
    ///
    /// # Errors
    /// [`TiffError::IntOverflow`] for a bit depth that does not fit a `u8`.
    pub fn color_type(&self) -> Result<ColorType> {
        let spp = self.samples_per_pixel;
        let Some(bits) = self.uniform_bit_depth() else {
            // Heterogeneous depths (RGB565 and friends) have no baseline
            // colour type; the raw path still decodes them (edge case E7).
            let widest = self.bits_per_sample.iter().copied().max().unwrap_or(8);
            return Ok(ColorType::Multiband {
                bit_depth: u8::try_from(widest).map_err(|_| TiffError::IntOverflow)?,
                num_samples: spp,
            });
        };
        let depth = u8::try_from(bits).map_err(|_| TiffError::IntOverflow)?;
        Ok(match (self.photometric, spp) {
            (PhotometricInterpretation::Palette, 1) => ColorType::Palette(depth),
            (
                PhotometricInterpretation::WhiteIsZero
                | PhotometricInterpretation::BlackIsZero
                | PhotometricInterpretation::TransparencyMask
                | PhotometricInterpretation::ColorFilterArray,
                1,
            ) => ColorType::Gray(depth),
            (
                PhotometricInterpretation::WhiteIsZero | PhotometricInterpretation::BlackIsZero,
                2,
            ) => ColorType::GrayA(depth),
            (PhotometricInterpretation::Rgb | PhotometricInterpretation::LinearRaw, 3) => {
                ColorType::Rgb(depth)
            }
            (PhotometricInterpretation::Rgb | PhotometricInterpretation::LinearRaw, 4) => {
                ColorType::Rgba(depth)
            }
            (PhotometricInterpretation::YCbCr, 3) => ColorType::YCbCr(depth),
            (PhotometricInterpretation::Separated, 4) => ColorType::Cmyk(depth),
            (PhotometricInterpretation::Separated, 5) => ColorType::CmykA(depth),
            (
                PhotometricInterpretation::CieLab
                | PhotometricInterpretation::IccLab
                | PhotometricInterpretation::ItuLab,
                3,
            ) => ColorType::Lab(depth),
            _ => ColorType::Multiband {
                bit_depth: depth,
                num_samples: spp,
            },
        })
    }

    /// Bytes the fully decoded, interleaved image occupies in native slots.
    ///
    /// # Errors
    /// [`TiffError::IntOverflow`] when the product does not fit in a `u64`.
    pub fn expected_total_bytes(&self) -> Result<u64> {
        let slot = self.sample_type().map_or_else(
            |_| {
                // Mixed formats still have a well-defined byte budget: use the
                // widest channel.
                let widest = self.bits_per_sample.iter().copied().max().unwrap_or(8);
                usize::from(widest.div_ceil(8).next_power_of_two().max(1))
            },
            SampleType::byte_width,
        );
        u64::from(self.width)
            .checked_mul(u64::from(self.height))
            .and_then(|v| v.checked_mul(u64::from(self.samples_per_pixel)))
            .and_then(|v| v.checked_mul(slot as u64))
            .ok_or(TiffError::IntOverflow)
    }

    /// Number of samples in the fully decoded, interleaved image.
    ///
    /// # Errors
    /// [`TiffError::IntOverflow`] when the product does not fit.
    pub fn expected_sample_count(&self) -> Result<u64> {
        u64::from(self.width)
            .checked_mul(u64::from(self.height))
            .and_then(|v| v.checked_mul(u64::from(self.samples_per_pixel)))
            .ok_or(TiffError::IntOverflow)
    }

    /// The layout [`crate::Decoder::read_image`] produces.
    ///
    /// # Errors
    /// The same set as [`Self::sample_type`] and [`Self::expected_total_bytes`].
    pub fn layout(&self) -> Result<ImageLayout> {
        let sample_type = self.sample_type()?;
        let total = self.expected_total_bytes()?;
        let total_len = usize::try_from(total).map_err(|_| TiffError::IntOverflow)?;
        let row_stride = usize::try_from(
            u64::from(self.width)
                .checked_mul(u64::from(self.samples_per_pixel))
                .and_then(|v| v.checked_mul(sample_type.byte_width() as u64))
                .ok_or(TiffError::IntOverflow)?,
        )
        .map_err(|_| TiffError::IntOverflow)?;
        Ok(ImageLayout {
            width: self.width,
            height: self.height,
            sample_type,
            samples_per_pixel: self.samples_per_pixel,
            row_stride,
            planes: 1,
            plane_stride: 0,
            total_len,
        })
    }

    /// Cross-checks the declared geometry against the compressed data actually
    /// present, so a file claiming a huge image with 40 bytes of strip data is
    /// rejected before any allocation.
    ///
    /// # Errors
    /// [`LimitError::ImageSize`] when the claim is implausible.
    pub fn check_against_chunk_sizes(&self, limits: &Limits) -> Result<()> {
        if self.compression != CompressionMethod::None {
            return Ok(());
        }
        let available: u64 = self.chunks.byte_counts().iter().copied().sum();
        let mut needed = 0u64;
        for index in 0..self.chunk_count() {
            needed = needed.saturating_add(self.chunk_packed_len(index, limits)? as u64);
        }
        if available < needed {
            return Err(TiffError::Limits(LimitError::ImageSize {
                size: needed,
                limit: usize::try_from(available).unwrap_or(usize::MAX),
            }));
        }
        Ok(())
    }
}

/// Reads a value that must exist, producing a named error when it does not.
///
/// # Errors
/// [`FormatError::RequiredTagNotFound`] when the tag is absent.
pub fn require<S: ValueSource>(dir: &Directory, tag: Tag, source: &mut S) -> Result<Value> {
    let entry = dir
        .get(tag)
        .ok_or(TiffError::Format(FormatError::RequiredTagNotFound(tag)))?;
    source.load(entry)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn color_type_geometry() {
        assert_eq!(ColorType::Gray(8).samples_per_pixel(), 1);
        assert_eq!(ColorType::GrayA(8).samples_per_pixel(), 2);
        assert_eq!(ColorType::Rgb(8).samples_per_pixel(), 3);
        assert_eq!(ColorType::Rgba(8).samples_per_pixel(), 4);
        assert_eq!(ColorType::Cmyk(8).samples_per_pixel(), 4);
        assert_eq!(ColorType::CmykA(8).samples_per_pixel(), 5);
        assert_eq!(ColorType::YCbCr(8).samples_per_pixel(), 3);
        assert_eq!(ColorType::Lab(8).samples_per_pixel(), 3);
        assert_eq!(ColorType::Palette(4).samples_per_pixel(), 1);
        assert_eq!(
            ColorType::Multiband {
                bit_depth: 16,
                num_samples: 5
            }
            .samples_per_pixel(),
            5
        );
        assert_eq!(ColorType::Rgba(16).bit_depth(), 16);
        assert_eq!(
            ColorType::Multiband {
                bit_depth: 32,
                num_samples: 2
            }
            .bit_depth(),
            32
        );
    }

    #[test]
    fn color_type_maps_to_the_right_photometric() {
        assert_eq!(
            ColorType::Gray(8).photometric(),
            PhotometricInterpretation::BlackIsZero
        );
        assert_eq!(
            ColorType::Palette(8).photometric(),
            PhotometricInterpretation::Palette
        );
        assert_eq!(
            ColorType::Rgba(8).photometric(),
            PhotometricInterpretation::Rgb
        );
        assert_eq!(
            ColorType::Cmyk(8).photometric(),
            PhotometricInterpretation::Separated
        );
        assert_eq!(
            ColorType::YCbCr(8).photometric(),
            PhotometricInterpretation::YCbCr
        );
        assert_eq!(
            ColorType::Lab(8).photometric(),
            PhotometricInterpretation::CieLab
        );
        assert_eq!(ColorType::Rgba(8).extra_samples().len(), 1);
        assert!(ColorType::Rgb(8).extra_samples().is_empty());
    }

    #[test]
    fn rect_bounds_checks_are_overflow_safe() {
        let r = Rect::new(2, 3, 4, 5);
        assert_eq!(r.area(), 20);
        assert!(r.fits_in(6, 8));
        assert!(!r.fits_in(5, 8));
        assert!(!Rect::new(u32::MAX, 0, 2, 2).fits_in(u32::MAX, u32::MAX));
    }

    #[test]
    fn chunk_type_displays() {
        assert_eq!(ChunkType::Strip.to_string(), "strip");
        assert_eq!(ChunkType::Tile.to_string(), "tile");
    }

    #[test]
    fn chunk_geometry_accessors() {
        let strips = ChunkGeometry::Strips {
            rows_per_strip: 4,
            offsets: vec![8, 40],
            byte_counts: vec![32, 16],
        };
        assert_eq!(strips.chunk_type(), ChunkType::Strip);
        assert_eq!(strips.offsets(), &[8, 40]);
        assert_eq!(strips.byte_counts(), &[32, 16]);
        let tiles = ChunkGeometry::Tiles {
            tile_width: 16,
            tile_length: 16,
            offsets: vec![8],
            byte_counts: vec![256],
        };
        assert_eq!(tiles.chunk_type(), ChunkType::Tile);
        assert_eq!(tiles.offsets(), &[8]);
        assert_eq!(tiles.byte_counts(), &[256]);
    }
}
