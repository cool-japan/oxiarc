//! Legacy OJPEG (`Compression = 6`) reconstruction, TIFF 6.0 section 22.
//!
//! TIFF 6.0's JPEG encoding was withdrawn and replaced by Technical Note 2's
//! `Compression = 7`, but files that use it still turn up — scanners and
//! early digital cameras emitted them for years, and libtiff still reads
//! them. Its defining property is that a strip need not be a datastream: the
//! tables can live in tags, the frame header can be absent entirely, and what
//! the strip holds may be nothing but entropy-coded bytes.
//!
//! Three spellings exist in the wild, and this module handles all three by
//! libtiff's strategy — scan the strip for whatever it already has, then
//! synthesise only what is missing:
//!
//! * **(a) interchange.** Tags 513/514 (`JPEGInterchangeFormat` and
//!   `JPEGInterchangeFormatLength`) point at a complete datastream covering
//!   the whole image; strips overlap or duplicate it. Pass the resolved bytes
//!   as [`OJpegTags::interchange`] and an empty strip to decode it whole.
//! * **(b) tables by offset.** The TIFF 6.0 spec form: the strip carries no
//!   markers at all and tags 519/520/521 hold *offsets* to raw tables. Pass
//!   the resolved payloads in [`OJpegTags::q_tables`], `dc_tables` and
//!   `ac_tables`, and a frame header is synthesised from
//!   [`OJpegGeometry`].
//! * **(c) hybrid.** The strip begins at some marker other than `SOI` — often
//!   `SOS`, sometimes an `RSTn` — with the tables split between the strip and
//!   the tags. Whatever the strip carries wins; the rest comes from the tags.
//!
//! # What the caller resolves
//!
//! Everything here takes **bytes**, never offsets: resolving a tag offset
//! needs the file, and that is the TIFF layer's job. `oxiarc-tiff` reads tag
//! 519 as an array of `LONG` offsets, seeks to each, reads 64 bytes, and
//! passes them in; the same for 520/521 (16 `BITS` counts followed by
//! `sum(BITS)` values, i.e. a `DHT` payload without its `Tc/Th` byte).
//!
//! # Colour
//!
//! Nothing here applies `PhotometricInterpretation`, `ReferenceBlackWhite` or
//! `YCbCrCoefficients`: those are TIFF's, not JPEG's, and the container owns
//! them. [`decode_ojpeg`] decodes raw components, exactly as
//! [`crate::decode_abbreviated_into`] does for `Compression = 7`.
//!
//! # Writing
//!
//! There is none. `Compression = 6` is deprecated by TTN2, libtiff itself
//! refuses to write it, and this crate follows.

use crate::decoder::{DecodeOptions, Decoder, ImageInfo};
use crate::error::{JpegError, Result};

/// The OJPEG tags, with every offset already resolved to bytes.
#[derive(Debug, Clone, Default)]
pub struct OJpegTags<'a> {
    /// Tag 512 `JPEGProc`: `1` for baseline sequential, `14` for lossless.
    pub jpeg_proc: Option<u16>,
    /// Tags 513/514 `JPEGInterchangeFormat`/`Length`, resolved to the bytes
    /// of the complete datastream.
    pub interchange: Option<&'a [u8]>,
    /// Tag 515 `JPEGRestartInterval`.
    pub restart_interval: Option<u16>,
    /// Tag 517 `JPEGLosslessPredictors`, one per component.
    pub lossless_predictors: Vec<u16>,
    /// Tag 518 `JPEGPointTransform`, one per component.
    pub point_transforms: Vec<u16>,
    /// Tag 519 `JPEGQTables`, resolved: 64 raw quantiser bytes in zig-zag
    /// order per entry, one entry per component.
    pub q_tables: Vec<Vec<u8>>,
    /// Tag 520 `JPEGDCTables`, resolved: 16 `BITS` counts followed by
    /// `sum(BITS)` values per entry.
    pub dc_tables: Vec<Vec<u8>>,
    /// Tag 521 `JPEGACTables`, same shape as `dc_tables`.
    pub ac_tables: Vec<Vec<u8>>,
}

