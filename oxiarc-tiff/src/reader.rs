//! The native decoder API.
//!
//! [`Decoder`] wraps any `Read + Seek` source. It parses lazily: the
//! constructor reads only the 8- or 16-byte header, so the [`Limits`] and
//! [`Leniency`] builders take effect before any number from the file is used to
//! size an allocation.
//!
//! ```
//! use oxiarc_tiff::{ColorType, Decoder, Encoder, ImageSpec};
//! use std::io::Cursor;
//!
//! # let bytes = {
//! #     let mut buffer = Cursor::new(Vec::new());
//! #     let mut encoder = Encoder::new(&mut buffer).expect("encoder");
//! #     encoder
//! #         .write_image(&ImageSpec::new(3, 2, ColorType::Gray(8)), &[1u8, 2, 3, 4, 5, 6])
//! #         .expect("write");
//! #     encoder.finish().expect("finish");
//! #     buffer.into_inner()
//! # };
//! let mut decoder = Decoder::new(Cursor::new(bytes))?;
//! assert_eq!(decoder.dimensions()?, (3, 2));
//! assert_eq!(decoder.image_count()?, 1);
//! let region = decoder.read_region(1, 0, 2, 2)?;
//! assert_eq!(region.as_u8(), Some(&[2u8, 3, 5, 6][..]));
//! # Ok::<(), oxiarc_tiff::TiffError>(())
//! ```

use std::io::{Read, Seek};

use crate::byteorder::{Endian, EndianReader};
use crate::compression::CodecRegistry;
use crate::decode::{ChunkBuffers, decode_chunk, strip, tile};
use crate::error::{FormatError, Result, TiffError, UsageError};
use crate::header::{Header, Variant};
use crate::ifd::{Directory, Entry, IfdPointer, IfdWalker, Value, ValueSource};
use crate::image::{ChunkType, ColorType, ImageInfo, ImageLayout, Rect};
use crate::limits::{Leniency, Limits, OutputBudget, Warning, Warnings};
use crate::sample::{SampleType, Samples};
use crate::tags::Tag;

/// Loads out-of-line tag values from the file.
struct Loader<'a, R> {
    reader: &'a mut EndianReader<R>,
    limits: &'a Limits,
}

impl<R: Read + Seek> ValueSource for Loader<'_, R> {
    fn load(&mut self, entry: &Entry) -> Result<Value> {
        load_value(self.reader, self.limits, entry)
    }

    fn read_raw(&mut self, offset: u64, len: u64) -> Result<Option<Vec<u8>>> {
        if len == 0 || !self.reader.range_in_bounds(offset, len) {
            return Ok(None);
        }
        let len = self.limits.check_value_size(0, len)?;
        let mut bytes = vec![0u8; len];
        self.reader.read_exact_at(offset, &mut bytes)?;
        Ok(Some(bytes))
    }
}

/// Materialises one entry's value, inline or out of line.
fn load_value<R: Read + Seek>(
    reader: &mut EndianReader<R>,
    limits: &Limits,
    entry: &Entry,
) -> Result<Value> {
    let endian = reader.endian();
    if let Some(value) = entry.inline_value(endian)? {
        return Ok(value);
    }
    let bytes = entry.value_bytes()?;
    let len = limits.check_value_size(entry.tag, bytes)?;
    let offset = entry.offset(endian)?.unwrap_or(0);
    if !reader.range_in_bounds(offset, bytes) {
        return Err(TiffError::Format(FormatError::ValueOffsetOutOfBounds {
            tag: entry.tag,
            offset,
            len: bytes,
            file_len: reader.len(),
        }));
    }
    let mut buf = vec![0u8; len];
    reader.read_exact_at(offset, &mut buf)?;
    let count = usize::try_from(entry.count).map_err(|_| TiffError::IntOverflow)?;
    Ok(Value::decode(entry.ty_raw, count, &buf, endian))
}

/// The six GeoTIFF tags, carried through verbatim.
///
/// This crate never interprets the geo keys; it guarantees only that they
/// survive a read/write round-trip byte-identically.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct GeoTags {
    /// Tag 33550, three `DOUBLE`s.
    pub model_pixel_scale: Option<Value>,
    /// Tag 33922, six `DOUBLE`s per tie point.
    pub model_tiepoint: Option<Value>,
    /// Tag 34264, sixteen `DOUBLE`s.
    pub model_transformation: Option<Value>,
    /// Tag 34735, four `SHORT`s per key.
    pub geo_key_directory: Option<Value>,
    /// Tag 34736, `DOUBLE` parameters.
    pub geo_double_params: Option<Value>,
    /// Tag 34737, pipe-separated `ASCII` parameters.
    pub geo_ascii_params: Option<Value>,
}

