//! Per-page chunk writing: pack, predict, compress, stream, then emit the IFD.
//!
//! The encode pipeline is the exact inverse of the decode one:
//!
//! 1. **gather** — the coded chunk is extracted from the caller's image buffer
//!    (tiles zero-padded at the right and bottom edges);
//! 2. **pack** — native slots are packed into the on-disk bit layout;
//! 3. **byte order** — swapped from the host's into the file's;
//! 4. **predictor** — applied forward, in the file's byte order;
//! 5. **fill order** — bits reversed when `FillOrder` is 2;
//! 6. **compress** — through [`crate::compression::encode_with`];
//! 7. **stream** — the payload is written immediately and only its offset and
//!    length are retained, so memory stays O(chunk).

use std::io::{Seek, Write};

use crate::error::{Result, TiffError, UsageError};
use crate::ifd::Value;
use crate::sample::{
    SampleType, is_byte_aligned_depth, pack_row, packed_row_bytes, reverse_bits_in_place,
};
use crate::tags::{FillOrder, PlanarConfiguration, Predictor, Tag};

use super::directory::DirectoryWriter;
use super::{Encoder, ImageSpec, Layout};

/// Writes the chunks and the IFD of one page.
#[derive(Debug)]
pub struct ImageWriter<'a, W: Write + Seek> {
    encoder: &'a mut Encoder<W>,
    spec: ImageSpec,
    directory: DirectoryWriter,
    offsets: Vec<u64>,
    byte_counts: Vec<u64>,
    next_chunk: u64,
    row_buffer: Vec<u8>,
    buffered_rows: u32,
    packed: Vec<u8>,
    finished: bool,
}

impl<'a, W: Write + Seek> ImageWriter<'a, W> {
    pub(crate) fn new(encoder: &'a mut Encoder<W>, spec: ImageSpec) -> Result<Self> {
        let count = usize::try_from(spec.chunk_count()).map_err(|_| TiffError::IntOverflow)?;
        Ok(Self {
            encoder,
            spec,
            directory: DirectoryWriter::new(),
            offsets: Vec::with_capacity(count),
            byte_counts: Vec::with_capacity(count),
            next_chunk: 0,
            row_buffer: Vec::new(),
            buffered_rows: 0,
            packed: Vec::new(),
            finished: false,
        })
    }

    /// The specification this page is being written to.
    #[must_use]
    pub fn spec(&self) -> &ImageSpec {
        &self.spec
    }

    /// Total number of chunks this page needs.
    #[must_use]
    pub fn chunk_count(&self) -> u64 {
        self.spec.chunk_count()
    }

    /// How many chunks have been written so far.
    #[must_use]
    pub fn chunks_written(&self) -> u64 {
        self.next_chunk
    }

    /// Bytes the next [`Self::write_chunk`] call expects.
    ///
    /// # Errors
    /// [`TiffError::IntOverflow`] when the geometry does not fit.
    pub fn next_chunk_len(&self) -> Result<usize> {
        self.spec.chunk_native_len(self.next_chunk)
    }

    /// Adds or overrides one tag in this page's directory.
    ///
    /// Called after the built-in tags are computed, so a caller can override a
    /// derived value as well as add a new one.
    pub fn set_tag(&mut self, tag: u16, value: Value) {
        self.directory.set_raw(tag, value);
    }

    /// Writes one chunk of native-endian samples.
    ///
    /// `data` must be exactly [`Self::next_chunk_len`] bytes: the *coded* chunk
    /// including any tile padding.
    ///
    /// # Errors
    /// [`UsageError::BufferTooSmall`], [`UsageError::ChunkCount`] when more
    /// chunks are written than the layout needs, plus codec and I/O failures.
    pub fn write_chunk(&mut self, data: &[u8]) -> Result<()> {
        let index = self.next_chunk;
        if index >= self.spec.chunk_count() {
            return Err(TiffError::Usage(UsageError::ChunkCount {
                expected: self.spec.chunk_count(),
                got: index + 1,
            }));
        }
        let need = self.spec.chunk_native_len(index)?;
        if data.len() < need {
            return Err(TiffError::Usage(UsageError::BufferTooSmall {
                needed: need,
                got: data.len(),
            }));
        }
        let payload = self.encode_chunk(index, data.get(..need).unwrap_or(data))?;
        let offset = self.encoder.writer_mut().align_to(2)?;
        self.encoder.writer_mut().write_bytes(&payload)?;
        self.offsets.push(offset);
        self.byte_counts.push(payload.len() as u64);
        self.next_chunk += 1;
        Ok(())
    }