/// The geometry a strip's synthesised frame header needs.
///
/// A strip carries no `SOF`, so every field here comes from the IFD. `height`
/// is the height of **this** strip or tile, not of the image.
#[derive(Debug, Clone, Copy)]
pub struct OJpegGeometry {
    /// Width of this strip or tile in pixels (`ImageWidth`, or `TileWidth`).
    pub width: u16,
    /// Height of this strip or tile in pixels.
    pub height: u16,
    /// `BitsPerSample`, which OJPEG requires to be equal for every component.
    pub bits_per_sample: u8,
    /// Number of components **in this strip**, `1..=4`.
    ///
    /// That is `SamplesPerPixel` for the usual chunky layout, and `1` when
    /// `PlanarConfiguration = 2`, where each strip holds one component.
    pub samples_per_pixel: u8,
    /// `PhotometricInterpretation`; only `6` (YCbCr) changes the sampling
    /// factors written into the `SOF`.
    pub photometric: u16,
    /// `YCbCrSubSampling` as `(horizontal, vertical)`, used for component 0
    /// when `photometric == 6`.
    pub subsampling: (u8, u8),
    /// `PlanarConfiguration`. `2` means one component per strip, which also
    /// means no component of this strip is subsampled whatever tag 530 says.
    pub planar_config: u16,
}

impl Default for OJpegGeometry {
    fn default() -> Self {
        Self {
            width: 0,
            height: 0,
            bits_per_sample: 8,
            samples_per_pixel: 1,
            photometric: 1,
            subsampling: (1, 1),
            planar_config: 1,
        }
    }
}

/// One marker segment found at the head of a strip.
#[derive(Debug, Clone, Copy)]
struct Segment {
    code: u8,
    /// Range of the whole segment, marker bytes included.
    start: usize,
    end: usize,
}

/// What a strip (or an interchange stream) already carries.
#[derive(Debug, Default)]
struct Shape {
    segments: Vec<Segment>,
    /// Offset of the first entropy-coded byte.
    entropy_start: usize,
    has_quant: bool,
    has_huffman: bool,
    has_dri: bool,
    has_frame: bool,
    /// Range of the `SOS` segment, when there is one.
    scan: Option<(usize, usize)>,
}

/// Read the marker segments at the head of `data`, stopping at the entropy
/// data.
///
/// A strip may start anywhere: at `SOI`, at a table, at `SOS`, at an `RSTn`,
/// or straight in the middle of the entropy-coded data. Anything that is not
/// a well-formed segment ends the header.
fn shape_of(data: &[u8]) -> Shape {
    let mut shape = Shape::default();
    let mut pos = 0usize;
    while pos + 1 < data.len() {
        if data[pos] != 0xFF {
            break;
        }
        // A run of `0xFF` fill bytes may precede any marker (T.81 B.1.1.2).
        let mut probe = pos + 1;
        while data.get(probe) == Some(&0xFF) {
            probe += 1;
        }
        let Some(&code) = data.get(probe) else {
            break;
        };
        match code {
            // `SOI` is structural; it is re-emitted, never copied.
            0xD8 => {
                pos = probe + 1;
                shape.entropy_start = pos;
                continue;
            }
            // A leading `RSTn` is the marker that separated this strip from
            // the one before it — the flavour (c) spelling libtiff
            // resynchronises on. Every strip is decoded on its own, with the
            // predictions and the restart counter reset by its own `SOS`, so
            // it is not a boundary the entropy decoder is waiting for: kept,
            // it would stop the decode before the first MCU and the strip
            // would decode to nothing.
            0xD0..=0xD7 => {
                pos = probe + 1;
                shape.entropy_start = pos;
                continue;
            }
            // Entropy data, a stuffed zero or the end: the header is over.
            0x00 | 0x01 | 0xD9 => break,
            _ => {}
        }
        if probe + 2 >= data.len() {
            break;
        }
        let length = usize::from(u16::from_be_bytes([data[probe + 1], data[probe + 2]]));
        if length < 2 || probe + 1 + length > data.len() {
            break;
        }
        let segment = Segment {
            code,
            // One `0xFF` is kept; a longer fill run is dropped rather than
            // copied into the synthesised stream.
            start: probe - 1,
            end: probe + 1 + length,
        };
        match code {
            0xDB => shape.has_quant = true,
            0xC4 => shape.has_huffman = true,
            0xDD => shape.has_dri = true,
            0xC0..=0xCF => shape.has_frame = true,
            _ => {}
        }
        pos = segment.end;
        if code == 0xDA {
            shape.scan = Some((segment.start, segment.end));
            shape.entropy_start = segment.end;
            return shape;
        }
        shape.segments.push(segment);
        shape.entropy_start = pos;
    }
    shape.entropy_start = pos.min(data.len());
    shape
}