impl GeoTags {
    /// `true` when the image carries no GeoTIFF tags at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.model_pixel_scale.is_none()
            && self.model_tiepoint.is_none()
            && self.model_transformation.is_none()
            && self.geo_key_directory.is_none()
            && self.geo_double_params.is_none()
            && self.geo_ascii_params.is_none()
    }

    /// The tags as `(tag number, value)` pairs, ready for
    /// [`crate::ImageSpec::with_extra_tag`].
    #[must_use]
    pub fn to_extra_tags(&self) -> Vec<(u16, Value)> {
        let mut out = Vec::new();
        let pairs: [(Tag, &Option<Value>); 6] = [
            (Tag::ModelPixelScale, &self.model_pixel_scale),
            (Tag::ModelTiepoint, &self.model_tiepoint),
            (Tag::ModelTransformation, &self.model_transformation),
            (Tag::GeoKeyDirectory, &self.geo_key_directory),
            (Tag::GeoDoubleParams, &self.geo_double_params),
            (Tag::GeoAsciiParams, &self.geo_ascii_params),
        ];
        for (tag, value) in pairs {
            if let Some(value) = value {
                out.push((tag.to_u16(), value.clone()));
            }
        }
        out
    }
}

/// One node of a SubIFD tree.
#[derive(Clone, Debug)]
pub struct SubIfdNode {
    /// Where the child IFD lives.
    pub pointer: IfdPointer,
    /// The child directory itself.
    pub directory: Directory,
    /// Its own children, up to `Limits::max_ifd_depth` levels deep.
    pub children: Vec<SubIfdNode>,
}

/// A TIFF decoder over any `Read + Seek` source.
#[derive(Debug)]
pub struct Decoder<R: Read + Seek> {
    reader: EndianReader<R>,
    header: Header,
    limits: Limits,
    leniency: Leniency,
    registry: Option<CodecRegistry>,
    warnings: Warnings,
    offsets: Vec<u64>,
    chain_complete: bool,
    current: usize,
    directory: Option<Directory>,
    info: Option<ImageInfo>,
    budget: OutputBudget,
    buffers: ChunkBuffers,
}

impl<R: Read + Seek> Decoder<R> {
    /// Reads the file header and prepares to parse IFD 0 on demand.
    ///
    /// # Errors
    /// [`FormatError::SignatureNotFound`], [`FormatError::UnknownVersion`],
    /// [`FormatError::InvalidBigTiffHeader`] and I/O failures.
    pub fn new(mut reader: R) -> Result<Self> {
        use std::io::SeekFrom;
        reader.seek(SeekFrom::Start(0))?;
        let header = Header::read_from(&mut reader)?;
        let endian_reader = EndianReader::new(reader, header.endian)?;
        let limits = Limits::default();
        let budget = OutputBudget::new(limits.max_image_bytes as u64);
        Ok(Self {
            reader: endian_reader,
            header,
            limits,
            leniency: Leniency::default(),
            registry: None,
            warnings: Warnings::new(),
            offsets: if header.first_ifd == 0 {
                Vec::new()
            } else {
                vec![header.first_ifd]
            },
            chain_complete: header.first_ifd == 0,
            current: 0,
            directory: None,
            info: None,
            budget,
            buffers: ChunkBuffers::new(),
        })
    }

    /// Replaces the allocation guards.
    #[must_use]
    pub fn with_limits(mut self, limits: Limits) -> Self {
        self.budget = OutputBudget::new(limits.max_image_bytes as u64);
        self.limits = limits;
        self.directory = None;
        self.info = None;
        self
    }

    /// Chooses how strictly spec violations are treated.
    #[must_use]
    pub fn with_leniency(mut self, leniency: Leniency) -> Self {
        self.leniency = leniency;
        self.directory = None;
        self.info = None;
        self
    }

    /// Registers out-of-tree codecs.
    #[must_use]
    pub fn with_codecs(mut self, registry: CodecRegistry) -> Self {
        self.registry = Some(registry);
        self
    }

    /// The file's byte order.
    #[must_use]
    pub fn endian(&self) -> Endian {
        self.header.endian
    }

    /// Classic TIFF or BigTIFF.
    #[must_use]
    pub fn variant(&self) -> Variant {
        self.header.variant
    }

    /// The parsed file header.
    #[must_use]
    pub fn header(&self) -> Header {
        self.header
    }

    /// Recoverable spec violations seen so far.
    #[must_use]
    pub fn warnings(&self) -> &[Warning] {
        self.warnings.as_slice()
    }

    /// The zero-based index of the image the decoder is positioned on.
    #[must_use]
    pub fn current_image(&self) -> usize {
        self.current
    }

    /// Where the current IFD lives.
    #[must_use]
    pub fn ifd_pointer(&self) -> Option<IfdPointer> {
        self.offsets.get(self.current).copied().map(IfdPointer)
    }

    /// Walks the whole IFD chain and returns the number of images.
    ///
    /// # Errors
    /// [`FormatError::IfdCycle`] for a looping chain and
    /// [`crate::LimitError::IfdCount`] when the chain is too long.
    pub fn image_count(&mut self) -> Result<usize> {
        self.walk_chain()?;
        Ok(self.offsets.len())
    }

    /// `true` when another image follows the current one.
    ///
    /// # Errors
    /// The same set as [`Self::image_count`].
    pub fn more_images(&mut self) -> Result<bool> {
        self.ensure_index(self.current + 1)?;
        Ok(self.current + 1 < self.offsets.len())
    }

    /// Advances to the next image, returning `false` when there is none.
    ///
    /// # Errors
    /// The same set as [`Self::image_count`].
    pub fn next_image(&mut self) -> Result<bool> {
        if !self.more_images()? {
            return Ok(false);
        }
        self.seek_to_image(self.current + 1)?;
        Ok(true)
    }

