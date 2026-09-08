//! The native encoder API.
//!
//! [`Encoder`] streams chunk payloads as they are handed over, so memory stays
//! O(chunk) rather than O(image), and patches the IFD offsets afterwards. The
//! file header is written lazily, on the first page, so
//! [`VariantChoice::Auto`] can promote to BigTIFF *before* any byte is
//! committed.
//!
//! ```
//! use oxiarc_tiff::{ColorType, Compression, Encoder, ImageSpec, Layout};
//! use std::io::Cursor;
//!
//! let spec = ImageSpec::new(4, 4, ColorType::Gray(8))
//!     .with_compression(Compression::PackBits)
//!     .with_layout(Layout::Strips { rows_per_strip: 2 });
//! let mut buffer = Cursor::new(Vec::new());
//! let mut encoder = Encoder::new(&mut buffer)?;
//! encoder.write_image(&spec, &[7u8; 16])?;
//! encoder.finish()?;
//! assert!(buffer.into_inner().len() > 8);
//! # Ok::<(), oxiarc_tiff::TiffError>(())
//! ```

pub mod directory;
pub mod image;

pub use directory::{DirectoryWriter, WrittenDirectory};
pub use image::ImageWriter;

use std::io::{Seek, Write};

use crate::byteorder::{Endian, EndianWriter};
use crate::compression::{CodecLevel, CodecRegistry};
use crate::error::{Result, TiffError, UsageError};
use crate::header::{Header, Variant};
use crate::ifd::{Rational, Value};
use crate::image::ColorType;
use crate::sample::{SampleType, packed_row_bytes};
use crate::tags::{
    CompressionMethod, ExtraSamples, FillOrder, PhotometricInterpretation, PlanarConfiguration,
    Predictor, ResolutionUnit, SampleFormat,
};

/// The compression a page is written with.
///
/// Every named variant is wired up: encoding with any of them (subject to
/// the codec's own cargo feature being compiled in --
/// [`crate::UnsupportedError::FeatureNotCompiled`] otherwise) produces a file
/// this crate, libtiff and Pillow/`tifffile` all decode. [`Self::Registered`]
/// selects an out-of-tree codec added through [`crate::CodecRegistry`]
/// instead of one of this crate's own -- see [`crate::Codec`]'s docs for a
/// full worked example (a plugin codec, registered for both directions).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum Compression {
    /// Uncompressed (tag value 1).
    #[default]
    None,
    /// Apple PackBits (32773).
    PackBits,
    /// LZW (5).
    Lzw,
    /// Deflate, written with the given zlib level.
    ///
    /// The tag value written is **8** (`AdobeDeflate`), which is what libtiff,
    /// GDAL and `tifffile` all write; 32946 is the older private registration
    /// and is read identically.
    Deflate {
        /// 0..=9.
        level: u8,
    },
    /// Zstandard (50000).
    Zstd {
        /// The zstd compression level.
        level: i32,
    },
    /// LZMA2 framed as `.xz` (34925).
    Lzma {
        /// The xz preset, 0..=9.
        preset: u8,
    },
    /// CCITT modified Huffman RLE (2).
    CcittRle,
    /// CCITT Group 3 (3).
    CcittGroup3 {
        /// Whether 2D coding is used.
        two_dimensional: bool,
        /// Whether EOLs are byte-aligned with fill bits.
        byte_align_eol: bool,
    },
    /// CCITT Group 4 (4).
    CcittGroup4,
    /// JPEG (7).
    Jpeg {
        /// 1..=100.
        quality: u8,
        /// Whether the tables go in tag 347 rather than in every chunk.
        shared_tables: bool,
    },
    /// An out-of-tree codec added through [`crate::CodecRegistry`], named by
    /// its raw `Compression` (259) tag value.
    ///
    /// The registry must be attached with [`Encoder::with_codecs`] *and*
    /// contain a [`crate::Codec`] whose [`crate::Codec::method`] returns this
    /// value, or encoding fails with
    /// [`crate::UnsupportedError::Compression`] -- exactly as an unregistered
    /// value would. [`Self::level`] always resolves to
    /// [`crate::compression::CodecLevel::Default`] for this variant: a
    /// plugin codec that wants tunable effort takes it from its own
    /// constructor, not from this enum.
    Registered(u16),
}

impl Compression {
    /// The TIFF compression tag value.
    #[must_use]
    pub const fn method(self) -> CompressionMethod {
        match self {
            Self::None => CompressionMethod::None,
            Self::PackBits => CompressionMethod::PackBits,
            Self::Lzw => CompressionMethod::Lzw,
            Self::Deflate { .. } => CompressionMethod::AdobeDeflate8,
            Self::Zstd { .. } => CompressionMethod::Zstd,
            Self::Lzma { .. } => CompressionMethod::Lzma,
            Self::CcittRle => CompressionMethod::CcittRle,
            Self::CcittGroup3 { .. } => CompressionMethod::CcittFax3,
            Self::CcittGroup4 => CompressionMethod::CcittFax4,
            Self::Jpeg { .. } => CompressionMethod::Jpeg,
            Self::Registered(code) => CompressionMethod::from_u16(code),
        }
    }

    /// The effort level handed to the codec.
    #[must_use]
    pub const fn level(self) -> CodecLevel {
        match self {
            Self::Deflate { level } => CodecLevel::Level(level as i32),
            Self::Zstd { level } => CodecLevel::Level(level),
            Self::Lzma { preset } => CodecLevel::Level(preset as i32),
            Self::Jpeg { quality, .. } => CodecLevel::Level(quality as i32),
            _ => CodecLevel::Default,
        }
    }