    /// Buffers full-width rows and flushes complete strips.
    ///
    /// Only defined for [`Layout::Strips`]; for a planar page the rows of each
    /// plane are supplied one plane after another, in chunk order.
    ///
    /// # Errors
    /// [`UsageError::InvalidSpec`] for a tiled page, plus every
    /// [`Self::write_chunk`] failure.
    pub fn write_rows(&mut self, rows: &[u8]) -> Result<()> {
        let Layout::Strips { rows_per_strip } = self.spec.layout else {
            return Err(TiffError::Usage(UsageError::InvalidSpec(
                "write_rows is only defined for strip layouts".to_string(),
            )));
        };
        let slot = self.spec.sample_type()?.byte_width();
        let row_len = (self.spec.width as usize)
            .checked_mul(usize::from(self.spec.chunk_samples_per_pixel()))
            .and_then(|n| n.checked_mul(slot))
            .ok_or(TiffError::IntOverflow)?;
        if row_len == 0 {
            return Ok(());
        }
        if rows.len() % row_len != 0 {
            return Err(TiffError::Usage(UsageError::BufferTooSmall {
                needed: rows.len().next_multiple_of(row_len),
                got: rows.len(),
            }));
        }
        for row in rows.chunks_exact(row_len) {
            self.row_buffer.extend_from_slice(row);
            self.buffered_rows += 1;
            let wanted = self
                .spec
                .chunk_coded_dimensions(self.next_chunk)
                .1
                .min(rows_per_strip);
            if self.buffered_rows >= wanted && wanted > 0 {
                let buffered = core::mem::take(&mut self.row_buffer);
                self.write_chunk(&buffered)?;
                self.row_buffer = buffered;
                self.row_buffer.clear();
                self.buffered_rows = 0;
            }
        }
        Ok(())
    }

    /// Writes the whole page from one interleaved native-endian buffer.
    ///
    /// # Errors
    /// [`UsageError::BufferTooSmall`] when the buffer is shorter than
    /// [`ImageSpec::image_native_len`], plus every [`Self::write_chunk`]
    /// failure.
    pub fn write_whole_image(&mut self, data: &[u8]) -> Result<()> {
        let need = self.spec.image_native_len()?;
        if data.len() < need {
            return Err(TiffError::Usage(UsageError::BufferTooSmall {
                needed: need,
                got: data.len(),
            }));
        }
        let mut chunk = Vec::new();
        for index in 0..self.spec.chunk_count() {
            self.gather_chunk(data, index, &mut chunk)?;
            self.write_chunk(&chunk)?;
        }
        Ok(())
    }