    /// Positions the decoder on image `index`.
    ///
    /// # Errors
    /// [`UsageError::ImageIndexOutOfRange`] plus the chain-walk failures.
    pub fn seek_to_image(&mut self, index: usize) -> Result<()> {
        self.ensure_index(index)?;
        if index >= self.offsets.len() {
            return Err(TiffError::Usage(UsageError::ImageIndexOutOfRange {
                index,
                count: self.offsets.len(),
            }));
        }
        if index != self.current {
            self.current = index;
            self.directory = None;
            self.info = None;
            self.budget.reset();
        }
        Ok(())
    }

    /// Extends the known chain until `index` is reachable or the chain ends.
    fn ensure_index(&mut self, index: usize) -> Result<()> {
        while !self.chain_complete && self.offsets.len() <= index {
            self.extend_chain()?;
        }
        Ok(())
    }

    /// Walks the chain to its end.
    fn walk_chain(&mut self) -> Result<()> {
        while !self.chain_complete {
            self.extend_chain()?;
        }
        Ok(())
    }

    /// Reads one more IFD header to learn the next offset.
    fn extend_chain(&mut self) -> Result<()> {
        let Some(last) = self.offsets.last().copied() else {
            self.chain_complete = true;
            return Ok(());
        };
        if last == 0 {
            self.offsets.pop();
            self.chain_complete = true;
            return Ok(());
        }
        let mut walker = IfdWalker::new();
        for offset in &self.offsets {
            walker.visit(*offset, &self.limits)?;
        }
        let dir = Directory::read(
            &mut self.reader,
            last,
            self.header.variant,
            &self.limits,
            &mut self.warnings,
        )?;
        match dir.next_ifd() {
            Some(next) => {
                if walker.seen(next.get()) {
                    return Err(TiffError::Format(FormatError::IfdCycle {
                        offset: next.get(),
                    }));
                }
                if self.offsets.len() >= self.limits.max_ifds {
                    return Err(TiffError::Limits(crate::error::LimitError::IfdCount {
                        limit: self.limits.max_ifds,
                    }));
                }
                self.offsets.push(next.get());
            }
            None => self.chain_complete = true,
        }
        Ok(())
    }

    /// Parses the current directory and geometry if that has not happened yet.
    fn ensure_image(&mut self) -> Result<()> {
        if self.info.is_some() {
            return Ok(());
        }
        let count = self.offsets.len();
        let offset = self
            .offsets
            .get(self.current)
            .copied()
            .ok_or(TiffError::Usage(UsageError::ImageIndexOutOfRange {
                index: self.current,
                count,
            }))?;
        if offset == 0 {
            return Err(TiffError::Format(FormatError::RequiredTagNotFound(
                Tag::ImageWidth,
            )));
        }
        let directory = Directory::read(
            &mut self.reader,
            offset,
            self.header.variant,
            &self.limits,
            &mut self.warnings,
        )?;
        let file_len = self.reader.len();
        let info = {
            let mut loader = Loader {
                reader: &mut self.reader,
                limits: &self.limits,
            };
            ImageInfo::from_directory(
                &directory,
                &mut loader,
                &self.limits,
                self.leniency,
                &mut self.warnings,
                file_len,
                self.header.endian,
            )?
        };
        self.directory = Some(directory);
        self.info = Some(info);
        Ok(())
    }

    /// The geometry and colour model of the current image.
    ///
    /// # Errors
    /// Every failure [`ImageInfo::from_directory`] can produce.
    pub fn info(&mut self) -> Result<&ImageInfo> {
        self.ensure_image()?;
        self.info
            .as_ref()
            .ok_or(TiffError::Format(FormatError::RequiredTagNotFound(
                Tag::ImageWidth,
            )))
    }

    /// The raw directory of the current image.
    ///
    /// # Errors
    /// The same set as [`Self::info`].
    pub fn directory(&mut self) -> Result<&Directory> {
        self.ensure_image()?;
        self.directory
            .as_ref()
            .ok_or(TiffError::Format(FormatError::RequiredTagNotFound(
                Tag::ImageWidth,
            )))
    }

    /// `(width, height)` of the current image.
    ///
    /// # Errors
    /// The same set as [`Self::info`].
    pub fn dimensions(&mut self) -> Result<(u32, u32)> {
        let info = self.info()?;
        Ok((info.width, info.height))
    }

    /// A coarse description of the current image's pixel layout.
    ///
    /// # Errors
    /// The same set as [`Self::info`], plus
    /// [`crate::UnsupportedError::MixedBitDepths`].
    pub fn color_type(&mut self) -> Result<ColorType> {
        self.info()?.color_type()
    }

    /// The native slot type of the current image's samples.
    ///
    /// # Errors
    /// The same set as [`Self::info`], plus
    /// [`crate::UnsupportedError::MixedSampleFormats`].
    pub fn sample_type(&mut self) -> Result<SampleType> {
        self.info()?.sample_type()
    }

