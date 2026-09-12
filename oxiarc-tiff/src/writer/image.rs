//! Per-page chunk writing: pack, predict, compress, stream, then emit the IFD.
//!
//! The encode pipeline is the exact inverse of the decode one:
//!
//! 1. **gather** — the coded chunk is extracted from the caller's image buffer
//!    (tiles zero-padded at the right and bottom edges);
//! 2. **pack** — native slots are packed into the on-disk bit layout;
//! 3. **byte order** — swapped from the host's into the file's;
//! 4. **predictor** — applied forward, in the file's byte order;
//! 5. **fill order** — the bits of every byte of the *compressed* chunk are
//!    reversed when `FillOrder` is 2, at every bit depth, matching libtiff's
//!    `TIFFFlushData1`;
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
    /// The `JPEGTables` (347) blob shared by every chunk of a JPEG page.
    jpeg_tables: Option<Vec<u8>>,
    /// Per-page codec state, so the encoders can hold scratch across chunks.
    codec_state: crate::compression::CodecState,
    finished: bool,
}

impl<'a, W: Write + Seek> ImageWriter<'a, W> {
    pub(crate) fn new(encoder: &'a mut Encoder<W>, spec: ImageSpec) -> Result<Self> {
        let count = usize::try_from(spec.chunk_count()).map_err(|_| TiffError::IntOverflow)?;
        // The chunk count comes from the caller's geometry and can be
        // astronomically larger than any file that will actually be written
        // (a 2^32 x 2^32 page in 16x16 tiles is 2^56 chunks), so the
        // pre-reservation is capped; the vectors still grow as chunks arrive.
        let reserve = count.min(4096);
        Ok(Self {
            encoder,
            spec,
            directory: DirectoryWriter::new(),
            offsets: Vec::with_capacity(reserve),
            byte_counts: Vec::with_capacity(reserve),
            next_chunk: 0,
            row_buffer: Vec::new(),
            buffered_rows: 0,
            packed: Vec::new(),
            jpeg_tables: None,
            codec_state: crate::compression::CodecState::new(),
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
        self.write_encoded_chunk(&payload)
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
        gather_chunk_into(&self.spec, image, index, out)
    }

    /// Runs steps 2-6 of the encode pipeline.
    fn encode_chunk(&mut self, index: u64, native: &[u8]) -> Result<Vec<u8>> {
        let endian = self.encoder.endian();
        if self.jpeg_tables.is_none() {
            self.jpeg_tables = build_shared_jpeg_tables_tag(&self.spec, endian)?;
        }
        encode_chunk_pure(
            &self.spec,
            endian,
            index,
            native,
            self.jpeg_tables.as_deref(),
            &self.codec_state,
            self.encoder.registry(),
            &mut self.packed,
        )
    }

    /// Sets the page-shared `JPEGTables` (347) blob directly, bypassing the
    /// lazy first-chunk build [`Self::encode_chunk`] otherwise does.
    ///
    /// For the `rayon` parallel driver, which must resolve the shared table
    /// set once, up front (every worker needs it, and only one of them can
    /// build it), then hand the writer the same blob it embedded into every
    /// chunk's abbreviated datastream -- [`Self::finish`] writes tag 347 from
    /// this field, so it has to be set even though no chunk went through
    /// [`Self::encode_chunk`] to set it as a side effect.
    #[cfg(feature = "rayon")]
    pub(crate) fn set_jpeg_tables(&mut self, tables: Option<Vec<u8>>) {
        self.jpeg_tables = tables;
    }

    /// Streams one chunk's already-encoded payload and records its offset
    /// and length, bypassing steps 1-6 of the encode pipeline
    /// (gather/pack/predictor/compress).
    ///
    /// [`Self::write_chunk`] calls this for its own tail; the `rayon`
    /// parallel driver calls it directly after computing every chunk's
    /// payload off the writer (in parallel, via [`encode_chunk_pure`]), so
    /// this is the one place a payload actually reaches the sink either way.
    ///
    /// # Errors
    /// [`UsageError::ChunkCount`] when more chunks are written than the
    /// layout needs, plus I/O failures.
    pub(crate) fn write_encoded_chunk(&mut self, payload: &[u8]) -> Result<()> {
        let index = self.next_chunk;
        if index >= self.spec.chunk_count() {
            return Err(TiffError::Usage(UsageError::ChunkCount {
                expected: self.spec.chunk_count(),
                got: index + 1,
            }));
        }
        let offset = self.encoder.writer_mut().align_to(2)?;
        self.encoder.writer_mut().write_bytes(payload)?;
        self.offsets.push(offset);
        self.byte_counts.push(payload.len() as u64);
        self.next_chunk += 1;
        Ok(())
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
        self.populate_directory()?;
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
    ///
    /// # Errors
    /// [`UsageError::ClassicTiffOverflow`] when a chunk offset or byte count
    /// does not fit the classic container's 32-bit fields.
    fn populate_directory(&mut self) -> Result<()> {
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
        if let Some(tables) = &self.jpeg_tables {
            dir.set(Tag::JpegTables, Value::Undefined(tables.clone()));
        }
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
        // `T4Options` bit 1 and `T6Options` bit 1 both mean "this file may
        // contain T.4 uncompressed mode"; each belongs to its own codec, so
        // the flag lands in exactly one of the two tags.
        let uncompressed = if spec.ccitt_uncompressed {
            crate::tags::T4Options::UNCOMPRESSED
        } else {
            0
        };
        match spec.compression.method() {
            crate::tags::CompressionMethod::CcittFax3 => {
                let t4 = spec.compression.t4_options() | uncompressed;
                if t4 != 0 {
                    dir.set(Tag::T4Options, Value::Long(vec![t4]));
                }
            }
            crate::tags::CompressionMethod::CcittFax4 if uncompressed != 0 => {
                dir.set(Tag::T6Options, Value::Long(vec![uncompressed]));
            }
            _ => {}
        }

        let offsets = chunk_table(&self.offsets, big)?;
        let counts = chunk_table(&self.byte_counts, big)?;
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
        Ok(())
    }
}

/// Packs a `StripOffsets` / `StripByteCounts`-shaped array into the value type
/// the container can address.
///
/// BigTIFF gets `LONG8`; classic TIFF gets `LONG`, and a value that does not
/// fit 32 bits is an **error** rather than a silent truncation — a classic file
/// whose pixel data crosses 4 GiB would otherwise be written with a wrapped
/// offset table and `Ok(())` returned.
///
/// # Errors
/// [`UsageError::ClassicTiffOverflow`] naming the first value that overflowed.
fn chunk_table(values: &[u64], big: bool) -> Result<Value> {
    if big {
        return Ok(Value::Long8(values.to_vec()));
    }
    let mut narrow = Vec::with_capacity(values.len());
    for value in values {
        narrow.push(
            u32::try_from(*value).map_err(|_| {
                TiffError::Usage(UsageError::ClassicTiffOverflow { offset: *value })
            })?,
        );
    }
    Ok(Value::Long(narrow))
}

/// Extracts the coded chunk `index` out of an interleaved image buffer.
///
/// The free-function form of [`ImageWriter::gather_chunk`] (which delegates
/// here), taking `&ImageSpec` instead of `&self` so the `rayon` parallel
/// driver can call it with nothing shared but the (read-only, `Sync`) spec
/// and the (read-only) source image.
///
/// # Errors
/// The same set as [`ImageSpec::sample_type`].
pub(crate) fn gather_chunk_into(
    spec: &ImageSpec,
    image: &[u8],
    index: u64,
    out: &mut Vec<u8>,
) -> Result<()> {
    let slot = spec.sample_type()?.byte_width();
    let (coded_w, coded_h) = spec.chunk_coded_dimensions(index);
    let (valid_w, valid_h) = spec.chunk_data_dimensions(index);
    let (origin_x, origin_y) = spec.chunk_origin(index);
    let plane = spec.chunk_plane(index);
    let chunk_spp = usize::from(spec.chunk_samples_per_pixel());
    let image_spp = usize::from(spec.samples_per_pixel);
    let image_row = (spec.width as usize) * image_spp * slot;
    let chunk_row = (coded_w as usize) * chunk_spp * slot;

    out.clear();
    out.resize(chunk_row * coded_h as usize, 0);
    let planar = spec.planar == PlanarConfiguration::Planar;

    for y in 0..valid_h as usize {
        let src_y = origin_y as usize + y;
        let dst_start = y * chunk_row;
        if planar {
            for x in 0..valid_w as usize {
                let src_x = origin_x as usize + x;
                let src_off = src_y * image_row + (src_x * image_spp + usize::from(plane)) * slot;
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

/// Builds the page-shared `JPEGTables` (347) blob from chunk 0's geometry,
/// when `spec` asks for one; `Ok(None)` for every other page.
///
/// Factored out of the lazy, per-chunk build [`ImageWriter::encode_chunk`]
/// used to do inline, so the `rayon` parallel driver can resolve it once,
/// synchronously, before any chunk's encode work is spread across workers
/// (only one table set may ever be built for a page, and every worker needs
/// the *same* one).
///
/// # Errors
/// Whatever the JPEG encoder's table-set construction can produce.
pub(crate) fn build_shared_jpeg_tables_tag(
    spec: &ImageSpec,
    endian: crate::byteorder::Endian,
) -> Result<Option<Vec<u8>>> {
    #[cfg(feature = "jpeg")]
    {
        if spec.compression.method() != crate::tags::CompressionMethod::Jpeg {
            return Ok(None);
        }
        let crate::writer::Compression::Jpeg {
            shared_tables: true,
            ..
        } = spec.compression
        else {
            return Ok(None);
        };
        // One table set for the whole page: tag 347 carries it and every
        // chunk is an abbreviated datastream, exactly as `tiffcp -c jpeg`
        // writes it. Building it from chunk 0's context guarantees the tag
        // and the chunks agree.
        let (coded_w, coded_h) = spec.chunk_coded_dimensions(0);
        let plane = spec.chunk_plane(0);
        let bits = spec.plane_bits(plane);
        let chunk_spp = spec.chunk_samples_per_pixel();
        let probe = crate::compression::CodecContext {
            compression: crate::tags::CompressionMethod::Jpeg,
            photometric: spec.photometric,
            fill_order: spec.fill_order,
            width: coded_w as usize,
            height: coded_h as usize,
            bits_per_sample: &bits,
            samples_per_pixel: chunk_spp,
            planar: spec.planar,
            plane,
            t4_options: crate::tags::T4Options::default(),
            t6_options: crate::tags::T6Options::default(),
            ycbcr_subsampling: if spec.photometric == crate::tags::PhotometricInterpretation::YCbCr
            {
                spec.ycbcr_subsampling.unwrap_or((2, 2))
            } else {
                (1, 1)
            },
            jpeg_restart_rows: spec.jpeg_restart_rows,
            jpeg_tables: None,
            old_jpeg: None,
            endian,
            leniency: crate::limits::Leniency::Normal,
            state: None,
            max_scratch_bytes: crate::limits::Limits::default().intermediate_buffer_size,
        };
        Ok(Some(crate::compression::jpeg::shared_tables(
            &probe,
            spec.compression.level(),
        )?))
    }
    #[cfg(not(feature = "jpeg"))]
    {
        let _ = (spec, endian);
        Ok(None)
    }
}

/// Runs steps 2-6 of the encode pipeline (pack, byte order, predictor,
/// compress, fill order) for one chunk.
///
/// The free-function form of [`ImageWriter::encode_chunk`] (which delegates
/// here): everything it needs is a parameter rather than a `self` field, so
/// the `rayon` parallel driver can call it from any number of workers at
/// once, each with its own `packed` scratch buffer, sharing only read-only
/// state (`spec`, the resolved `jpeg_tables`, and `codec_state`, which is
/// `Send + Sync` -- see [`crate::compression::CodecState`]'s docs for which
/// codecs serialise on it).
///
/// # Errors
/// Every failure the pack/predictor/compress steps can produce.
#[allow(clippy::too_many_arguments)]
pub(crate) fn encode_chunk_pure(
    spec: &ImageSpec,
    endian: crate::byteorder::Endian,
    index: u64,
    native: &[u8],
    jpeg_tables: Option<&[u8]>,
    codec_state: &crate::compression::CodecState,
    registry: Option<&crate::compression::CodecRegistry>,
    packed: &mut Vec<u8>,
) -> Result<Vec<u8>> {
    let (coded_w, coded_h) = spec.chunk_coded_dimensions(index);
    let plane = spec.chunk_plane(index);
    let bits = spec.plane_bits(plane);
    let chunk_spp = spec.chunk_samples_per_pixel();
    let sample_type: SampleType = spec.sample_type()?;
    let slot = sample_type.byte_width();
    let method = spec.compression.method();
    // A JPEG encoder downsamples chroma itself, from full-resolution input,
    // so the buffer handed to it is full-resolution too.
    let codec_expands = spec.is_subsampled() && crate::compression::expands_subsampling(method);
    let packed_len = if codec_expands {
        (coded_w as usize)
            .checked_mul(coded_h as usize)
            .and_then(|n| n.checked_mul(3))
            .and_then(|n| n.checked_mul(slot))
            .ok_or(TiffError::IntOverflow)?
    } else {
        spec.chunk_packed_len(index)?
    };

    packed.clear();
    packed.resize(packed_len, 0);

    let uniform = bits
        .first()
        .copied()
        .filter(|b| bits.iter().all(|x| x == b));
    let byte_aligned = uniform.map(is_byte_aligned_depth).unwrap_or(false);

    if spec.is_subsampled() && !codec_expands {
        crate::colour::pack_ycbcr_subsampling(
            native,
            coded_w,
            coded_h,
            spec.ycbcr_subsampling.unwrap_or((2, 2)),
            slot,
            packed,
        )?;
        endian.from_native_in_place(packed, slot);
    } else if byte_aligned {
        let copy = packed_len.min(native.len());
        if let Some(dst) = packed.get_mut(..copy) {
            if let Some(src) = native.get(..copy) {
                dst.copy_from_slice(src);
            }
        }
        endian.from_native_in_place(packed, slot);
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
            let Some(dst) = packed.get_mut(dst_start..dst_start + row_bytes) else {
                break;
            };
            pack_row(src, &bits, samples_per_row, sample_type, dst)?;
        }
    }

    if spec.predictor != Predictor::None {
        let bytes_per_sample = usize::from(bits.first().copied().unwrap_or(8) / 8).max(1);
        let stride = if spec.planar == PlanarConfiguration::Planar {
            1
        } else {
            usize::from(chunk_spp)
        };
        crate::predictor::apply_predictor_forward(
            packed,
            spec.predictor,
            bytes_per_sample,
            stride,
            coded_w as usize,
            endian,
        )?;
    }

    let cx = crate::compression::CodecContext {
        compression: method,
        photometric: spec.photometric,
        fill_order: spec.fill_order,
        width: coded_w as usize,
        height: coded_h as usize,
        bits_per_sample: &bits,
        samples_per_pixel: chunk_spp,
        planar: spec.planar,
        plane,
        t4_options: crate::tags::T4Options::from_u32(if spec.ccitt_uncompressed {
            spec.compression.t4_options() | crate::tags::T4Options::UNCOMPRESSED
        } else {
            spec.compression.t4_options()
        }),
        t6_options: crate::tags::T6Options::from_u32(if spec.ccitt_uncompressed {
            crate::tags::T6Options::UNCOMPRESSED
        } else {
            0
        }),
        ycbcr_subsampling: if spec.photometric == crate::tags::PhotometricInterpretation::YCbCr {
            spec.ycbcr_subsampling.unwrap_or((2, 2))
        } else {
            (1, 1)
        },
        jpeg_restart_rows: spec.jpeg_restart_rows,
        jpeg_tables,
        old_jpeg: None,
        endian,
        leniency: crate::limits::Leniency::Normal,
        state: Some(codec_state),
        max_scratch_bytes: crate::limits::Limits::default().intermediate_buffer_size,
    };
    let mut encoded =
        crate::compression::encode_with(packed, &cx, spec.compression.level(), registry)?;
    // `FillOrder` 2 reverses the bits of every byte of the *compressed*
    // chunk, at every bit depth, exactly where libtiff does it
    // (`TIFFFlushData1`). The CCITT codecs apply the tag themselves.
    if spec.fill_order == FillOrder::Lsb2Msb
        && !crate::compression::handles_fill_order(spec.compression.method())
    {
        reverse_bits_in_place(&mut encoded);
    }
    Ok(encoded)
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

    #[test]
    fn a_chunk_table_past_four_gib_is_an_error_in_a_classic_file() {
        // BigTIFF addresses it.
        let big = chunk_table(&[8, 0x1_0000_0000, u64::MAX], true).expect("bigtiff");
        assert_eq!(
            big,
            Value::Long8(vec![8, 0x1_0000_0000, u64::MAX]),
            "BigTIFF must carry the offsets verbatim"
        );

        // Classic TIFF must refuse, never wrap: `0x1_0000_0000 as u32` is 0,
        // which would produce a file whose strips point at the header.
        let err = chunk_table(&[8, 0x1_0000_0000], false).expect_err("classic overflow");
        match err {
            TiffError::Usage(UsageError::ClassicTiffOverflow { offset }) => {
                assert_eq!(offset, 0x1_0000_0000);
            }
            other => panic!("expected ClassicTiffOverflow, got {other}"),
        }

        // Everything that fits is still packed as LONG.
        let ok = chunk_table(&[8, u64::from(u32::MAX)], false).expect("classic");
        assert_eq!(ok, Value::Long(vec![8, u32::MAX]));
    }

    /// A sink that only tracks its stream position, so a 4 GiB file costs no
    /// memory and no I/O.
    #[derive(Debug, Default)]
    struct CountingSink {
        position: u64,
    }

    impl std::io::Write for CountingSink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.position = self.position.saturating_add(buf.len() as u64);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl std::io::Seek for CountingSink {
        fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
            self.position = match pos {
                std::io::SeekFrom::Start(n) => n,
                std::io::SeekFrom::Current(n) | std::io::SeekFrom::End(n) => {
                    self.position.saturating_add_signed(n)
                }
            };
            Ok(self.position)
        }
    }

    #[test]
    fn a_classic_page_whose_strips_cross_four_gib_fails_instead_of_wrapping() {
        let spec = ImageSpec::new(4, 2, ColorType::Gray(8))
            .with_layout(Layout::Strips { rows_per_strip: 1 });
        let mut encoder = Encoder::new(CountingSink::default()).expect("encoder");
        let mut page = encoder.new_image(&spec).expect("page");
        page.write_chunk(&[1u8; 4]).expect("first strip");
        // Push the write position past 4 GiB. The sink discards, so this is
        // 4096 no-op calls, not 4 GiB of memory.
        let filler = vec![0u8; 1 << 20];
        for _ in 0..4096 {
            page.encoder
                .writer_mut()
                .write_bytes(&filler)
                .expect("filler");
        }
        page.write_chunk(&[2u8; 4]).expect("second strip");
        let err = page.finish().expect_err("offset table cannot be narrowed");
        assert!(
            matches!(
                err,
                TiffError::Usage(UsageError::ClassicTiffOverflow { .. })
            ),
            "expected ClassicTiffOverflow, got {err}"
        );
    }
}