    /// The `T4Options` bit field this compression implies.
    #[must_use]
    pub const fn t4_options(self) -> u32 {
        match self {
            Self::CcittGroup3 {
                two_dimensional,
                byte_align_eol,
            } => {
                let mut bits = 0u32;
                if two_dimensional {
                    bits |= 1;
                }
                if byte_align_eol {
                    bits |= 4;
                }
                bits
            }
            _ => 0,
        }
    }
}

/// Whether a page is stored in strips or tiles.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Layout {
    /// Row bands of `rows_per_strip` rows.
    Strips {
        /// Rows in each strip except possibly the last.
        rows_per_strip: u32,
    },
    /// Rectangular tiles; both dimensions must be multiples of 16.
    Tiles {
        /// Tile width in pixels.
        width: u32,
        /// Tile height in pixels.
        length: u32,
    },
}

/// Which container shape to write.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum VariantChoice {
    /// Always classic TIFF; a file that outgrows 32-bit offsets is an error.
    #[default]
    Classic,
    /// Always BigTIFF.
    Big,
    /// Classic unless the projected size would exceed roughly 4 GiB.
    ///
    /// The projection happens before the first byte is written and cannot be
    /// revised afterwards, so it is deliberately conservative. When the final
    /// size is not known up front, choose [`VariantChoice::Big`] explicitly.
    Auto,
}

/// Everything needed to write one page.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct ImageSpec {
    /// `ImageWidth`.
    pub width: u32,
    /// `ImageLength`.
    pub height: u32,
    /// `SamplesPerPixel`.
    pub samples_per_pixel: u16,
    /// `BitsPerSample`, one entry per channel.
    pub bits_per_sample: Vec<u16>,
    /// `SampleFormat`, one entry per channel.
    pub sample_format: Vec<SampleFormat>,
    /// `PhotometricInterpretation`.
    pub photometric: PhotometricInterpretation,
    /// `PlanarConfiguration`.
    pub planar: PlanarConfiguration,
    /// The codec.
    pub compression: Compression,
    /// `Predictor`.
    pub predictor: Predictor,
    /// `FillOrder`.
    pub fill_order: FillOrder,
    /// Strips or tiles.
    pub layout: Layout,
    /// `ColorMap`, `3 * 2^bits` 16-bit entries.
    pub color_map: Option<Vec<u16>>,
    /// `ExtraSamples`.
    pub extra_samples: Vec<ExtraSamples>,
    /// `XResolution`, `YResolution`, `ResolutionUnit`.
    pub resolution: Option<(Rational, Rational, ResolutionUnit)>,
    /// `YCbCrSubSampling`.
    pub ycbcr_subsampling: Option<(u16, u16)>,
    /// How often a JPEG chunk writes a restart marker, in MCU rows.
    ///
    /// `0`, the default, writes none, which is what libtiff writes.
    pub jpeg_restart_rows: u16,
    /// Whether the CCITT encoders may use T.4 uncompressed mode.
    ///
    /// `false`, the default: libtiff refuses to *read* the mode, so a file
    /// that used it uninvited would be unreadable there. See
    /// [`ImageSpec::with_ccitt_uncompressed`].
    pub ccitt_uncompressed: bool,
    /// Arbitrary extra tags: GeoTIFF, ICC, XMP, `DocumentName`, ...
    pub extra_tags: Vec<(u16, Value)>,
}

impl ImageSpec {
    /// A page of `width` x `height` pixels in the given colour layout.
    ///
    /// The strip height defaults to roughly 8 KiB of pixel data, which is what
    /// libtiff picks. Every channel's `SampleFormat` defaults to
    /// [`SampleFormat::Uint`], because [`ColorType`] names a bit depth and a
    /// colour model but not a numeric format -- a page of signed or
    /// floating-point samples **must** call
    /// [`with_sample_format`](Self::with_sample_format), or it will be
    /// written with the right bytes under the wrong label and read back as
    /// unsigned integers everywhere, with no error reported.
    #[must_use]
    pub fn new(width: u32, height: u32, color: ColorType) -> Self {
        let spp = color.samples_per_pixel();
        let bits = u16::from(color.bit_depth());
        let row_bytes = packed_row_bytes(&[bits], (width as usize) * usize::from(spp)).max(1);
        let rows_per_strip = ((8192 / row_bytes).max(1) as u32).min(height.max(1));
        Self {
            width,
            height,
            samples_per_pixel: spp,
            bits_per_sample: vec![bits; spp as usize],
            sample_format: vec![SampleFormat::Uint; spp as usize],
            photometric: color.photometric(),
            planar: PlanarConfiguration::Chunky,
            compression: Compression::None,
            predictor: Predictor::None,
            fill_order: FillOrder::Msb2Lsb,
            layout: Layout::Strips { rows_per_strip },
            color_map: None,
            extra_samples: color.extra_samples(),
            resolution: None,
            // A YCbCr image with no tag 530 is read back as 2x2 subsampled,
            // because that is the TIFF 6.0 default. Full-resolution chroma
            // therefore has to say so explicitly.
            ycbcr_subsampling: matches!(color, ColorType::YCbCr(_)).then_some((1, 1)),
            jpeg_restart_rows: 0,
            ccitt_uncompressed: false,
            extra_tags: Vec::new(),
        }
    }

    /// Overrides the per-channel bit depths.
    #[must_use]
    pub fn with_bits_per_sample(mut self, bits: Vec<u16>) -> Self {
        self.samples_per_pixel = bits.len() as u16;
        self.sample_format.resize(bits.len(), SampleFormat::Uint);
        self.bits_per_sample = bits;
        self
    }