    /// Strips or tiles.
    ///
    /// # Errors
    /// The same set as [`Self::info`].
    pub fn chunk_type(&mut self) -> Result<ChunkType> {
        Ok(self.info()?.chunk_type())
    }

    /// Number of strips or tiles, including plane multiplicity.
    ///
    /// # Errors
    /// The same set as [`Self::info`].
    pub fn chunk_count(&mut self) -> Result<u64> {
        Ok(self.info()?.chunk_count())
    }

    /// The nominal chunk size.
    ///
    /// # Errors
    /// The same set as [`Self::info`].
    pub fn chunk_dimensions(&mut self) -> Result<(u32, u32)> {
        Ok(self.info()?.chunk_dimensions())
    }

    /// The valid data size of chunk `index`.
    ///
    /// # Errors
    /// The same set as [`Self::info`], plus
    /// [`UsageError::ChunkIndexOutOfRange`].
    pub fn chunk_data_dimensions(&mut self, index: u64) -> Result<(u32, u32)> {
        self.info()?.chunk_data_dimensions(index)
    }

    /// The layout [`Self::read_image`] produces.
    ///
    /// # Errors
    /// The same set as [`Self::info`].
    pub fn layout(&mut self) -> Result<ImageLayout> {
        self.info()?.layout()
    }

    /// Decodes the whole image into typed samples.
    ///
    /// # Errors
    /// Every failure the decode pipeline can produce.
    pub fn read_image(&mut self) -> Result<Samples> {
        let (width, height) = self.dimensions()?;
        self.read_region(0, 0, width, height)
    }

    /// Decodes the whole image into a caller-owned native-endian byte buffer.
    ///
    /// # Errors
    /// The same set as [`Self::read_image`], plus
    /// [`UsageError::BufferTooSmall`].
    pub fn read_image_bytes(&mut self, dst: &mut [u8]) -> Result<ImageLayout> {
        let (width, height) = self.dimensions()?;
        self.read_region_bytes(Rect::new(0, 0, width, height), dst)
    }

    /// Decodes a rectangle of the image into typed samples.
    ///
    /// Only the strips or tiles that intersect the rectangle are fetched and
    /// decoded, which is what makes a windowed read of a large tiled image
    /// cheap.
    ///
    /// # Errors
    /// [`UsageError::RegionOutOfBounds`] plus every decode failure.
    pub fn read_region(&mut self, x: u32, y: u32, width: u32, height: u32) -> Result<Samples> {
        let rect = Rect::new(x, y, width, height);
        let (sample_type, spp) = {
            let info = self.info()?;
            // Bounds first: an out-of-image rectangle must report
            // `RegionOutOfBounds`, and must do so *before* its area is used to
            // size a buffer. Checking it only inside `read_region_bytes` let a
            // 2^32-wide rectangle allocate (or trip the image-byte guard and
            // report the wrong error) on the way there.
            if !rect.fits_in(info.width, info.height) {
                return Err(TiffError::Usage(UsageError::RegionOutOfBounds {
                    x: rect.x,
                    y: rect.y,
                    width: rect.width,
                    height: rect.height,
                    image_width: info.width,
                    image_height: info.height,
                }));
            }
            (info.sample_type()?, info.samples_per_pixel)
        };
        let count = rect
            .area()
            .checked_mul(u64::from(spp))
            .ok_or(TiffError::IntOverflow)?;
        let bytes = count
            .checked_mul(sample_type.byte_width() as u64)
            .ok_or(TiffError::IntOverflow)?;
        let len = self.limits.check_image_bytes(bytes)?;
        let mut buffer = vec![0u8; len];
        self.read_region_bytes(rect, &mut buffer)?;
        Samples::from_native_bytes(sample_type, &buffer)
    }

    /// Decodes a rectangle into a caller-owned native-endian byte buffer.
    ///
    /// # Errors
    /// The same set as [`Self::read_region`], plus
    /// [`UsageError::BufferTooSmall`].
    pub fn read_region_bytes(&mut self, rect: Rect, dst: &mut [u8]) -> Result<ImageLayout> {
        self.ensure_image()?;
        let info =
            self.info
                .as_ref()
                .ok_or(TiffError::Format(FormatError::RequiredTagNotFound(
                    Tag::ImageWidth,
                )))?;
        if !rect.fits_in(info.width, info.height) {
            return Err(TiffError::Usage(UsageError::RegionOutOfBounds {
                x: rect.x,
                y: rect.y,
                width: rect.width,
                height: rect.height,
                image_width: info.width,
                image_height: info.height,
            }));
        }
        let sample_type = info.sample_type()?;
        let spp = info.samples_per_pixel;
        let row_stride = (rect.width as usize)
            .checked_mul(usize::from(spp))
            .and_then(|n| n.checked_mul(sample_type.byte_width()))
            .ok_or(TiffError::IntOverflow)?;
        let need = row_stride
            .checked_mul(rect.height as usize)
            .ok_or(TiffError::IntOverflow)?;
        if dst.len() < need {
            return Err(TiffError::Usage(UsageError::BufferTooSmall {
                needed: need,
                got: dst.len(),
            }));
        }

        self.budget.reset();
        match info.chunk_type() {
            ChunkType::Strip => strip::decode_into(
                &mut self.reader,
                info,
                rect,
                dst,
                &self.limits,
                self.leniency,
                &mut self.budget,
                self.registry.as_ref(),
                &mut self.warnings,
                &mut self.buffers,
            )?,
            ChunkType::Tile => tile::decode_into(
                &mut self.reader,
                info,
                rect,
                dst,
                &self.limits,
                self.leniency,
                &mut self.budget,
                self.registry.as_ref(),
                &mut self.warnings,
                &mut self.buffers,
            )?,
        }

        Ok(ImageLayout {
            width: rect.width,
            height: rect.height,
            sample_type,
            samples_per_pixel: spp,
            row_stride,
            planes: 1,
            plane_stride: 0,
            total_len: need,
        })
    }