    /// Extracts the coded chunk `index` out of an interleaved image buffer.
    fn gather_chunk(&self, image: &[u8], index: u64, out: &mut Vec<u8>) -> Result<()> {
        let slot = self.spec.sample_type()?.byte_width();
        let (coded_w, coded_h) = self.spec.chunk_coded_dimensions(index);
        let (valid_w, valid_h) = self.spec.chunk_data_dimensions(index);
        let (origin_x, origin_y) = self.spec.chunk_origin(index);
        let plane = self.spec.chunk_plane(index);
        let chunk_spp = usize::from(self.spec.chunk_samples_per_pixel());
        let image_spp = usize::from(self.spec.samples_per_pixel);
        let image_row = (self.spec.width as usize) * image_spp * slot;
        let chunk_row = (coded_w as usize) * chunk_spp * slot;

        out.clear();
        out.resize(chunk_row * coded_h as usize, 0);
        let planar = self.spec.planar == PlanarConfiguration::Planar;

        for y in 0..valid_h as usize {
            let src_y = origin_y as usize + y;
            let dst_start = y * chunk_row;
            if planar {
                for x in 0..valid_w as usize {
                    let src_x = origin_x as usize + x;
                    let src_off =
                        src_y * image_row + (src_x * image_spp + usize::from(plane)) * slot;
                    let dst_off = dst_start + x * slot;
                    let (Some(src), Some(dst)) = (
                        image.get(src_off..src_off + slot),
                        out.get_mut(dst_off..dst_off + slot),
                    ) else {
                        continue;
                    };
                    dst.copy_from_slice(src);
                }
            } else {
                let src_off = src_y * image_row + origin_x as usize * image_spp * slot;
                let take = valid_w as usize * image_spp * slot;
                let (Some(src), Some(dst)) = (
                    image.get(src_off..src_off + take),
                    out.get_mut(dst_start..dst_start + take),
                ) else {
                    continue;
                };
                dst.copy_from_slice(src);
            }
        }
        Ok(())
    }

    /// Runs steps 2-6 of the encode pipeline.
    fn encode_chunk(&mut self, index: u64, native: &[u8]) -> Result<Vec<u8>> {
        let (coded_w, coded_h) = self.spec.chunk_coded_dimensions(index);
        let plane = self.spec.chunk_plane(index);
        let bits = self.spec.plane_bits(plane);
        let chunk_spp = self.spec.chunk_samples_per_pixel();
        let sample_type: SampleType = self.spec.sample_type()?;
        let slot = sample_type.byte_width();
        let packed_len = self.spec.chunk_packed_len(index)?;
        let endian = self.encoder.endian();

        self.packed.clear();
        self.packed.resize(packed_len, 0);

        let uniform = bits
            .first()
            .copied()
            .filter(|b| bits.iter().all(|x| x == b));
        let byte_aligned = uniform.map(is_byte_aligned_depth).unwrap_or(false);

        if self.spec.is_subsampled() {
            crate::colour::pack_ycbcr_subsampling(
                native,
                coded_w,
                coded_h,
                self.spec.ycbcr_subsampling.unwrap_or((2, 2)),
                slot,
                &mut self.packed,
            )?;
            endian.from_native_in_place(&mut self.packed, slot);
        } else if byte_aligned {
            let copy = packed_len.min(native.len());
            if let Some(dst) = self.packed.get_mut(..copy) {
                if let Some(src) = native.get(..copy) {
                    dst.copy_from_slice(src);
                }
            }
            endian.from_native_in_place(&mut self.packed, slot);
        } else {
            let samples_per_row = coded_w as usize * usize::from(chunk_spp);
            let row_bytes = packed_row_bytes(&bits, samples_per_row) as usize;
            let native_row = samples_per_row * slot;
            for row in 0..coded_h as usize {
                let src_start = row * native_row;
                let Some(src) = native.get(src_start..src_start + native_row) else {
                    break;
                };
                let dst_start = row * row_bytes;
                let Some(dst) = self.packed.get_mut(dst_start..dst_start + row_bytes) else {
                    break;
                };
                pack_row(src, &bits, samples_per_row, sample_type, dst)?;
            }
        }

        if self.spec.predictor != Predictor::None {
            let bytes_per_sample = usize::from(bits.first().copied().unwrap_or(8) / 8).max(1);
            let stride = if self.spec.planar == PlanarConfiguration::Planar {
                1
            } else {
                usize::from(chunk_spp)
            };
            crate::predictor::apply_predictor_forward(
                &mut self.packed,
                self.spec.predictor,
                bytes_per_sample,
                stride,
                coded_w as usize,
                endian,
            )?;
        }

        if self.spec.fill_order == FillOrder::Lsb2Msb && bits.iter().any(|b| *b < 8) {
            reverse_bits_in_place(&mut self.packed);
        }

        let cx = crate::compression::CodecContext {
            compression: self.spec.compression.method(),
            photometric: self.spec.photometric,
            fill_order: self.spec.fill_order,
            width: coded_w as usize,
            height: coded_h as usize,
            bits_per_sample: &bits,
            samples_per_pixel: chunk_spp,
            planar: self.spec.planar,
            plane,
            t4_options: crate::tags::T4Options::from_u32(self.spec.compression.t4_options()),
            t6_options: crate::tags::T6Options::default(),
            jpeg_tables: None,
            endian,
        };
        crate::compression::encode_with(
            &self.packed,
            &cx,
            self.spec.compression.level(),
            self.encoder.registry(),
        )
    }