    /// Overrides the per-channel numeric formats.
    #[must_use]
    pub fn with_sample_format(mut self, format: SampleFormat) -> Self {
        self.sample_format = vec![format; self.samples_per_pixel as usize];
        self
    }

    /// Overrides the photometric interpretation.
    #[must_use]
    pub fn with_photometric(mut self, photometric: PhotometricInterpretation) -> Self {
        self.photometric = photometric;
        self
    }

    /// Chooses chunky or planar storage.
    #[must_use]
    pub fn with_planar(mut self, planar: PlanarConfiguration) -> Self {
        self.planar = planar;
        self
    }

    /// Chooses the codec.
    #[must_use]
    pub fn with_compression(mut self, compression: Compression) -> Self {
        self.compression = compression;
        self
    }

    /// Chooses the predictor.
    ///
    /// # Interoperability caveat
    ///
    /// libtiff installs its predictor hooks only from the codecs that call
    /// `TIFFPredictorInit` — LZW, Deflate, ZSTD, LZMA, PixarLog and LERC. With
    /// [`Compression::None`] or [`Compression::PackBits`] it ignores tag 317
    /// entirely, so a file written with a predictor *and* an unpredicted codec
    /// round-trips perfectly through this crate but is misread by libtiff,
    /// Pillow and `tifffile`. The combination is accepted (TIFF 6.0 does not
    /// forbid it, and `oxigeo`'s reader honours it) but should not be written
    /// for interchange. `tests/tiff_oracle.rs` pins this behaviour.
    #[must_use]
    pub fn with_predictor(mut self, predictor: Predictor) -> Self {
        self.predictor = predictor;
        self
    }

    /// Chooses the bit fill order.
    #[must_use]
    pub fn with_fill_order(mut self, fill_order: FillOrder) -> Self {
        self.fill_order = fill_order;
        self
    }

    /// Chooses strips or tiles.
    #[must_use]
    pub fn with_layout(mut self, layout: Layout) -> Self {
        self.layout = layout;
        self
    }

    /// Attaches a palette (and switches the photometric to `Palette`).
    #[must_use]
    pub fn with_color_map(mut self, map: Vec<u16>) -> Self {
        self.photometric = PhotometricInterpretation::Palette;
        self.color_map = Some(map);
        self
    }

    /// Declares the meaning of the channels beyond the photometric ones.
    #[must_use]
    pub fn with_extra_samples(mut self, extra: Vec<ExtraSamples>) -> Self {
        self.extra_samples = extra;
        self
    }

    /// Records the physical resolution.
    #[must_use]
    pub fn with_resolution(mut self, x: Rational, y: Rational, unit: ResolutionUnit) -> Self {
        self.resolution = Some((x, y, unit));
        self
    }

    /// Declares YCbCr chroma subsampling.
    #[must_use]
    pub fn with_ycbcr_subsampling(mut self, h: u16, v: u16) -> Self {
        self.ycbcr_subsampling = Some((h, v));
        self
    }

    /// Writes a JPEG restart marker every `rows` MCU rows (`cjpeg -restart`).
    ///
    /// `0` restores libtiff's behaviour: no `DRI` segment and no `RSTn`
    /// markers. Only meaningful for [`Compression::Jpeg`]; every other codec
    /// ignores it.
    #[must_use]
    pub fn with_jpeg_restart_rows(mut self, rows: u16) -> Self {
        self.jpeg_restart_rows = rows;
        self
    }

    /// Lets the CCITT encoders fall into T.4 uncompressed mode (§4.2.1.3.2)
    /// for rows where it is smaller, and sets the option-tag bit that says so
    /// (`T4Options` bit 1 for Group 3, `T6Options` bit 1 for Group 4).
    ///
    /// The mode transmits pixels at about one bit each instead of Huffman
    /// coding runs, so it wins on dithered or halftoned regions — where fax
    /// coding *expands* the data — and loses everywhere else. The encoder
    /// prices both codings of every row exactly and picks the smaller, so
    /// turning this on can only shrink a page.
    ///
    /// # Interoperability
    ///
    /// **libtiff 4.7.1 cannot read uncompressed mode** (`Fax3Decode2D`
    /// reports `Uncompressed data (not supported)`), and neither can the
    /// readers built on it. It parses the file — `tiffinfo` reports the tags
    /// and the option bit — but the pixels of any row that used the mode do
    /// not come out. Leave this off for files that have to be read
    /// elsewhere; this crate reads its own output, and every other
    /// uncompressed-mode file, either way.
    ///
    /// Ignored for compression 2 and 32771 (the bare RLE dialects), which
    /// have no option tag to declare it in.
    #[must_use]
    pub fn with_ccitt_uncompressed(mut self, allowed: bool) -> Self {
        self.ccitt_uncompressed = allowed;
        self
    }

    /// Adds one arbitrary tag, written verbatim.
    ///
    /// This is how GeoTIFF tags, ICC profiles, XMP packets and page metadata
    /// round-trip: read them with [`crate::Decoder::geo_tags`] or
    /// [`crate::Decoder::find_tag`] and hand the [`Value`] straight back.
    #[must_use]
    pub fn with_extra_tag(mut self, tag: u16, value: Value) -> Self {
        self.extra_tags.push((tag, value));
        self
    }

    /// Adds several arbitrary tags.
    #[must_use]
    pub fn with_extra_tags(mut self, tags: Vec<(u16, Value)>) -> Self {
        self.extra_tags.extend(tags);
        self
    }