    /// [`Self::read_image`], with strip/tile decompression spread across a
    /// `rayon` thread pool.
    ///
    /// Output is byte-identical to [`Self::read_image`]: only the CPU-bound
    /// decompress/predictor/unpack step of the pipeline runs in parallel (a
    /// fresh [`crate::decode::ChunkBuffers`] per chunk, never shared);
    /// fetching bytes from the underlying reader and placing decoded pixels
    /// into the destination both stay serial, because the reader is a single
    /// `Read + Seek` handle and placement is cheap next to decompression.
    /// See the [`crate::rayon_support`] module docs for the full design.
    ///
    /// # Errors
    /// Every failure [`Self::read_image`] can produce.
    #[cfg(feature = "rayon")]
    pub fn read_image_parallel(&mut self) -> Result<Samples> {
        let (width, height) = self.dimensions()?;
        self.read_region_parallel(0, 0, width, height)
    }

    /// [`Self::read_image_bytes`], parallel per [`Self::read_image_parallel`].
    ///
    /// # Errors
    /// Every failure [`Self::read_image_bytes`] can produce.
    #[cfg(feature = "rayon")]
    pub fn read_image_bytes_parallel(&mut self, dst: &mut [u8]) -> Result<ImageLayout> {
        let (width, height) = self.dimensions()?;
        self.read_region_bytes_parallel(Rect::new(0, 0, width, height), dst)
    }

    /// [`Self::read_region`], parallel per [`Self::read_image_parallel`].
    ///
    /// # Errors
    /// Every failure [`Self::read_region`] can produce.
    #[cfg(feature = "rayon")]
    pub fn read_region_parallel(
        &mut self,
        x: u32,
        y: u32,
        width: u32,
        height: u32,
    ) -> Result<Samples> {
        let rect = Rect::new(x, y, width, height);
        let (sample_type, spp) = {
            let info = self.info()?;
            if !rect.fits_in(info.width, info.height) {
                return Err(TiffError::Usage(UsageError::RegionOutOfBounds {
                    x: rect.x,
                    y: rect.y,
                    width: rect.width,
                    height: rect.height,
                    image_width: info.width,
                    image_height: info.height,
                }));
            }
            (info.sample_type()?, info.samples_per_pixel)
        };
        let count = rect
            .area()
            .checked_mul(u64::from(spp))
            .ok_or(TiffError::IntOverflow)?;
        let bytes = count
            .checked_mul(sample_type.byte_width() as u64)
            .ok_or(TiffError::IntOverflow)?;
        let len = self.limits.check_image_bytes(bytes)?;
        let mut buffer = vec![0u8; len];
        self.read_region_bytes_parallel(rect, &mut buffer)?;
        Samples::from_native_bytes(sample_type, &buffer)
    }

    /// [`Self::read_region_bytes`], parallel per [`Self::read_image_parallel`].
    ///
    /// # Errors
    /// The same set as [`Self::read_region_bytes`].
    #[cfg(feature = "rayon")]
    pub fn read_region_bytes_parallel(
        &mut self,
        rect: Rect,
        dst: &mut [u8],
    ) -> Result<ImageLayout> {
        self.ensure_image()?;
        let info =
            self.info
                .as_ref()
                .ok_or(TiffError::Format(FormatError::RequiredTagNotFound(
                    Tag::ImageWidth,
                )))?;
        if !rect.fits_in(info.width, info.height) {
            return Err(TiffError::Usage(UsageError::RegionOutOfBounds {
                x: rect.x,
                y: rect.y,
                width: rect.width,
                height: rect.height,
                image_width: info.width,
                image_height: info.height,
            }));
        }
        let sample_type = info.sample_type()?;
        let spp = info.samples_per_pixel;
        let row_stride = (rect.width as usize)
            .checked_mul(usize::from(spp))
            .and_then(|n| n.checked_mul(sample_type.byte_width()))
            .ok_or(TiffError::IntOverflow)?;
        let need = row_stride
            .checked_mul(rect.height as usize)
            .ok_or(TiffError::IntOverflow)?;
        if dst.len() < need {
            return Err(TiffError::Usage(UsageError::BufferTooSmall {
                needed: need,
                got: dst.len(),
            }));
        }

        self.budget.reset();
        crate::rayon_support::decode_into_parallel(
            &mut self.reader,
            info,
            rect,
            dst,
            &self.limits,
            self.leniency,
            &mut self.budget,
            self.registry.as_ref(),
            &mut self.warnings,
        )?;

        Ok(ImageLayout {
            width: rect.width,
            height: rect.height,
            sample_type,
            samples_per_pixel: spp,
            row_stride,
            planes: 1,
            plane_stride: 0,
            total_len: need,
        })
    }

