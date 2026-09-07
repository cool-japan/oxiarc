//! The small value types of the image layer.
//!
//! [`ChunkType`], [`ChunkGeometry`], [`ColorType`], [`Rect`] and
//! [`ImageLayout`] describe *shape*; [`super::ImageInfo`] is what turns a
//! directory into them.
//!
//! ```
//! use oxiarc_tiff::image::{ChunkGeometry, ChunkType, ColorType, Rect};
//!
//! assert_eq!(ColorType::Cmyk(8).samples_per_pixel(), 4);
//! assert_eq!(ColorType::GrayA(16).bit_depth(), 16);
//!
//! let strips = ChunkGeometry::Strips {
//!     rows_per_strip: 4,
//!     offsets: vec![8, 40],
//!     byte_counts: vec![32, 32],
//! };
//! assert_eq!(strips.offsets(), &[8, 40]);
//! assert_eq!(strips.byte_counts(), &[32, 32]);
//! assert_eq!(strips.chunk_type(), ChunkType::Strip);
//!
//! let rect = Rect::new(1, 2, 3, 4);
//! assert_eq!((rect.x, rect.y, rect.width, rect.height), (1, 2, 3, 4));
//! ```

use crate::sample::SampleType;
use crate::tags::{ExtraSamples, PhotometricInterpretation};

/// Whether an image is stored in strips or in tiles.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ChunkType {
    /// Row-band strips (`StripOffsets` / `StripByteCounts`).
    Strip,
    /// Rectangular tiles (`TileOffsets` / `TileByteCounts`).
    Tile,
}

impl core::fmt::Display for ChunkType {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Strip => f.write_str("strip"),
            Self::Tile => f.write_str("tile"),
        }
    }
}

/// Strip or tile layout with the per-chunk offsets and byte counts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChunkGeometry {
    /// Strips, each `rows_per_strip` rows tall except possibly the last.
    Strips {
        /// Rows per strip; `u32::MAX` when `RowsPerStrip` is absent.
        rows_per_strip: u32,
        /// File offset of each strip.
        offsets: Vec<u64>,
        /// Compressed length of each strip.
        byte_counts: Vec<u64>,
    },
    /// Tiles, always coded at the full declared size and zero padded.
    Tiles {
        /// Tile width in pixels.
        tile_width: u32,
        /// Tile height in pixels.
        tile_length: u32,
        /// File offset of each tile.
        offsets: Vec<u64>,
        /// Compressed length of each tile.
        byte_counts: Vec<u64>,
    },
}

impl ChunkGeometry {
    /// Whether this geometry is strips or tiles.
    #[must_use]
    pub const fn chunk_type(&self) -> ChunkType {
        match self {
            Self::Strips { .. } => ChunkType::Strip,
            Self::Tiles { .. } => ChunkType::Tile,
        }
    }

    /// The per-chunk file offsets.
    #[must_use]
    pub fn offsets(&self) -> &[u64] {
        match self {
            Self::Strips { offsets, .. } | Self::Tiles { offsets, .. } => offsets,
        }
    }

    /// The per-chunk compressed lengths.
    #[must_use]
    pub fn byte_counts(&self) -> &[u64] {
        match self {
            Self::Strips { byte_counts, .. } | Self::Tiles { byte_counts, .. } => byte_counts,
        }
    }
}

/// A coarse description of the decoded pixel layout.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ColorType {
    /// One greyscale channel of the given bit depth.
    Gray(u8),
    /// Greyscale plus alpha.
    GrayA(u8),
    /// Palette indices of the given bit depth.
    Palette(u8),
    /// Red, green, blue.
    Rgb(u8),
    /// Red, green, blue, alpha.
    Rgba(u8),
    /// Cyan, magenta, yellow, black.
    Cmyk(u8),
    /// Cyan, magenta, yellow, black, alpha.
    CmykA(u8),
    /// Luma plus two chroma channels.
    YCbCr(u8),
    /// CIE L*a*b* (or one of its ICC/ITU flavours).
    Lab(u8),
    /// Anything else: `num_samples` channels of `bit_depth` bits.
    Multiband {
        /// Bits per channel.
        bit_depth: u8,
        /// Number of channels.
        num_samples: u16,
    },
}

impl ColorType {
    /// Channels per pixel.
    #[must_use]
    pub const fn samples_per_pixel(self) -> u16 {
        match self {
            Self::Gray(_) | Self::Palette(_) => 1,
            Self::GrayA(_) => 2,
            Self::Rgb(_) | Self::YCbCr(_) | Self::Lab(_) => 3,
            Self::Rgba(_) | Self::Cmyk(_) => 4,
            Self::CmykA(_) => 5,
            Self::Multiband { num_samples, .. } => num_samples,
        }
    }

    /// Bits per channel.
    #[must_use]
    pub const fn bit_depth(self) -> u8 {
        match self {
            Self::Gray(b)
            | Self::GrayA(b)
            | Self::Palette(b)
            | Self::Rgb(b)
            | Self::Rgba(b)
            | Self::Cmyk(b)
            | Self::CmykA(b)
            | Self::YCbCr(b)
            | Self::Lab(b) => b,
            Self::Multiband { bit_depth, .. } => bit_depth,
        }
    }

    /// The photometric interpretation a writer should record for this layout.
    #[must_use]
    pub const fn photometric(self) -> PhotometricInterpretation {
        match self {
            Self::Gray(_) | Self::GrayA(_) | Self::Multiband { .. } => {
                PhotometricInterpretation::BlackIsZero
            }
            Self::Palette(_) => PhotometricInterpretation::Palette,
            Self::Rgb(_) | Self::Rgba(_) => PhotometricInterpretation::Rgb,
            Self::Cmyk(_) | Self::CmykA(_) => PhotometricInterpretation::Separated,
            Self::YCbCr(_) => PhotometricInterpretation::YCbCr,
            Self::Lab(_) => PhotometricInterpretation::CieLab,
        }
    }

    /// The `ExtraSamples` a writer should record for this layout.
    #[must_use]
    pub fn extra_samples(self) -> Vec<ExtraSamples> {
        match self {
            Self::GrayA(_) | Self::Rgba(_) | Self::CmykA(_) => {
                vec![ExtraSamples::UnassociatedAlpha]
            }
            _ => Vec::new(),
        }
    }
}

/// A rectangle in image coordinates.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Rect {
    /// Left edge.
    pub x: u32,
    /// Top edge.
    pub y: u32,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
}

impl Rect {
    /// A rectangle from its four coordinates.
    #[must_use]
    pub const fn new(x: u32, y: u32, width: u32, height: u32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    /// Number of pixels the rectangle covers.
    #[must_use]
    pub const fn area(self) -> u64 {
        self.width as u64 * self.height as u64
    }

    /// `true` when the rectangle fits inside a `w * h` image.
    #[must_use]
    pub const fn fits_in(self, w: u32, h: u32) -> bool {
        match (
            self.x.checked_add(self.width),
            self.y.checked_add(self.height),
        ) {
            (Some(right), Some(bottom)) => right <= w && bottom <= h,
            _ => false,
        }
    }
}

/// The shape of a decoded buffer handed back to the caller.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ImageLayout {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Native slot type of every sample.
    pub sample_type: SampleType,
    /// Channels per pixel.
    pub samples_per_pixel: u16,
    /// Bytes between the starts of two consecutive rows.
    pub row_stride: usize,
    /// Number of planes; 1 for interleaved output.
    pub planes: usize,
    /// Bytes between the starts of two planes; 0 when `planes == 1`.
    pub plane_stride: usize,
    /// Total size of the buffer in bytes.
    pub total_len: usize,
}