    /// The native slot type the caller's sample buffer uses.
    ///
    /// # Errors
    /// [`crate::UnsupportedError::MixedSampleFormats`] and
    /// [`crate::UnsupportedError::BitsPerSample`].
    pub fn sample_type(&self) -> Result<SampleType> {
        let format = self
            .sample_format
            .first()
            .copied()
            .unwrap_or(SampleFormat::Uint);
        let widest = self.bits_per_sample.iter().copied().max().unwrap_or(8);
        SampleType::resolve(widest, format)
    }

    /// Channels carried by one chunk.
    #[must_use]
    pub fn chunk_samples_per_pixel(&self) -> u16 {
        if self.planar == PlanarConfiguration::Planar {
            1
        } else {
            self.samples_per_pixel
        }
    }

    /// Number of planes.
    #[must_use]
    pub fn plane_count(&self) -> u16 {
        if self.planar == PlanarConfiguration::Planar {
            self.samples_per_pixel
        } else {
            1
        }
    }

    /// Chunks across the image.
    #[must_use]
    pub fn chunks_across(&self) -> u32 {
        match self.layout {
            Layout::Strips { .. } => 1,
            Layout::Tiles { width, .. } => {
                if width == 0 {
                    0
                } else {
                    self.width.div_ceil(width)
                }
            }
        }
    }

