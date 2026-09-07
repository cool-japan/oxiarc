//! The chunk decode pipeline.
//!
//! For every strip or tile the pipeline runs, in this order:
//!
//! 1. **bounds** — the offset and byte count are validated against the file
//!    length and the configured [`Limits`];
//! 2. **fetch** — the compressed bytes are read into a reusable buffer (never a
//!    fresh allocation per chunk);
//! 3. **sizing** — the packed length is computed from the *coded* dimensions
//!    (tiles are padded, strips are clipped);
//! 4. **fill order** — the bits of every byte of the still-compressed chunk are
//!    reversed when `FillOrder` is 2, exactly where libtiff does it
//!    (`TIFFFillStrip`), for every bit depth, and skipped for the CCITT codecs
//!    that consume the tag themselves;
//! 5. **decompress** — straight into the pre-sized packed buffer;
//! 6. **predictor** — undone in place, in the *file's* byte order;
//! 7. **byte order** — swapped to the host's, *after* the predictor;
//! 8. **unpack** — sub-byte, 12- and 24-bit samples expanded into native slots;
//! 9. **subsampling** — YCbCr units expanded to full resolution;
//! 10. **place** — the valid sub-rectangle is copied into the image buffer by
//!     [`strip`] or [`tile`].
//!
//! Steps 4-8 all operate on the same buffer wherever possible; only bit
//! unpacking and subsampling need a second one.

pub mod strip;
pub mod tile;

use std::io::{Read, Seek};

use crate::byteorder::EndianReader;
use crate::compression::{CodecContext, CodecRegistry, decode_into_with};
use crate::error::{FormatError, Result, TiffError};
use crate::image::{ImageInfo, Rect};
use crate::limits::{Leniency, Limits, OutputBudget, Warning, Warnings};
use crate::sample::{
    SampleType, apply_fill_order, is_byte_aligned_depth, packed_row_bytes, to_native_endian,
    unpack_row,
};
use crate::tags::{PlanarConfiguration, Predictor};

/// The reusable buffers one decode pass owns.
#[derive(Debug, Default)]
pub struct ChunkBuffers {
    compressed: Vec<u8>,
    packed: Vec<u8>,
    native: Vec<u8>,
    scratch: Vec<u8>,
}

impl ChunkBuffers {
    /// Fresh, empty buffers.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The decoded, native-endian, unpacked chunk.
    #[must_use]
    pub fn native(&self) -> &[u8] {
        &self.native
    }

    /// The raw compressed bytes of the last chunk fetched.
    #[must_use]
    pub fn compressed(&self) -> &[u8] {
        &self.compressed
    }

    /// Loads pre-fetched compressed bytes, as an alternative to
    /// [`fetch_chunk`] for a caller that already has them (a parallel
    /// driver's serial fetch pass, a byte-range reader).
    #[cfg(feature = "rayon")]
    pub(crate) fn load_compressed(&mut self, data: Vec<u8>) {
        self.compressed = data;
    }

    /// Takes ownership of the decoded, native-endian buffer, leaving an empty
    /// one behind. For a caller (the `rayon` driver) that decodes into a
    /// per-chunk [`ChunkBuffers`] and needs to move the result across a
    /// thread boundary without a copy.
    #[cfg(feature = "rayon")]
    pub(crate) fn take_native(&mut self) -> Vec<u8> {
        core::mem::take(&mut self.native)
    }

    /// Total bytes currently held, for diagnostics and allocation tests.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.compressed.capacity()
            + self.packed.capacity()
            + self.native.capacity()
            + self.scratch.capacity()
    }
}