    /// The bytes of chunk `index` exactly as they are stored in the file.
    ///
    /// "Raw" here means *literally as stored*: **no `FillOrder` reversal is
    /// applied**. libtiff's `TIFFReadRawStrip` does reverse the bits when tag
    /// 266 is 2, so a caller comparing against libtiff — or hand-decoding a
    /// chunk while debugging a codec — must call
    /// [`crate::sample::apply_fill_order`] on the returned bytes first when
    /// [`ImageInfo::fill_order`](crate::ImageInfo) is
    /// [`FillOrder::Lsb2Msb`](crate::FillOrder::Lsb2Msb) and the compression
    /// is not one of the CCITT methods
    /// ([`crate::compression::handles_fill_order`]). [`Self::read_chunk`] and
    /// [`Self::read_image`] do this for you.
    ///
    /// # Errors
    /// [`FormatError::ChunkOffsetOutOfBounds`] plus I/O failures.
    pub fn read_chunk_raw(&mut self, index: u64) -> Result<Vec<u8>> {
        self.ensure_image()?;
        let info =
            self.info
                .as_ref()
                .ok_or(TiffError::Format(FormatError::RequiredTagNotFound(
                    Tag::ImageWidth,
                )))?;
        crate::decode::fetch_chunk(
            &mut self.reader,
            info,
            index,
            &self.limits,
            self.leniency,
            &mut self.warnings,
            &mut self.buffers,
        )?;
        Ok(self.buffers.compressed().to_vec())
    }

    /// Decodes chunk `index` at its coded size, without cropping.
    ///
    /// # Output budget
    ///
    /// [`Self::read_image`] and [`Self::read_region`] reset the file-level
    /// [`OutputBudget`] before they start, so each of them is bounded by
    /// [`Limits::max_image_bytes`] however many strips it decodes. `read_chunk`
    /// deliberately does **not** reset it: it *adds* to the running total, so a
    /// caller that walks the chunks itself is bounded exactly as a whole-image
    /// read would be, and cannot get `chunk_count` times the cap by decoding one
    /// chunk at a time. Decoding more bytes than the budget allows — by reading
    /// the same chunk repeatedly, or by mixing `read_image` and `read_chunk` on
    /// an image that already fills the budget — is therefore
    /// [`crate::LimitError::OutputBudget`]; raise
    /// [`Limits::max_image_bytes`] or re-create the decoder to start over.
    ///
    /// # Errors
    /// Every failure the decode pipeline can produce.
    pub fn read_chunk(&mut self, index: u64) -> Result<Samples> {
        self.ensure_image()?;
        let info =
            self.info
                .as_ref()
                .ok_or(TiffError::Format(FormatError::RequiredTagNotFound(
                    Tag::ImageWidth,
                )))?;
        let shape = decode_chunk(
            &mut self.reader,
            info,
            index,
            &self.limits,
            self.leniency,
            &mut self.budget,
            self.registry.as_ref(),
            &mut self.warnings,
            &mut self.buffers,
        )?;
        Samples::from_native_bytes(shape.sample_type, self.buffers.native())
    }

    /// [`Self::read_chunk_raw`] for a strip image.
    ///
    /// # Errors
    /// [`UsageError::InvalidSpec`] when the image is tiled, plus the usual set.
    pub fn read_strip_raw(&mut self, index: u64) -> Result<Vec<u8>> {
        self.require_chunk_type(ChunkType::Strip)?;
        self.read_chunk_raw(index)
    }

    /// [`Self::read_chunk`] for a strip image.
    ///
    /// # Errors
    /// [`UsageError::InvalidSpec`] when the image is tiled, plus the usual set.
    pub fn read_strip(&mut self, index: u64) -> Result<Samples> {
        self.require_chunk_type(ChunkType::Strip)?;
        self.read_chunk(index)
    }

    /// [`Self::read_chunk_raw`] for a tiled image.
    ///
    /// # Errors
    /// [`UsageError::InvalidSpec`] when the image uses strips, plus the usual set.
    pub fn read_tile_raw(&mut self, index: u64) -> Result<Vec<u8>> {
        self.require_chunk_type(ChunkType::Tile)?;
        self.read_chunk_raw(index)
    }

    /// [`Self::read_chunk`] for a tiled image.
    ///
    /// # Errors
    /// [`UsageError::InvalidSpec`] when the image uses strips, plus the usual set.
    pub fn read_tile(&mut self, index: u64) -> Result<Samples> {
        self.require_chunk_type(ChunkType::Tile)?;
        self.read_chunk(index)
    }

    fn require_chunk_type(&mut self, wanted: ChunkType) -> Result<()> {
        let actual = self.chunk_type()?;
        if actual != wanted {
            return Err(TiffError::Usage(UsageError::InvalidSpec(format!(
                "this image is stored in {actual}s, not {wanted}s"
            ))));
        }
        Ok(())
    }