/// Append a marker segment with its length field.
fn segment(out: &mut Vec<u8>, code: u8, payload: &[u8]) -> Result<()> {
    let length = payload.len() + 2;
    if length > 0xFFFF {
        return Err(JpegError::malformed(
            "OJPEG",
            0,
            "a synthesised segment is longer than 65533 bytes",
        ));
    }
    out.push(0xFF);
    out.push(code);
    out.push((length >> 8) as u8);
    out.push((length & 0xFF) as u8);
    out.extend_from_slice(payload);
    Ok(())
}

/// `DQT` segments from tag 519's resolved payloads.
fn emit_quant_tables(out: &mut Vec<u8>, tables: &[Vec<u8>]) -> Result<()> {
    for (index, table) in tables.iter().enumerate().take(4) {
        if table.len() != 64 {
            return Err(JpegError::malformed(
                "JPEGQTables",
                index,
                "an OJPEG quantisation table must be exactly 64 bytes",
            ));
        }
        let mut payload = Vec::with_capacity(65);
        // OJPEG quantisation values are always eight-bit, so `Pq` is zero.
        payload.push(index as u8);
        payload.extend_from_slice(table);
        segment(out, 0xDB, &payload)?;
    }
    Ok(())
}

/// `DHT` segments from tags 520/521's resolved payloads.
///
/// `class` is `0` for tag 520 (DC) and `1` for tag 521 (AC); it selects both
/// the `Tc` nibble and the tag an error names.
fn emit_huffman_tables(out: &mut Vec<u8>, tables: &[Vec<u8>], class: u8) -> Result<()> {
    let tag = if class == 0 {
        "JPEGDCTables"
    } else {
        "JPEGACTables"
    };
    for (index, table) in tables.iter().enumerate().take(4) {
        if table.len() < 16 {
            return Err(JpegError::malformed(
                tag,
                index,
                "an OJPEG Huffman table needs sixteen BITS counts",
            ));
        }
        let counted: usize = table[..16].iter().map(|&n| usize::from(n)).sum();
        if table.len() < 16 + counted {
            return Err(JpegError::malformed(
                tag,
                index,
                "an OJPEG Huffman table is shorter than its BITS counts claim",
            ));
        }
        let mut payload = Vec::with_capacity(17 + counted);
        payload.push((class << 4) | index as u8);
        payload.extend_from_slice(&table[..16 + counted]);
        segment(out, 0xC4, &payload)?;
    }
    Ok(())
}

/// The `SOF` marker code the `JPEGProc` tag selects.
fn frame_marker(tags: &OJpegTags<'_>) -> Result<u8> {
    match tags.jpeg_proc {
        None | Some(1) => Ok(0xC0),
        Some(14) => Ok(0xC3),
        Some(other) => Err(JpegError::malformed(
            "JPEGProc",
            usize::from(other),
            "only JPEGProc 1 (baseline) and 14 (lossless) are defined",
        )),
    }
}

/// Synthesise the `SOF` a marker-free strip lacks.
fn emit_frame_header(
    out: &mut Vec<u8>,
    tags: &OJpegTags<'_>,
    geometry: &OJpegGeometry,
) -> Result<()> {
    let components = usize::from(geometry.samples_per_pixel).clamp(1, 4);
    if geometry.width == 0 || geometry.height == 0 {
        return Err(JpegError::malformed(
            "OJPEG",
            0,
            "an OJPEG strip needs a non-zero width and height",
        ));
    }
    let mut payload = Vec::with_capacity(6 + 3 * components);
    payload.push(geometry.bits_per_sample);
    payload.extend_from_slice(&geometry.height.to_be_bytes());
    payload.extend_from_slice(&geometry.width.to_be_bytes());
    payload.push(components as u8);
    // Only a YCbCr photometric decimates chroma, and only its first component
    // carries the sampling factors; `PlanarConfiguration = 2` means one
    // component per strip, so that strip is 1x1 whatever the tag says.
    let subsampled = geometry.photometric == 6 && geometry.planar_config != 2 && components > 1;
    for index in 0..components {
        payload.push(index as u8 + 1);
        let (h, v) = if subsampled && index == 0 {
            (
                geometry.subsampling.0.clamp(1, 4),
                geometry.subsampling.1.clamp(1, 4),
            )
        } else {
            (1, 1)
        };
        payload.push((h << 4) | v);
        // libtiff's rule for a short table array: the last entry repeats.
        let tq = table_index(index, tags.q_tables.len());
        payload.push(tq as u8);
    }
    segment(out, frame_marker(tags)?, &payload)
}