/// The shape of a decoded chunk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChunkShape {
    /// Width the chunk was coded at.
    pub coded_width: u32,
    /// Height the chunk was coded at.
    pub coded_height: u32,
    /// Image-space x of the chunk's top-left corner.
    pub origin_x: u32,
    /// Image-space y of the chunk's top-left corner.
    pub origin_y: u32,
    /// Valid width once edge clipping is applied.
    pub valid_width: u32,
    /// Valid height once edge clipping is applied.
    pub valid_height: u32,
    /// Which plane this chunk belongs to (0 for chunky).
    pub plane: u16,
    /// Channels carried by this chunk.
    pub samples_per_pixel: u16,
    /// Native slot type of every sample.
    pub sample_type: SampleType,
}

impl ChunkShape {
    /// Bytes one coded row of the decoded chunk occupies.
    #[must_use]
    pub fn row_bytes(&self) -> usize {
        self.coded_width as usize
            * usize::from(self.samples_per_pixel)
            * self.sample_type.byte_width()
    }
}

/// Reads the compressed bytes of chunk `index` into `buffers`.
///
/// # Errors
/// [`FormatError::ChunkOffsetOutOfBounds`] when the chunk lies outside the
/// file (truncated with a warning under [`Leniency::Lenient`]), plus limits and
/// I/O failures.
pub fn fetch_chunk<R: Read + Seek>(
    reader: &mut EndianReader<R>,
    info: &ImageInfo,
    index: u64,
    limits: &Limits,
    leniency: Leniency,
    warnings: &mut Warnings,
    buffers: &mut ChunkBuffers,
) -> Result<()> {
    let idx = usize::try_from(index).map_err(|_| TiffError::IntOverflow)?;
    let offset = info
        .chunks
        .offsets()
        .get(idx)
        .copied()
        .ok_or(TiffError::Usage(
            crate::error::UsageError::ChunkIndexOutOfRange {
                index,
                count: info.chunk_count(),
            },
        ))?;
    let mut count = info.chunks.byte_counts().get(idx).copied().unwrap_or(0);
    let file_len = reader.len();
    if !reader.range_in_bounds(offset, count) {
        if leniency.is_lenient() && offset < file_len {
            let clipped = file_len - offset;
            warnings.push(Warning::ChunkTruncated {
                index,
                dropped: count - clipped,
            });
            count = clipped;
        } else {
            return Err(TiffError::Format(FormatError::ChunkOffsetOutOfBounds {
                index,
                offset,
                len: count,
                file_len,
            }));
        }
    }
    let len = limits.check_intermediate(count)?;
    buffers.compressed.clear();
    buffers.compressed.resize(len, 0);
    reader.read_exact_at(offset, &mut buffers.compressed)?;
    Ok(())
}

/// Decodes chunk `index` all the way to native-endian, unpacked samples.
///
/// The result lives in [`ChunkBuffers::native`]; the returned [`ChunkShape`]
/// says how to interpret it.
///
/// # Errors
/// Every failure the ten pipeline steps can produce.
#[allow(clippy::too_many_arguments)]
pub fn decode_chunk<R: Read + Seek>(
    reader: &mut EndianReader<R>,
    info: &ImageInfo,
    index: u64,
    limits: &Limits,
    leniency: Leniency,
    budget: &mut OutputBudget,
    registry: Option<&CodecRegistry>,
    warnings: &mut Warnings,
    buffers: &mut ChunkBuffers,
) -> Result<ChunkShape> {
    fetch_chunk(reader, info, index, limits, leniency, warnings, buffers)?;
    decode_fetched_chunk(
        info, index, limits, leniency, budget, registry, warnings, buffers,
    )
}