    /// Chunks down the image.
    #[must_use]
    pub fn chunks_down(&self) -> u32 {
        match self.layout {
            Layout::Strips { rows_per_strip } => {
                if rows_per_strip == 0 {
                    0
                } else {
                    self.height.div_ceil(rows_per_strip)
                }
            }
            Layout::Tiles { length, .. } => {
                if length == 0 {
                    0
                } else {
                    self.height.div_ceil(length)
                }
            }
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
        u16::try_from(index / self.chunks_per_plane().max(1)).unwrap_or(0)
    }

    /// The image-space origin of a chunk.
    #[must_use]
    pub fn chunk_origin(&self, index: u64) -> (u32, u32) {
        let within = index % self.chunks_per_plane().max(1);
        match self.layout {
            Layout::Strips { rows_per_strip } => (
                0,
                u32::try_from(within)
                    .ok()
                    .and_then(|v| v.checked_mul(rows_per_strip))
                    .unwrap_or(0),
            ),
            Layout::Tiles { width, length } => {
                let across = u64::from(self.chunks_across().max(1));
                let col = u32::try_from(within % across).unwrap_or(0);
                let row = u32::try_from(within / across).unwrap_or(0);
                (col.saturating_mul(width), row.saturating_mul(length))
            }
        }
    }

    /// The dimensions a chunk is coded at (tiles are padded, strips clipped).
    #[must_use]
    pub fn chunk_coded_dimensions(&self, index: u64) -> (u32, u32) {
        match self.layout {
            Layout::Strips { rows_per_strip } => {
                let (_, y) = self.chunk_origin(index);
                (
                    self.width,
                    rows_per_strip.min(self.height.saturating_sub(y)),
                )
            }
            Layout::Tiles { width, length } => (width, length),
        }
    }

    /// The dimensions of the valid data inside a chunk.
    #[must_use]
    pub fn chunk_data_dimensions(&self, index: u64) -> (u32, u32) {
        let (coded_w, coded_h) = self.chunk_coded_dimensions(index);
        let (x, y) = self.chunk_origin(index);
        (
            coded_w.min(self.width.saturating_sub(x)),
            coded_h.min(self.height.saturating_sub(y)),
        )
    }

    /// Bit depths carried by one chunk of the given plane.
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

    /// `true` when the page is written in YCbCr subsampling units.
    #[must_use]
    pub fn is_subsampled(&self) -> bool {
        self.photometric == PhotometricInterpretation::YCbCr
            && self.planar == PlanarConfiguration::Chunky
            && self.ycbcr_subsampling.map(|s| s != (1, 1)).unwrap_or(false)
    }

    /// Bytes the caller's native sample buffer for one chunk occupies.
    ///
    /// # Errors
    /// [`TiffError::IntOverflow`] when the product does not fit.
    pub fn chunk_native_len(&self, index: u64) -> Result<usize> {
        let (w, h) = self.chunk_coded_dimensions(index);
        let slot = self.sample_type()?.byte_width();
        let spp = usize::from(self.chunk_samples_per_pixel());
        (w as usize)
            .checked_mul(h as usize)
            .and_then(|n| n.checked_mul(spp))
            .and_then(|n| n.checked_mul(slot))
            .ok_or(TiffError::IntOverflow)
    }

    /// Bytes the packed, file-ordered chunk occupies before compression.
    ///
    /// # Errors
    /// [`TiffError::IntOverflow`] when the product does not fit.
    pub fn chunk_packed_len(&self, index: u64) -> Result<usize> {
        let (w, h) = self.chunk_coded_dimensions(index);
        if self.is_subsampled() {
            let (sh, sv) = self.ycbcr_subsampling.unwrap_or((2, 2));
            let bits = self.bits_per_sample.first().copied().unwrap_or(8);
            let width_bytes = usize::from(bits / 8).max(1);
            let units =
                (w as usize).div_ceil(usize::from(sh)) * (h as usize).div_ceil(usize::from(sv));
            return units
                .checked_mul(usize::from(sh) * usize::from(sv) + 2)
                .and_then(|n| n.checked_mul(width_bytes))
                .ok_or(TiffError::IntOverflow);
        }
        let bits = self.plane_bits(self.chunk_plane(index));
        let samples = (w as usize)
            .checked_mul(usize::from(self.chunk_samples_per_pixel()))
            .ok_or(TiffError::IntOverflow)?;
        let row = packed_row_bytes(&bits, samples) as usize;
        row.checked_mul(h as usize).ok_or(TiffError::IntOverflow)
    }

    /// Bytes the caller's whole-image buffer occupies.
    ///
    /// # Errors
    /// [`TiffError::IntOverflow`] when the product does not fit.
    pub fn image_native_len(&self) -> Result<usize> {
        let slot = self.sample_type()?.byte_width();
        (self.width as usize)
            .checked_mul(self.height as usize)
            .and_then(|n| n.checked_mul(usize::from(self.samples_per_pixel)))
            .and_then(|n| n.checked_mul(slot))
            .ok_or(TiffError::IntOverflow)
    }

    /// Rejects a specification that cannot produce a valid file.
    ///
    /// # Errors
    /// [`UsageError::InvalidSpec`] with a message naming the problem.
    pub fn validate(&self) -> Result<()> {
        let invalid = |message: String| TiffError::Usage(UsageError::InvalidSpec(message));
        if self.width == 0 || self.height == 0 {
            return Err(invalid(format!(
                "image dimensions must be non-zero, got {}x{}",
                self.width, self.height
            )));
        }
        if self.samples_per_pixel == 0 {
            return Err(invalid("SamplesPerPixel must be non-zero".to_string()));
        }
        if self.bits_per_sample.len() != self.samples_per_pixel as usize {
            return Err(invalid(format!(
                "BitsPerSample has {} entries but SamplesPerPixel is {}",
                self.bits_per_sample.len(),
                self.samples_per_pixel
            )));
        }
        if self.sample_format.len() != self.samples_per_pixel as usize {
            return Err(invalid(format!(
                "SampleFormat has {} entries but SamplesPerPixel is {}",
                self.sample_format.len(),
                self.samples_per_pixel
            )));
        }
        for bits in &self.bits_per_sample {
            if *bits == 0 || *bits > 64 {
                return Err(invalid(format!("unsupported BitsPerSample {bits}")));
            }
        }
        match self.layout {
            Layout::Strips { rows_per_strip } => {
                if rows_per_strip == 0 {
                    return Err(invalid("RowsPerStrip must be non-zero".to_string()));
                }
                if let Compression::Jpeg { .. } = self.compression {
                    let v = self.ycbcr_subsampling.map(|s| s.1).unwrap_or(1);
                    let unit = 8 * u32::from(v.max(1));
                    if rows_per_strip % unit != 0 && rows_per_strip < self.height {
                        return Err(invalid(format!(
                            "JPEG strips need RowsPerStrip to be a multiple of {unit}, got \
                             {rows_per_strip}"
                        )));
                    }
                }
            }
            Layout::Tiles { width, length } => {
                if width == 0 || length == 0 {
                    return Err(invalid("tile dimensions must be non-zero".to_string()));
                }
                if width % 16 != 0 || length % 16 != 0 {
                    return Err(invalid(format!(
                        "TIFF 6.0 requires tile dimensions to be multiples of 16, got \
                         {width}x{length}"
                    )));
                }
            }
        }
        if let Some(map) = &self.color_map {
            let bits = self.bits_per_sample.first().copied().unwrap_or(8);
            let expected = 3usize << bits.min(16);
            if map.len() != expected {
                return Err(invalid(format!(
                    "ColorMap has {} entries, expected {expected}",
                    map.len()
                )));
            }
            if self.samples_per_pixel != 1 {
                return Err(invalid(
                    "a palette image must have exactly one sample per pixel".to_string(),
                ));
            }
        }
        if self.predictor != Predictor::None {
            // Validate per plane, exactly as `ImageWriter::encode_chunk`
            // applies the predictor: a chunky chunk carries every channel and
            // so needs one uniform depth across the whole array, while each
            // planar chunk carries one channel with a stride of 1, so planes
            // of different widths are fine. The decoder checks the same thing
            // with `ImageInfo::plane_bits`, so write and read agree.
            for plane in 0..self.plane_count() {
                crate::predictor::validate(
                    self.predictor,
                    &self.plane_bits(plane),
                    self.compression.method(),
                )?;
            }
        }
        if let Some((h, v)) = self.ycbcr_subsampling {
            if !matches!(h, 1 | 2 | 4) || !matches!(v, 1 | 2 | 4) {
                return Err(invalid(format!("illegal YCbCrSubSampling {h}x{v}")));
            }
            if self.is_subsampled() {
                let bits = self.bits_per_sample.first().copied().unwrap_or(8);
                if bits % 8 != 0 {
                    return Err(invalid(
                        "subsampled YCbCr needs whole-byte sample widths".to_string(),
                    ));
                }
            }
        }
        self.sample_type()?;
        Ok(())
    }

    /// A conservative projection of the file size, for
    /// [`VariantChoice::Auto`].
    ///
    /// Computed in closed form. Every chunk of a plane is coded at the same
    /// size except, for a strip layout, the last one, so the payload sum needs
    /// one term per plane rather than one per chunk — a loop over
    /// [`Self::chunk_count`] would run up to 2^64 times for a large tiled
    /// specification and never return.
    ///
    /// # Errors
    /// [`TiffError::IntOverflow`] when the product does not fit.
    pub fn projected_bytes(&self) -> Result<u64> {
        let mut total = 16u64; // header
        let per_plane = self.chunks_per_plane();
        if per_plane > 0 {
            for plane in 0..u64::from(self.plane_count()) {
                let base = plane.checked_mul(per_plane).ok_or(TiffError::IntOverflow)?;
                // Chunk 0 of the plane is full size; the last one may be clipped.
                let full = self.chunk_packed_len(base)? as u64 + 1;
                let last = self.chunk_packed_len(base + per_plane - 1)? as u64 + 1;
                let body = full
                    .checked_mul(per_plane - 1)
                    .and_then(|v| v.checked_add(last))
                    .ok_or(TiffError::IntOverflow)?;
                total = total.checked_add(body).ok_or(TiffError::IntOverflow)?;
            }
        }
        // IFD plus generous slack for the offset/byte-count arrays.
        let entries = 24u64 + self.extra_tags.len() as u64;
        let arrays = self
            .chunk_count()
            .checked_mul(16)
            .ok_or(TiffError::IntOverflow)?;
        total
            .checked_add(entries * 20 + arrays + 4096)
            .ok_or(TiffError::IntOverflow)
    }
}

/// A TIFF encoder over any `Write + Seek` sink.
///
/// The sink must be positioned at the start of the file: TIFF offsets are
/// absolute.
#[derive(Debug)]
pub struct Encoder<W: Write + Seek> {
    writer: EndianWriter<W>,
    choice: VariantChoice,
    variant: Option<Variant>,
    registry: Option<CodecRegistry>,
    link_field: Option<u64>,
    pages: usize,
    finished: bool,
}

impl<W: Write + Seek> Encoder<W> {
    /// A little-endian classic-TIFF encoder.
    ///
    /// # Errors
    /// Propagates the sink's seek failure.
    pub fn new(writer: W) -> Result<Self> {
        Ok(Self {
            writer: EndianWriter::new(writer, Endian::Little)?,
            choice: VariantChoice::Classic,
            variant: None,
            registry: None,
            link_field: None,
            pages: 0,
            finished: false,
        })
    }