    /// Emits the page's IFD and links it into the chain.
    ///
    /// # Errors
    /// [`UsageError::ChunkCount`] when not every chunk was written, plus I/O
    /// failures.
    pub fn finish(mut self) -> Result<()> {
        if !self.row_buffer.is_empty() {
            let buffered = core::mem::take(&mut self.row_buffer);
            self.write_chunk(&buffered)?;
        }
        let expected = self.spec.chunk_count();
        if self.next_chunk != expected {
            return Err(TiffError::Usage(UsageError::ChunkCount {
                expected,
                got: self.next_chunk,
            }));
        }
        self.populate_directory();
        let variant = self.encoder.resolved_variant();
        let written = {
            let writer = self.encoder.writer_mut();
            self.directory.write(writer, variant)?
        };
        self.encoder.link_directory(written)?;
        self.finished = true;
        Ok(())
    }

    /// Fills in the built-in tags, leaving any caller override in place.
    fn populate_directory(&mut self) {
        let big = self.encoder.resolved_variant().is_big();
        let spec = &self.spec;
        let dir = &mut self.directory;

        // Caller-supplied extra tags first, so the derived tags below can be
        // overridden with `set_tag` but a stale extra tag never wins over the
        // geometry.
        for (tag, value) in &spec.extra_tags {
            dir.set_default(crate::tags::Tag::from_u16(*tag), value.clone());
        }

        dir.set(Tag::ImageWidth, Value::Long(vec![spec.width]));
        dir.set(Tag::ImageLength, Value::Long(vec![spec.height]));
        dir.set(
            Tag::BitsPerSample,
            Value::Short(spec.bits_per_sample.clone()),
        );
        dir.set(
            Tag::Compression,
            Value::Short(vec![spec.compression.method().to_u16()]),
        );
        dir.set(
            Tag::PhotometricInterpretation,
            Value::Short(vec![spec.photometric.to_u16()]),
        );
        if spec.fill_order != FillOrder::Msb2Lsb {
            dir.set(Tag::FillOrder, Value::Short(vec![spec.fill_order.to_u16()]));
        }
        dir.set(
            Tag::SamplesPerPixel,
            Value::Short(vec![spec.samples_per_pixel]),
        );
        dir.set(
            Tag::PlanarConfiguration,
            Value::Short(vec![spec.planar.to_u16()]),
        );
        if spec.predictor != Predictor::None {
            dir.set(Tag::Predictor, Value::Short(vec![spec.predictor.to_u16()]));
        }
        dir.set(
            Tag::SampleFormat,
            Value::Short(spec.sample_format.iter().map(|f| f.to_u16()).collect()),
        );
        if !spec.extra_samples.is_empty() {
            dir.set(
                Tag::ExtraSamples,
                Value::Short(spec.extra_samples.iter().map(|e| e.to_u16()).collect()),
            );
        }
        if let Some(map) = &spec.color_map {
            dir.set(Tag::ColorMap, Value::Short(map.clone()));
        }
        if let Some((x, y, unit)) = spec.resolution {
            dir.set(Tag::XResolution, Value::Rational(vec![x]));
            dir.set(Tag::YResolution, Value::Rational(vec![y]));
            dir.set(Tag::ResolutionUnit, Value::Short(vec![unit.to_u16()]));
        }
        if spec.photometric == crate::tags::PhotometricInterpretation::YCbCr
            || spec.ycbcr_subsampling.is_some()
        {
            let (h, v) = spec.ycbcr_subsampling.unwrap_or((1, 1));
            dir.set(Tag::YCbCrSubSampling, Value::Short(vec![h, v]));
        }
        let t4 = spec.compression.t4_options();
        if t4 != 0 {
            dir.set(Tag::T4Options, Value::Long(vec![t4]));
        }

        let offsets = if big {
            Value::Long8(self.offsets.clone())
        } else {
            Value::Long(self.offsets.iter().map(|v| *v as u32).collect())
        };
        let counts = if big {
            Value::Long8(self.byte_counts.clone())
        } else {
            Value::Long(self.byte_counts.iter().map(|v| *v as u32).collect())
        };
        match spec.layout {
            Layout::Strips { rows_per_strip } => {
                dir.set(Tag::StripOffsets, offsets);
                dir.set(Tag::RowsPerStrip, Value::Long(vec![rows_per_strip]));
                dir.set(Tag::StripByteCounts, counts);
            }
            Layout::Tiles { width, length } => {
                dir.set(Tag::TileWidth, Value::Long(vec![width]));
                dir.set(Tag::TileLength, Value::Long(vec![length]));
                dir.set(Tag::TileOffsets, offsets);
                dir.set(Tag::TileByteCounts, counts);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image::ColorType;
    use std::io::Cursor;

    #[test]
    fn write_rows_rejects_a_tiled_page() {
        let spec = ImageSpec::new(32, 32, ColorType::Gray(8)).with_layout(Layout::Tiles {
            width: 16,
            length: 16,
        });
        let mut buffer = Cursor::new(Vec::new());
        let mut encoder = Encoder::new(&mut buffer).expect("encoder");
        let mut page = encoder.new_image(&spec).expect("page");
        assert!(page.write_rows(&[0u8; 32]).is_err());
    }

    #[test]
    fn writing_too_many_chunks_is_an_error() {
        let spec = ImageSpec::new(4, 2, ColorType::Gray(8))
            .with_layout(Layout::Strips { rows_per_strip: 2 });
        let mut buffer = Cursor::new(Vec::new());
        let mut encoder = Encoder::new(&mut buffer).expect("encoder");
        let mut page = encoder.new_image(&spec).expect("page");
        assert_eq!(page.chunk_count(), 1);
        assert_eq!(page.next_chunk_len().expect("len"), 8);
        page.write_chunk(&[0u8; 8]).expect("first chunk");
        assert_eq!(page.chunks_written(), 1);
        assert!(page.write_chunk(&[0u8; 8]).is_err());
    }

    #[test]
    fn finishing_early_is_an_error() {
        let spec = ImageSpec::new(4, 4, ColorType::Gray(8))
            .with_layout(Layout::Strips { rows_per_strip: 2 });
        let mut buffer = Cursor::new(Vec::new());
        let mut encoder = Encoder::new(&mut buffer).expect("encoder");
        let page = encoder.new_image(&spec).expect("page");
        assert_eq!(page.spec().height, 4);
        let err = page.finish().expect_err("no chunks written");
        assert!(matches!(
            err,
            TiffError::Usage(UsageError::ChunkCount { .. })
        ));
    }

    #[test]
    fn a_short_chunk_buffer_is_rejected() {
        let spec = ImageSpec::new(4, 2, ColorType::Gray(8))
            .with_layout(Layout::Strips { rows_per_strip: 2 });
        let mut buffer = Cursor::new(Vec::new());
        let mut encoder = Encoder::new(&mut buffer).expect("encoder");
        let mut page = encoder.new_image(&spec).expect("page");
        assert!(page.write_chunk(&[0u8; 4]).is_err());
    }
}