/// The packed (post-decompress, pre-unpack) byte length chunk `index`
/// decodes to.
///
/// This is step 3 of the pipeline (see the module docs), factored out of
/// [`decode_fetched_chunk`] so a caller that needs the size *before*
/// decoding -- a parallel driver precharging [`OutputBudget`] for a whole
/// batch, or a byte-range reader sizing its fetch -- computes exactly the
/// number [`decode_fetched_chunk`] itself will use, rather than a second copy
/// that can drift from it. A JPEG chunk decodes to full-resolution
/// interleaved components even when the image is subsampled, because the
/// JPEG stream owns the sampling factors and upsamples as it renders;
/// everything else decodes to TIFF subsampling units.
///
/// # Errors
/// The same set as [`ImageInfo::chunk_packed_len`], plus
/// [`TiffError::IntOverflow`] and [`crate::LimitError`] from the size guard.
pub fn chunk_output_len(info: &ImageInfo, index: u64, limits: &Limits) -> Result<usize> {
    let (coded_width, coded_height) = info.chunk_coded_dimensions(index)?;
    let sample_type = info.sample_type()?;
    let codec_expands =
        info.is_subsampled() && crate::compression::expands_subsampling(info.compression);
    if codec_expands {
        let slot = sample_type.byte_width();
        let bytes = (coded_width as u64)
            .checked_mul(u64::from(coded_height))
            .and_then(|n| n.checked_mul(3))
            .and_then(|n| n.checked_mul(slot as u64))
            .ok_or(TiffError::IntOverflow)?;
        limits.check_decoding_buffer(bytes)
    } else {
        info.chunk_packed_len(index, limits)
    }
}