    /// Chooses the byte order. Valid only before the first page is written.
    #[must_use]
    pub fn with_endian(mut self, endian: Endian) -> Self {
        if self.variant.is_none() {
            self.writer.set_endian(endian);
        }
        self
    }

    /// Chooses classic, BigTIFF or automatic promotion.
    #[must_use]
    pub fn with_variant(mut self, choice: VariantChoice) -> Self {
        if self.variant.is_none() {
            self.choice = choice;
        }
        self
    }

    /// Registers out-of-tree codecs.
    #[must_use]
    pub fn with_codecs(mut self, registry: CodecRegistry) -> Self {
        self.registry = Some(registry);
        self
    }

    /// The byte order pages are written in.
    #[must_use]
    pub fn endian(&self) -> Endian {
        self.writer.endian()
    }

    /// The container shape, once it has been resolved.
    #[must_use]
    pub fn variant(&self) -> Option<Variant> {
        self.variant
    }

    /// Number of pages written so far.
    #[must_use]
    pub fn page_count(&self) -> usize {
        self.pages
    }

    /// Begins a page, returning a writer that must be `finish`ed.
    ///
    /// # Errors
    /// [`UsageError::InvalidSpec`] for a bad specification, plus I/O failures.
    pub fn new_image(&mut self, spec: &ImageSpec) -> Result<ImageWriter<'_, W>> {
        spec.validate()?;
        self.ensure_header(spec)?;
        ImageWriter::new(self, spec.clone())
    }

    /// Writes a whole page from one native-endian sample buffer.
    ///
    /// # Errors
    /// [`UsageError::BufferTooSmall`] when `data` is shorter than
    /// [`ImageSpec::image_native_len`], plus every codec and I/O failure.
    pub fn write_image(&mut self, spec: &ImageSpec, data: &[u8]) -> Result<()> {
        let mut page = self.new_image(spec)?;
        page.write_whole_image(data)?;
        page.finish()
    }

    /// Writes a whole page from typed samples.
    ///
    /// # Errors
    /// The same set as [`Self::write_image`].
    pub fn write_image_samples(
        &mut self,
        spec: &ImageSpec,
        samples: &crate::sample::Samples,
    ) -> Result<()> {
        let bytes = samples.to_native_bytes();
        self.write_image(spec, &bytes)
    }

    /// Flushes and returns the sink.
    ///
    /// # Errors
    /// I/O failures.
    pub fn finish(mut self) -> Result<W> {
        if self.variant.is_none() {
            // No page was written: emit an empty but valid header.
            let header = Header {
                endian: self.writer.endian(),
                variant: match self.choice {
                    VariantChoice::Big => Variant::Big,
                    _ => Variant::Classic,
                },
                first_ifd: 0,
            };
            let (bytes, len) = header.to_bytes();
            self.writer.write_bytes(bytes.get(..len).unwrap_or(&[]))?;
            self.variant = Some(header.variant);
        }
        self.finished = true;
        self.writer.into_inner()
    }

    /// Writes the file header once the variant is known.
    fn ensure_header(&mut self, spec: &ImageSpec) -> Result<()> {
        if self.variant.is_some() {
            return Ok(());
        }
        let variant = match self.choice {
            VariantChoice::Classic => Variant::Classic,
            VariantChoice::Big => Variant::Big,
            VariantChoice::Auto => {
                if spec.projected_bytes()? > 0xFFFF_0000 {
                    Variant::Big
                } else {
                    Variant::Classic
                }
            }
        };
        let header = Header {
            endian: self.writer.endian(),
            variant,
            first_ifd: 0,
        };
        let (bytes, len) = header.to_bytes();
        self.writer.write_bytes(bytes.get(..len).unwrap_or(&[]))?;
        self.link_field = Some(header.first_ifd_field_offset());
        self.variant = Some(variant);
        Ok(())
    }