/// The table a component selects when the tag array is shorter than the
/// component count, which libtiff tolerates by repeating the last entry.
fn table_index(component: usize, available: usize) -> usize {
    if available == 0 {
        0
    } else {
        component.min(available - 1).min(3)
    }
}

/// Synthesise the `SOS` a marker-free strip lacks.
fn emit_scan_header(
    out: &mut Vec<u8>,
    tags: &OJpegTags<'_>,
    geometry: &OJpegGeometry,
) -> Result<()> {
    let components = usize::from(geometry.samples_per_pixel).clamp(1, 4);
    let lossless = tags.jpeg_proc == Some(14);
    let mut payload = Vec::with_capacity(4 + 2 * components);
    payload.push(components as u8);
    for index in 0..components {
        payload.push(index as u8 + 1);
        let td = table_index(index, tags.dc_tables.len());
        let ta = if lossless {
            0
        } else {
            table_index(index, tags.ac_tables.len())
        };
        payload.push(((td as u8) << 4) | ta as u8);
    }
    if lossless {
        // T.81 H.1: `Ss` carries the predictor and `Al` the point transform.
        let predictor = tags.lossless_predictors.first().copied().unwrap_or(1);
        let point_transform = tags.point_transforms.first().copied().unwrap_or(0);
        payload.push((predictor & 0xFF) as u8);
        payload.push(0);
        payload.push((point_transform & 0x0F) as u8);
    } else {
        payload.push(0);
        payload.push(63);
        payload.push(0);
    }
    segment(out, 0xDA, &payload)
}

/// Rebuild a decodable JPEG datastream for one OJPEG strip or tile.
///
/// The strip is scanned first: every marker segment it already carries is
/// kept, in its original order, and only what is missing is synthesised from
/// `tags` and `geometry`. When [`OJpegTags::interchange`] is present, its
/// header segments are the second source, ahead of the tags — that is how a
/// flavour (a) file, whose strips are bare entropy data, gets its tables.
///
/// Passing an empty `strip` together with an interchange stream returns that
/// stream unchanged, which is the whole-image path for flavour (a).
///
/// # Examples
///
/// ```
/// # fn main() -> Result<(), oxiarc_jpeg::JpegError> {
/// use oxiarc_jpeg::tiff::{OJpegGeometry, OJpegTags, reconstruct_ojpeg};
///
/// // A flavour (a) file: the whole datastream sits in tags 513/514.
/// let tags = OJpegTags {
///     jpeg_proc: Some(1),
///     interchange: Some(&oxiarc_jpeg::sample::GRAY_1X1),
///     ..Default::default()
/// };
/// let geometry = OJpegGeometry {
///     width: 1,
///     height: 1,
///     ..Default::default()
/// };
/// let stream = reconstruct_ojpeg(&tags, &geometry, &[])?;
/// assert_eq!(&stream[..2], &[0xFF, 0xD8]);
/// # Ok(())
/// # }
/// ```
pub fn reconstruct_ojpeg(
    tags: &OJpegTags<'_>,
    geometry: &OJpegGeometry,
    strip: &[u8],
) -> Result<Vec<u8>> {
    if strip.is_empty() {
        let interchange = tags.interchange.ok_or_else(|| {
            JpegError::malformed(
                "OJPEG",
                0,
                "an empty strip needs a JPEGInterchangeFormat stream to decode",
            )
        })?;
        if interchange.is_empty() {
            return Err(JpegError::eof("JPEGInterchangeFormat"));
        }
        return Ok(interchange.to_vec());
    }

    let strip_shape = shape_of(strip);
    let interchange = tags.interchange.unwrap_or(&[]);
    let interchange_shape = shape_of(interchange);

    let mut out = Vec::with_capacity(strip.len() + 512);
    out.extend_from_slice(&[0xFF, 0xD8]);

    // Tables: the strip wins, then the interchange stream's, then the tags'.
    let copy_from_interchange = |kinds: &[u8], out: &mut Vec<u8>| -> bool {
        let mut copied = false;
        for segment in &interchange_shape.segments {
            if kinds.contains(&segment.code) {
                out.extend_from_slice(&interchange[segment.start..segment.end]);
                copied = true;
            }
        }
        copied
    };

    if !strip_shape.has_quant && !copy_from_interchange(&[0xDB], &mut out) {
        emit_quant_tables(&mut out, &tags.q_tables)?;
    }
    if !strip_shape.has_huffman && !copy_from_interchange(&[0xC4], &mut out) {
        emit_huffman_tables(&mut out, &tags.dc_tables, 0)?;
        emit_huffman_tables(&mut out, &tags.ac_tables, 1)?;
    }
    if !strip_shape.has_dri && !copy_from_interchange(&[0xDD], &mut out) {
        if let Some(interval) = tags.restart_interval.filter(|&n| n != 0) {
            segment(&mut out, 0xDD, &interval.to_be_bytes())?;
        }
    }

    // Everything else the strip carried, in its own order.
    for segment in &strip_shape.segments {
        out.extend_from_slice(&strip[segment.start..segment.end]);
    }
    if !strip_shape.has_frame {
        emit_frame_header(&mut out, tags, geometry)?;
    }
    match strip_shape.scan {
        Some((start, end)) => out.extend_from_slice(&strip[start..end]),
        None => emit_scan_header(&mut out, tags, geometry)?,
    }

    out.extend_from_slice(&strip[strip_shape.entropy_start..]);
    if out.len() < 2 || out[out.len() - 2..] != [0xFF, 0xD9] {
        out.extend_from_slice(&[0xFF, 0xD9]);
    }
    Ok(out)
}