/// Runs steps 3-9 on the bytes already in [`ChunkBuffers::compressed`].
///
/// Split out of [`decode_chunk`] so a caller that fetches chunks itself (a
/// byte-range reader, a parallel prefetch) can reuse the pipeline.
///
/// The compressed buffer is *consumed*: when `FillOrder` is 2 its bytes are
/// reversed in place, so decoding the same chunk twice needs a fresh fetch.
///
/// # Errors
/// Every failure the pipeline steps can produce.
#[allow(clippy::too_many_arguments)]
pub fn decode_fetched_chunk(
    info: &ImageInfo,
    index: u64,
    limits: &Limits,
    leniency: Leniency,
    budget: &mut OutputBudget,
    registry: Option<&CodecRegistry>,
    warnings: &mut Warnings,
    buffers: &mut ChunkBuffers,
) -> Result<ChunkShape> {
    let (coded_width, coded_height) = info.chunk_coded_dimensions(index)?;
    let (valid_width, valid_height) = info.chunk_data_dimensions(index)?;
    let (origin_x, origin_y) = info.chunk_origin(index)?;
    let plane = info.chunk_plane(index);
    let bits = info.plane_bits(plane);
    let chunk_spp = info.plane_samples_per_pixel();
    let sample_type = info.sample_type()?;

    // 3. sizing. A JPEG chunk decodes to full-resolution interleaved
    // components even when the image is subsampled, because the JPEG stream
    // owns the sampling factors and upsamples as it renders; everything else
    // decodes to TIFF subsampling units that step 9 expands. The byte count
    // itself comes from `chunk_output_len`, the single source of truth a
    // parallel driver also precharges its budget from; `codec_expands` is
    // re-derived here (cheap, pure) because steps 7-9 below still branch on
    // it.
    let subsampled = info.is_subsampled();
    let codec_expands = subsampled && crate::compression::expands_subsampling(info.compression);
    let packed_len = chunk_output_len(info, index, limits)?;
    buffers.packed.clear();
    buffers.packed.resize(packed_len, 0);

    // 4. fill order, on the still-compressed bytes (see `apply_fill_order`)
    if !crate::compression::handles_fill_order(info.compression) {
        apply_fill_order(&mut buffers.compressed, info.fill_order);
    }

    // 5. decompress
    let cx = CodecContext {
        compression: info.compression,
        photometric: info.photometric,
        fill_order: info.fill_order,
        width: coded_width as usize,
        height: coded_height as usize,
        bits_per_sample: &bits,
        samples_per_pixel: chunk_spp,
        planar: info.planar,
        plane,
        t4_options: info.t4_options,
        t6_options: info.t6_options,
        ycbcr_subsampling: if info.photometric == crate::tags::PhotometricInterpretation::YCbCr {
            info.ycbcr_subsampling
        } else {
            (1, 1)
        },
        // Decode-only: the interval a stream was coded with lives in its own
        // `DRI` segment, never in a tag.
        jpeg_restart_rows: 0,
        jpeg_tables: info.jpeg_tables.as_deref(),
        old_jpeg: info.old_jpeg.as_ref(),
        endian: info.endian,
        leniency,
        state: Some(&info.codec_state),
        max_scratch_bytes: limits.intermediate_buffer_size,
    };
    let produced = {
        let compressed = core::mem::take(&mut buffers.compressed);
        let result = decode_into_with(&compressed, &mut buffers.packed, &cx, registry);
        buffers.compressed = compressed;
        result?
    };
    if produced < packed_len {
        let last_chunk = index + 1 == info.chunk_count();
        if leniency.is_strict() || !(last_chunk || leniency.is_lenient()) {
            return Err(TiffError::Format(FormatError::ChunkSizeMismatch {
                index,
                expected: packed_len,
                got: produced,
            }));
        }
        warnings.push(Warning::SpecViolation {
            message: format!(
                "chunk {index} decoded to {produced} bytes, geometry calls for {packed_len}"
            ),
        });
    }
    budget.charge(packed_len as u64)?;

    // 6. predictor (in the file's byte order)
    if info.predictor != Predictor::None {
        crate::predictor::validate(info.predictor, &bits, info.compression)?;
        let bytes_per_sample = usize::from(bits.first().copied().unwrap_or(8) / 8).max(1);
        let stride = if info.planar == PlanarConfiguration::Planar {
            1
        } else {
            usize::from(chunk_spp)
        };
        crate::predictor::apply_predictor_reverse(
            &mut buffers.packed,
            info.predictor,
            bytes_per_sample,
            stride,
            coded_width as usize,
            info.endian,
        )?;
    }

    // 7 + 8. byte order and bit unpacking
    let uniform = bits
        .first()
        .copied()
        .filter(|b| bits.iter().all(|x| x == b));
    let byte_aligned = uniform.map(is_byte_aligned_depth).unwrap_or(false);
    let samples_per_row = coded_width as usize * usize::from(chunk_spp);
    let slot = sample_type.byte_width();

    if byte_aligned && (!subsampled || codec_expands) {
        to_native_endian(&mut buffers.packed, info.endian, &bits);
        let native_len = samples_per_row
            .checked_mul(coded_height as usize)
            .and_then(|n| n.checked_mul(slot))
            .ok_or(TiffError::IntOverflow)?;
        limits.check_decoding_buffer(native_len as u64)?;
        if native_len == buffers.packed.len() {
            // Byte-aligned samples need no unpacking, so the decoded buffer
            // *is* the native buffer: swapping instead of copying saves one
            // pass over every chunk of the image (the packed buffer is
            // re-sized for the next chunk anyway).
            core::mem::swap(&mut buffers.native, &mut buffers.packed);
        } else {
            buffers.native.clear();
            buffers.native.resize(native_len, 0);
            let copy = native_len.min(buffers.packed.len());
            if let Some(dst) = buffers.native.get_mut(..copy) {
                if let Some(src) = buffers.packed.get(..copy) {
                    dst.copy_from_slice(src);
                }
            }
        }
    } else if subsampled {
        // The packed buffer holds subsampling units; expand to full resolution.
        let native_len = (coded_width as usize)
            .checked_mul(coded_height as usize)
            .and_then(|n| n.checked_mul(3))
            .and_then(|n| n.checked_mul(slot))
            .ok_or(TiffError::IntOverflow)?;
        limits.check_decoding_buffer(native_len as u64)?;
        buffers.native.clear();
        buffers.native.resize(native_len, 0);
        to_native_endian(&mut buffers.packed, info.endian, &bits);
        crate::colour::expand_ycbcr_subsampling(
            &buffers.packed,
            coded_width,
            coded_height,
            info.ycbcr_subsampling,
            slot,
            &mut buffers.native,
        )?;
    } else {
        let native_len = samples_per_row
            .checked_mul(coded_height as usize)
            .and_then(|n| n.checked_mul(slot))
            .ok_or(TiffError::IntOverflow)?;
        limits.check_decoding_buffer(native_len as u64)?;
        buffers.native.clear();
        buffers.native.resize(native_len, 0);
        let row_bytes = packed_row_bytes(&bits, samples_per_row) as usize;
        let native_row = samples_per_row * slot;
        for row in 0..coded_height as usize {
            let src_start = row * row_bytes;
            let src = buffers
                .packed
                .get(src_start..(src_start + row_bytes).min(buffers.packed.len()))
                .unwrap_or(&[]);
            let dst_start = row * native_row;
            let Some(dst) = buffers.native.get_mut(dst_start..dst_start + native_row) else {
                break;
            };
            unpack_row(src, &bits, samples_per_row, sample_type, dst)?;
        }
        buffers.scratch.clear();
    }

    Ok(ChunkShape {
        coded_width,
        coded_height,
        origin_x,
        origin_y,
        valid_width,
        valid_height,
        plane,
        samples_per_pixel: if subsampled { 3 } else { chunk_spp },
        sample_type,
    })
}