    pub(crate) fn writer_mut(&mut self) -> &mut EndianWriter<W> {
        &mut self.writer
    }

    pub(crate) fn resolved_variant(&self) -> Variant {
        self.variant.unwrap_or(Variant::Classic)
    }

    pub(crate) fn registry(&self) -> Option<&CodecRegistry> {
        self.registry.as_ref()
    }

    pub(crate) fn link_directory(&mut self, written: WrittenDirectory) -> Result<()> {
        let big = self.resolved_variant().is_big();
        if let Some(field) = self.link_field {
            self.writer.patch_offset_at(field, written.offset, big)?;
        }
        self.link_field = Some(written.next_field);
        self.pages += 1;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compression_maps_to_the_registered_tag_values() {
        assert_eq!(Compression::None.method(), CompressionMethod::None);
        assert_eq!(Compression::PackBits.method(), CompressionMethod::PackBits);
        assert_eq!(Compression::Lzw.method(), CompressionMethod::Lzw);
        // 8, not 32946: libtiff, GDAL and `tifffile` all write the Adobe
        // registration, and both values decode identically here.
        assert_eq!(
            Compression::Deflate { level: 6 }.method(),
            CompressionMethod::AdobeDeflate8
        );
        assert_eq!(CompressionMethod::AdobeDeflate8.to_u16(), 8);
        assert_eq!(CompressionMethod::Deflate.to_u16(), 32946);
        assert_eq!(
            Compression::Zstd { level: 3 }.method(),
            CompressionMethod::Zstd
        );
        assert_eq!(
            Compression::Lzma { preset: 6 }.method(),
            CompressionMethod::Lzma
        );
        assert_eq!(Compression::CcittRle.method(), CompressionMethod::CcittRle);
        assert_eq!(
            Compression::CcittGroup3 {
                two_dimensional: true,
                byte_align_eol: true
            }
            .method(),
            CompressionMethod::CcittFax3
        );
        assert_eq!(
            Compression::CcittGroup4.method(),
            CompressionMethod::CcittFax4
        );
        assert_eq!(
            Compression::Jpeg {
                quality: 75,
                shared_tables: true
            }
            .method(),
            CompressionMethod::Jpeg
        );
        assert_eq!(Compression::default(), Compression::None);
    }

    #[test]
    fn compression_levels_and_fax_options() {
        assert_eq!(Compression::None.level(), CodecLevel::Default);
        assert_eq!(
            Compression::Deflate { level: 9 }.level(),
            CodecLevel::Level(9)
        );
        assert_eq!(
            Compression::Zstd { level: -1 }.level(),
            CodecLevel::Level(-1)
        );
        assert_eq!(
            Compression::CcittGroup3 {
                two_dimensional: true,
                byte_align_eol: false
            }
            .t4_options(),
            1
        );
        assert_eq!(
            Compression::CcittGroup3 {
                two_dimensional: true,
                byte_align_eol: true
            }
            .t4_options(),
            5
        );
        assert_eq!(Compression::CcittGroup4.t4_options(), 0);
    }

    #[test]
    fn default_strip_height_targets_eight_kilobytes() {
        let spec = ImageSpec::new(1024, 1024, ColorType::Rgb(8));
        match spec.layout {
            Layout::Strips { rows_per_strip } => assert_eq!(rows_per_strip, 2),
            other => panic!("expected strips, got {other:?}"),
        }
        let tiny = ImageSpec::new(4, 4, ColorType::Gray(8));
        match tiny.layout {
            Layout::Strips { rows_per_strip } => assert_eq!(rows_per_strip, 4),
            other => panic!("expected strips, got {other:?}"),
        }
    }

    #[test]
    fn spec_geometry_matches_the_reader_side_rules() {
        let spec = ImageSpec::new(10, 10, ColorType::Gray(8))
            .with_layout(Layout::Strips { rows_per_strip: 4 });
        assert_eq!(spec.chunk_count(), 3);
        assert_eq!(spec.chunk_coded_dimensions(0), (10, 4));
        assert_eq!(spec.chunk_coded_dimensions(2), (10, 2));
        assert_eq!(spec.chunk_origin(2), (0, 8));
        assert_eq!(spec.chunk_native_len(0).expect("len"), 40);

        let tiled = ImageSpec::new(20, 20, ColorType::Gray(8)).with_layout(Layout::Tiles {
            width: 16,
            length: 16,
        });
        assert_eq!(tiled.chunk_count(), 4);
        assert_eq!(tiled.chunk_coded_dimensions(3), (16, 16));
        assert_eq!(tiled.chunk_data_dimensions(3), (4, 4));
        assert_eq!(tiled.chunk_origin(3), (16, 16));
        assert_eq!(tiled.chunk_packed_len(0).expect("len"), 256);

        let planar = ImageSpec::new(4, 4, ColorType::Rgb(8))
            .with_planar(PlanarConfiguration::Planar)
            .with_layout(Layout::Strips { rows_per_strip: 4 });
        assert_eq!(planar.plane_count(), 3);
        assert_eq!(planar.chunk_count(), 3);
        assert_eq!(planar.chunk_plane(2), 2);
        assert_eq!(planar.chunk_samples_per_pixel(), 1);
        assert_eq!(planar.chunk_packed_len(0).expect("len"), 16);
    }

    #[test]
    fn validation_rejects_the_documented_mistakes() {
        assert!(ImageSpec::new(0, 4, ColorType::Gray(8)).validate().is_err());
        assert!(ImageSpec::new(4, 0, ColorType::Gray(8)).validate().is_err());
        assert!(
            ImageSpec::new(4, 4, ColorType::Gray(8))
                .with_layout(Layout::Strips { rows_per_strip: 0 })
                .validate()
                .is_err()
        );
        assert!(
            ImageSpec::new(32, 32, ColorType::Gray(8))
                .with_layout(Layout::Tiles {
                    width: 20,
                    length: 16
                })
                .validate()
                .is_err()
        );
        assert!(
            ImageSpec::new(32, 32, ColorType::Gray(8))
                .with_layout(Layout::Tiles {
                    width: 16,
                    length: 16
                })
                .validate()
                .is_ok()
        );
        assert!(
            ImageSpec::new(4, 4, ColorType::Gray(12))
                .with_predictor(Predictor::Horizontal)
                .validate()
                .is_err()
        );
        assert!(
            ImageSpec::new(4, 4, ColorType::Gray(8))
                .with_color_map(vec![0; 4])
                .validate()
                .is_err()
        );
        assert!(
            ImageSpec::new(4, 4, ColorType::Gray(8))
                .with_bits_per_sample(vec![0])
                .validate()
                .is_err()
        );
        assert!(
            ImageSpec::new(4, 4, ColorType::YCbCr(8))
                .with_ycbcr_subsampling(3, 2)
                .validate()
                .is_err()
        );
    }

    #[test]
    fn jpeg_strips_must_be_a_multiple_of_the_mcu_height() {
        let spec = ImageSpec::new(64, 64, ColorType::YCbCr(8))
            .with_compression(Compression::Jpeg {
                quality: 75,
                shared_tables: true,
            })
            .with_ycbcr_subsampling(2, 2)
            .with_layout(Layout::Strips { rows_per_strip: 8 });
        assert!(spec.validate().is_err());
        let spec = spec.with_layout(Layout::Strips { rows_per_strip: 16 });
        assert!(spec.validate().is_ok());
    }

    #[test]
    fn projection_grows_with_the_image() {
        let small = ImageSpec::new(16, 16, ColorType::Gray(8));
        let large = ImageSpec::new(4096, 4096, ColorType::Rgb(16));
        assert!(small.projected_bytes().expect("small") < large.projected_bytes().expect("large"));
        assert!(large.projected_bytes().expect("large") > 4096 * 4096 * 6);
    }

    #[test]
    fn projection_is_closed_form_not_a_loop_over_every_chunk() {
        // The exact sum for a clipped last strip: three full strips of
        // 10 x 4 x 1 byte plus one of 10 x 2, each charged one padding byte.
        let strips = ImageSpec::new(10, 14, ColorType::Gray(8))
            .with_layout(Layout::Strips { rows_per_strip: 4 });
        let payload = 3 * (40 + 1) + (20 + 1);
        // header + 24 built-in entries * 20 bytes + 4 chunks * 16 + slack
        let overhead = 16 + 24 * 20 + 4 * 16 + 4096;
        assert_eq!(
            strips.projected_bytes().expect("strips"),
            payload + overhead
        );

        // Planar planes are summed per plane, not per chunk.
        let planar = ImageSpec::new(10, 14, ColorType::Rgb(8))
            .with_planar(PlanarConfiguration::Planar)
            .with_layout(Layout::Strips { rows_per_strip: 4 });
        assert_eq!(
            planar.projected_bytes().expect("planar"),
            3 * payload + 16 + 24 * 20 + 12 * 16 + 4096
        );

        // A specification whose chunk count is astronomically large must
        // return promptly instead of looping 2^56 times. (Before this was
        // closed form, `VariantChoice::Auto` hung here forever.)
        let huge =
            ImageSpec::new(u32::MAX, u32::MAX, ColorType::Gray(8)).with_layout(Layout::Tiles {
                width: 16,
                length: 16,
            });
        assert_eq!(huge.chunk_count(), 268_435_456u64 * 268_435_456);
        assert!(matches!(
            huge.projected_bytes(),
            Err(TiffError::IntOverflow)
        ));

        // And it must not be reachable as a hang through the encoder either.
        let mut buffer = std::io::Cursor::new(Vec::new());
        let mut encoder = Encoder::new(&mut buffer)
            .expect("encoder")
            .with_variant(VariantChoice::Auto);
        assert!(encoder.new_image(&huge).is_err());
    }

    #[test]
    fn builders_thread_every_field_through() {
        let spec = ImageSpec::new(8, 8, ColorType::Gray(8))
            .with_bits_per_sample(vec![16, 16])
            .with_sample_format(SampleFormat::Int)
            .with_photometric(PhotometricInterpretation::BlackIsZero)
            .with_planar(PlanarConfiguration::Planar)
            .with_compression(Compression::PackBits)
            .with_predictor(Predictor::Horizontal)
            .with_fill_order(FillOrder::Lsb2Msb)
            .with_extra_samples(vec![ExtraSamples::UnassociatedAlpha])
            .with_resolution(
                Rational { num: 72, den: 1 },
                Rational { num: 72, den: 1 },
                ResolutionUnit::Inch,
            )
            .with_extra_tag(700, Value::Byte(vec![1, 2, 3]))
            .with_extra_tags(vec![(34675, Value::Undefined(vec![9]))]);
        assert_eq!(spec.samples_per_pixel, 2);
        assert_eq!(spec.bits_per_sample, vec![16, 16]);
        assert_eq!(spec.sample_format, vec![SampleFormat::Int; 2]);
        assert_eq!(spec.fill_order, FillOrder::Lsb2Msb);
        assert_eq!(spec.extra_tags.len(), 2);
        assert_eq!(spec.sample_type().expect("type"), SampleType::I16);
        assert!(spec.validate().is_ok());
    }
}