    /// The whole image converted to interleaved 8-bit RGB.
    ///
    /// # Errors
    /// Every decode failure plus
    /// [`crate::UnsupportedError::Conversion`].
    pub fn read_image_rgb8(&mut self) -> Result<Vec<u8>> {
        let samples = self.read_image()?;
        let info = self.info()?;
        crate::colour::to_rgb8(info, &samples)
    }

    /// The whole image converted to interleaved 8-bit RGBA with straight alpha.
    ///
    /// Associated (premultiplied) alpha is undone.
    ///
    /// # Errors
    /// The same set as [`Self::read_image_rgb8`].
    pub fn read_image_rgba8(&mut self) -> Result<Vec<u8>> {
        let samples = self.read_image()?;
        let info = self.info()?;
        crate::colour::to_rgba8(info, &samples)
    }

    /// Reads the value of a tag, erroring when it is absent.
    ///
    /// # Errors
    /// [`FormatError::RequiredTagNotFound`] plus value-loading failures.
    pub fn get_tag(&mut self, tag: Tag) -> Result<Value> {
        self.find_tag(tag)?
            .ok_or(TiffError::Format(FormatError::RequiredTagNotFound(tag)))
    }

    /// Reads the value of a tag, or `None` when it is absent.
    ///
    /// # Errors
    /// Value-loading failures.
    pub fn find_tag(&mut self, tag: Tag) -> Result<Option<Value>> {
        self.find_tag_raw(tag.to_u16())
    }

    /// Reads a tag by raw number, or `None` when it is absent.
    ///
    /// # Errors
    /// Value-loading failures.
    pub fn find_tag_raw(&mut self, tag: u16) -> Result<Option<Value>> {
        self.ensure_image()?;
        let Some(directory) = self.directory.as_ref() else {
            return Ok(None);
        };
        let Some(entry) = directory.get_raw(tag).copied() else {
            return Ok(None);
        };
        Ok(Some(load_value(&mut self.reader, &self.limits, &entry)?))
    }

    /// A tag's first value as a `u64`.
    ///
    /// # Errors
    /// Value-loading failures.
    pub fn get_tag_u64(&mut self, tag: Tag) -> Result<Option<u64>> {
        Ok(self.find_tag(tag)?.and_then(|v| v.first_u64()))
    }

    /// A tag's first value as a `u32`.
    ///
    /// # Errors
    /// Value-loading failures.
    pub fn get_tag_u32(&mut self, tag: Tag) -> Result<Option<u32>> {
        Ok(self.find_tag(tag)?.and_then(|v| v.first_u32()))
    }

    /// A tag's values widened to `u64`.
    ///
    /// # Errors
    /// Value-loading failures.
    pub fn get_tag_u64_vec(&mut self, tag: Tag) -> Result<Option<Vec<u64>>> {
        Ok(self.find_tag(tag)?.and_then(|v| v.as_u64_vec()))
    }

    /// A tag's values as `f64`s.
    ///
    /// # Errors
    /// Value-loading failures.
    pub fn get_tag_f64_vec(&mut self, tag: Tag) -> Result<Option<Vec<f64>>> {
        Ok(self.find_tag(tag)?.and_then(|v| v.as_f64_vec()))
    }

    /// An `ASCII` tag's text.
    ///
    /// # Errors
    /// Value-loading failures.
    pub fn get_tag_ascii(&mut self, tag: Tag) -> Result<Option<String>> {
        Ok(self
            .find_tag(tag)?
            .and_then(|v| v.as_str().map(str::to_string)))
    }

    /// A `BYTE`/`UNDEFINED` tag's bytes.
    ///
    /// # Errors
    /// Value-loading failures.
    pub fn get_tag_bytes(&mut self, tag: Tag) -> Result<Option<Vec<u8>>> {
        Ok(self
            .find_tag(tag)?
            .and_then(|v| v.as_bytes().map(<[u8]>::to_vec)))
    }

    /// Every tag of the current image, in ascending numeric order.
    ///
    /// # Errors
    /// Value-loading failures.
    pub fn all_tags(&mut self) -> Result<Vec<(Tag, Value)>> {
        self.ensure_image()?;
        let entries: Vec<Entry> = self
            .directory
            .as_ref()
            .map(|d| d.iter().map(|(_, e)| *e).collect())
            .unwrap_or_default();
        let mut out = Vec::with_capacity(entries.len());
        for entry in entries {
            let value = load_value(&mut self.reader, &self.limits, &entry)?;
            out.push((entry.tag(), value));
        }
        Ok(out)
    }

    /// The immediate children of tag 330.
    ///
    /// # Errors
    /// Directory-parsing failures.
    pub fn sub_ifds(&mut self) -> Result<Vec<Directory>> {
        let pointers = self.info()?.sub_ifds.clone();
        let mut out = Vec::with_capacity(pointers.len());
        let mut walker = IfdWalker::new();
        for offset in &self.offsets {
            walker.visit(*offset, &self.limits)?;
        }
        for pointer in pointers {
            if walker.seen(pointer.get()) {
                continue;
            }
            walker.visit(pointer.get(), &self.limits)?;
            out.push(self.read_directory_at(pointer)?);
        }
        Ok(out)
    }