/// Copies the valid part of a decoded chunk into a destination that covers
/// `rect` of the image.
///
/// Chunky chunks are copied row by row; planar chunks are scattered into their
/// channel slot. `image_spp` is the image's `SamplesPerPixel`, which for a
/// planar image differs from the chunk's. Only the intersection of the chunk's
/// valid area with `rect` is written, so the same routine serves whole-image
/// reads and [`crate::Decoder::read_region`].
///
/// # Errors
/// [`crate::UsageError::BufferTooSmall`] when the destination is too short.
pub fn place_chunk_in_rect(
    chunk: &[u8],
    shape: &ChunkShape,
    image: &mut [u8],
    rect: Rect,
    image_spp: u16,
    planar: PlanarConfiguration,
) -> Result<()> {
    let slot = shape.sample_type.byte_width();
    let image_row = (rect.width as usize)
        .checked_mul(usize::from(image_spp))
        .and_then(|n| n.checked_mul(slot))
        .ok_or(TiffError::IntOverflow)?;
    let need = image_row
        .checked_mul(rect.height as usize)
        .ok_or(TiffError::IntOverflow)?;
    if image.len() < need {
        return Err(TiffError::Usage(crate::error::UsageError::BufferTooSmall {
            needed: need,
            got: image.len(),
        }));
    }

    // Intersect the chunk's valid area with the destination rectangle.
    let chunk_x0 = shape.origin_x;
    let chunk_y0 = shape.origin_y;
    let chunk_x1 = chunk_x0.saturating_add(shape.valid_width);
    let chunk_y1 = chunk_y0.saturating_add(shape.valid_height);
    let rect_x1 = rect.x.saturating_add(rect.width);
    let rect_y1 = rect.y.saturating_add(rect.height);
    let x0 = chunk_x0.max(rect.x);
    let y0 = chunk_y0.max(rect.y);
    let x1 = chunk_x1.min(rect_x1);
    let y1 = chunk_y1.min(rect_y1);
    if x0 >= x1 || y0 >= y1 {
        return Ok(());
    }

    let chunk_row = shape.row_bytes();
    let planar_output = planar == PlanarConfiguration::Planar;
    let chunk_spp = usize::from(shape.samples_per_pixel);

    for image_y in y0..y1 {
        let src_y = (image_y - chunk_y0) as usize;
        let src_start = src_y * chunk_row;
        let Some(src_row) = chunk.get(src_start..src_start + chunk_row) else {
            break;
        };
        let dst_y = (image_y - rect.y) as usize;
        if planar_output {
            let plane = usize::from(shape.plane);
            for image_x in x0..x1 {
                let src_off = (image_x - chunk_x0) as usize * slot;
                let dst_off = dst_y * image_row
                    + ((image_x - rect.x) as usize * usize::from(image_spp) + plane) * slot;
                let (Some(src), Some(dst)) = (
                    src_row.get(src_off..src_off + slot),
                    image.get_mut(dst_off..dst_off + slot),
                ) else {
                    break;
                };
                dst.copy_from_slice(src);
            }
        } else {
            let src_off = (x0 - chunk_x0) as usize * chunk_spp * slot;
            let take = (x1 - x0) as usize * chunk_spp * slot;
            let dst_off =
                dst_y * image_row + (x0 - rect.x) as usize * usize::from(image_spp) * slot;
            let (Some(src), Some(dst)) = (
                src_row.get(src_off..src_off + take),
                image.get_mut(dst_off..dst_off + take),
            ) else {
                break;
            };
            dst.copy_from_slice(src);
        }
    }
    Ok(())
}