/// Rebuild and decode one OJPEG strip into `out`, without a colour transform.
///
/// `out` holds `width * height * samples_per_pixel` interleaved samples of the
/// reconstructed components — chroma already upsampled, no photometric
/// interpretation applied. See [`reconstruct_ojpeg`] for the reconstruction
/// rules.
pub fn decode_ojpeg_into(
    tags: &OJpegTags<'_>,
    geometry: &OJpegGeometry,
    strip: &[u8],
    options: &DecodeOptions,
    out: &mut [u8],
) -> Result<ImageInfo> {
    let stream = reconstruct_ojpeg(tags, geometry, strip)?;
    let mut decoder = Decoder::with_options(stream.as_slice(), options.clone());
    decoder.read_info()?;
    decoder.decode_into(out)?;
    decoder.info().ok_or(JpegError::AbbreviatedWithoutFrame)
}

/// Rebuild and decode one OJPEG strip into a wide sample buffer.
///
/// `JPEGProc = 14` (lossless) allows precisions up to sixteen bits, which
/// [`decode_ojpeg_into`] refuses rather than truncating. This is the entry
/// point for those, and it accepts eight-bit strips as well.
pub fn decode_ojpeg_into_u16(
    tags: &OJpegTags<'_>,
    geometry: &OJpegGeometry,
    strip: &[u8],
    options: &DecodeOptions,
    out: &mut [u16],
) -> Result<ImageInfo> {
    let stream = reconstruct_ojpeg(tags, geometry, strip)?;
    let mut decoder = Decoder::with_options(stream.as_slice(), options.clone());
    decoder.read_info()?;
    decoder.decode_into_u16(out)?;
    decoder.info().ok_or(JpegError::AbbreviatedWithoutFrame)
}

/// Rebuild and decode one OJPEG strip into a fresh buffer.
///
/// # Examples
///
/// ```
/// # fn main() -> Result<(), oxiarc_jpeg::JpegError> {
/// use oxiarc_jpeg::DecodeOptions;
/// use oxiarc_jpeg::tiff::{OJpegGeometry, OJpegTags, decode_ojpeg};
///
/// let tags = OJpegTags {
///     jpeg_proc: Some(1),
///     interchange: Some(&oxiarc_jpeg::sample::GRAY_1X1),
///     ..Default::default()
/// };
/// let geometry = OJpegGeometry {
///     width: 1,
///     height: 1,
///     ..Default::default()
/// };
/// let (info, pixels) = decode_ojpeg(&tags, &geometry, &[], &DecodeOptions::raw())?;
/// assert_eq!((info.width, info.height), (1, 1));
/// assert_eq!(pixels.len(), 1);
/// # Ok(())
/// # }
/// ```
pub fn decode_ojpeg(
    tags: &OJpegTags<'_>,
    geometry: &OJpegGeometry,
    strip: &[u8],
    options: &DecodeOptions,
) -> Result<(ImageInfo, Vec<u8>)> {
    let stream = reconstruct_ojpeg(tags, geometry, strip)?;
    let mut decoder = Decoder::with_options(stream.as_slice(), options.clone());
    decoder.read_info()?;
    let pixels = decoder.decode()?;
    let info = decoder.info().ok_or(JpegError::AbbreviatedWithoutFrame)?;
    Ok((info, pixels))
}