    /// The full SubIFD tree, with its own visited set and a depth cap.
    ///
    /// A SubIFD may itself carry tag 330, so the tree needs cycle detection
    /// independent of the main chain's.
    ///
    /// # Errors
    /// [`FormatError::IfdCycle`] and [`crate::LimitError::IfdCount`].
    pub fn sub_ifd_tree(&mut self) -> Result<Vec<SubIfdNode>> {
        let pointers = self.info()?.sub_ifds.clone();
        let mut walker = IfdWalker::new();
        for offset in &self.offsets {
            walker.visit(*offset, &self.limits)?;
        }
        self.build_sub_tree(&pointers, &mut walker, 0)
    }

    fn build_sub_tree(
        &mut self,
        pointers: &[IfdPointer],
        walker: &mut IfdWalker,
        depth: usize,
    ) -> Result<Vec<SubIfdNode>> {
        if depth >= self.limits.max_ifd_depth {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        for pointer in pointers {
            if walker.seen(pointer.get()) {
                continue;
            }
            walker.visit(pointer.get(), &self.limits)?;
            let directory = self.read_directory_at(*pointer)?;
            let child_pointers = match directory.get(Tag::SubIfds).copied() {
                Some(entry) => {
                    let mut loader = Loader {
                        reader: &mut self.reader,
                        limits: &self.limits,
                    };
                    crate::ifd::read_ifd_pointers(&entry, &mut loader, self.leniency)?
                }
                None => Vec::new(),
            };
            let children = self.build_sub_tree(&child_pointers, walker, depth + 1)?;
            out.push(SubIfdNode {
                pointer: *pointer,
                directory,
                children,
            });
        }
        Ok(out)
    }

    /// The EXIF sub-IFD (tag 34665).
    ///
    /// # Errors
    /// Directory-parsing failures.
    pub fn exif_directory(&mut self) -> Result<Option<Directory>> {
        let pointer = self.info()?.exif_ifd;
        self.optional_directory(pointer)
    }

    /// The GPS sub-IFD (tag 34853).
    ///
    /// # Errors
    /// Directory-parsing failures.
    pub fn gps_directory(&mut self) -> Result<Option<Directory>> {
        let pointer = self.info()?.gps_ifd;
        self.optional_directory(pointer)
    }

    /// The EXIF interoperability sub-IFD (tag 40965).
    ///
    /// # Errors
    /// Directory-parsing failures.
    pub fn interop_directory(&mut self) -> Result<Option<Directory>> {
        let pointer = self.info()?.interop_ifd;
        self.optional_directory(pointer)
    }

    fn optional_directory(&mut self, pointer: Option<IfdPointer>) -> Result<Option<Directory>> {
        match pointer {
            Some(pointer) => Ok(Some(self.read_directory_at(pointer)?)),
            None => Ok(None),
        }
    }

    /// Parses an arbitrary IFD at `pointer`.
    ///
    /// # Errors
    /// Directory-parsing failures.
    pub fn read_directory_at(&mut self, pointer: IfdPointer) -> Result<Directory> {
        Directory::read(
            &mut self.reader,
            pointer.get(),
            self.header.variant,
            &self.limits,
            &mut self.warnings,
        )
    }

    /// The embedded ICC profile (tag 34675).
    ///
    /// # Errors
    /// Value-loading failures.
    pub fn icc_profile(&mut self) -> Result<Option<Vec<u8>>> {
        self.get_tag_bytes(Tag::InterColorProfile)
    }

    /// The XMP packet (tag 700).
    ///
    /// # Errors
    /// Value-loading failures.
    pub fn xmp(&mut self) -> Result<Option<Vec<u8>>> {
        self.get_tag_bytes(Tag::Xmp)
    }

    /// The IPTC/NAA blob (tag 33723).
    ///
    /// # Errors
    /// Value-loading failures.
    pub fn iptc(&mut self) -> Result<Option<Vec<u8>>> {
        match self.find_tag(Tag::IptcNaa)? {
            Some(value) => Ok(value
                .as_bytes()
                .map(<[u8]>::to_vec)
                .or_else(|| value.to_bytes(self.header.endian).ok().map(|(b, _)| b))),
            None => Ok(None),
        }
    }

    /// The Photoshop image-resource blob (tag 34377).
    ///
    /// # Errors
    /// Value-loading failures.
    pub fn photoshop(&mut self) -> Result<Option<Vec<u8>>> {
        self.get_tag_bytes(Tag::Photoshop)
    }

    /// The six GeoTIFF tags, carried through verbatim.
    ///
    /// # Errors
    /// Value-loading failures.
    pub fn geo_tags(&mut self) -> Result<GeoTags> {
        Ok(GeoTags {
            model_pixel_scale: self.find_tag(Tag::ModelPixelScale)?,
            model_tiepoint: self.find_tag(Tag::ModelTiepoint)?,
            model_transformation: self.find_tag(Tag::ModelTransformation)?,
            geo_key_directory: self.find_tag(Tag::GeoKeyDirectory)?,
            geo_double_params: self.find_tag(Tag::GeoDoubleParams)?,
            geo_ascii_params: self.find_tag(Tag::GeoAsciiParams)?,
        })
    }

    /// Unwraps the underlying reader.
    pub fn into_inner(self) -> R {
        self.reader.into_inner()
    }
}