/// [`place_chunk_in_rect`] over the whole image.
///
/// # Errors
/// The same set as [`place_chunk_in_rect`].
pub fn place_chunk(
    chunk: &[u8],
    shape: &ChunkShape,
    image: &mut [u8],
    image_width: u32,
    image_height: u32,
    image_spp: u16,
    planar: PlanarConfiguration,
) -> Result<()> {
    place_chunk_in_rect(
        chunk,
        shape,
        image,
        Rect::new(0, 0, image_width, image_height),
        image_spp,
        planar,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shape(spp: u16, plane: u16) -> ChunkShape {
        ChunkShape {
            coded_width: 2,
            coded_height: 2,
            origin_x: 0,
            origin_y: 0,
            valid_width: 2,
            valid_height: 2,
            plane,
            samples_per_pixel: spp,
            sample_type: SampleType::U8,
        }
    }

    #[test]
    fn chunk_shape_row_geometry() {
        let s = shape(3, 0);
        assert_eq!(s.row_bytes(), 6);
    }

    #[test]
    fn chunky_placement_copies_whole_rows() {
        let chunk = [1u8, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12];
        let mut image = vec![0u8; 12];
        place_chunk(
            &chunk,
            &shape(3, 0),
            &mut image,
            2,
            2,
            3,
            PlanarConfiguration::Chunky,
        )
        .expect("place");
        assert_eq!(image, chunk);
    }

    #[test]
    fn planar_placement_scatters_into_one_channel() {
        let chunk = [1u8, 2, 3, 4];
        let mut image = vec![0u8; 12];
        place_chunk(
            &chunk,
            &shape(1, 1),
            &mut image,
            2,
            2,
            3,
            PlanarConfiguration::Planar,
        )
        .expect("place");
        assert_eq!(image, vec![0, 1, 0, 0, 2, 0, 0, 3, 0, 0, 4, 0]);
    }

    #[test]
    fn placement_clips_at_the_image_edge() {
        let chunk = [1u8, 2, 3, 4];
        let mut shape = shape(1, 0);
        shape.origin_x = 1;
        shape.valid_width = 1;
        let mut image = vec![0u8; 4];
        place_chunk(
            &chunk,
            &shape,
            &mut image,
            2,
            2,
            1,
            PlanarConfiguration::Chunky,
        )
        .expect("place");
        assert_eq!(image, vec![0, 1, 0, 3]);
    }

    #[test]
    fn placement_rejects_a_short_destination() {
        let chunk = [1u8; 12];
        let mut image = vec![0u8; 4];
        let err = place_chunk(
            &chunk,
            &shape(3, 0),
            &mut image,
            2,
            2,
            3,
            PlanarConfiguration::Chunky,
        )
        .expect_err("short image");
        assert!(matches!(
            err,
            TiffError::Usage(crate::error::UsageError::BufferTooSmall { .. })
        ));
    }

    #[test]
    fn buffers_report_their_capacity() {
        let mut buffers = ChunkBuffers::new();
        assert_eq!(buffers.capacity(), 0);
        assert!(buffers.native().is_empty());
        assert!(buffers.compressed().is_empty());
        buffers.packed.resize(16, 0);
        assert!(buffers.capacity() >= 16);
    }
}
