//! [`Info`]: everything the decoder learned from a file's chunks.
//!
//! The field names and types of the `png` crate's `Info` are reproduced
//! exactly, so field reads in existing code keep compiling. Six chunk types
//! that `png` does not parse (`tIME`, `sPLT`, `hIST`, `oFFs`, `sCAL`, `pCAL`),
//! the `sTER` stereo indicator and a list of retained unknown chunks are
//! additive extensions.

use std::borrow::Cow;

use crate::chunk::ChunkType;
use crate::common::{
    AnimationControl, CodingIndependentCodePoints, ContentLightLevelInfo, FrameControl,
    MasteringDisplayColorVolume, PixelDimensions, ScaledFloat, SourceChromaticities,
    SrgbRenderingIntent, StereoLayout,
};
use crate::header::{BitDepth, BytesPerPixel, ColorType, Ihdr, Interlace};
use crate::text_metadata::{ITXtChunk, TEXtChunk, ZTXtChunk};

/// The `tIME` last-modification timestamp.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Time {
    /// Full year, e.g. 2026.
    pub year: u16,
    /// Month, 1 to 12.
    pub month: u8,
    /// Day, 1 to 31.
    pub day: u8,
    /// Hour, 0 to 23.
    pub hour: u8,
    /// Minute, 0 to 59.
    pub minute: u8,
    /// Second, 0 to 60 (60 for a leap second).
    pub second: u8,
}

/// One entry of a `sPLT` suggested palette.
///
/// Values are stored at the palette's own sample depth; an 8-bit palette
/// leaves the high byte of each channel zero.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SuggestedPaletteEntry {
    /// Red sample.
    pub red: u16,
    /// Green sample.
    pub green: u16,
    /// Blue sample.
    pub blue: u16,
    /// Alpha sample.
    pub alpha: u16,
    /// Relative frequency of the entry in the image.
    pub frequency: u16,
}

/// A `sPLT` suggested palette.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SuggestedPalette {
    /// The palette's name, 1 to 79 printable Latin-1 characters.
    pub name: String,
    /// Sample depth: 8 or 16.
    pub sample_depth: u8,
    /// The entries, in the file's order.
    pub entries: Vec<SuggestedPaletteEntry>,
}

/// The unit an `oFFs` chunk measures in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum OffsetUnit {
    /// The offset is in pixels.
    Pixel = 0,
    /// The offset is in micrometres.
    Micrometer = 1,
}

/// The `oFFs` position of the image on a page or screen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ImageOffset {
    /// Horizontal offset, which may be negative.
    pub x: i32,
    /// Vertical offset, which may be negative.
    pub y: i32,
    /// The unit both offsets are expressed in.
    pub unit: OffsetUnit,
}

/// The unit an `sCAL` chunk measures in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum ScalUnit {
    /// Metres.
    Meter = 1,
    /// Radians.
    Radian = 2,
}

/// The `sCAL` physical scale of the subject.
///
/// The two measurements are ASCII floating-point strings in the file and are
/// kept as strings here, because the format allows more precision than `f64`
/// and round-tripping matters more than arithmetic.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PhysicalScale {
    /// The unit both measurements are expressed in.
    pub unit: ScalUnit,
    /// Physical width of one pixel.
    pub width: String,
    /// Physical height of one pixel.
    pub height: String,
}

/// The `pCAL` mapping from stored sample values to physical quantities.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PixelCalibration {
    /// Calibration name, 1 to 79 printable Latin-1 characters.
    pub name: String,
    /// The original sample value that maps to the first parameter.
    pub x0: i32,
    /// The original sample value that maps to the last parameter.
    pub x1: i32,
    /// Which equation relates samples to physical values.
    pub equation_type: u8,
    /// The name of the physical unit.
    pub unit_name: String,
    /// The equation's parameters, as ASCII floating-point strings.
    pub parameters: Vec<String>,
}

/// An ancillary chunk the decoder did not recognise but kept.
///
/// The `png` crate discards these, which makes a decode-then-encode round trip
/// lossy. Retaining them is what lets an optimiser or a metadata-preserving
/// pipeline use this crate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnknownChunk {
    /// The chunk's four-byte type.
    pub kind: ChunkType,
    /// Its payload, exactly as stored.
    pub data: Vec<u8>,
    /// Whether an editor may copy this chunk into a modified file, i.e. the
    /// low bit of the fourth type byte.
    pub safe_to_copy: bool,
}

/// The Apple `CgBI` marker found in iOS-processed files.
///
/// Such files use **raw DEFLATE** image data with no zlib header, store colour
/// channels as BGR(A) rather than RGB(A), and premultiply alpha. They are not
/// conformant PNGs; this crate decodes them in lenient mode and reports what it
/// had to do here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CgbiInfo {
    /// The four raw flag bytes of the chunk.
    pub flags: [u8; 4],
    /// Whether the alpha channel is premultiplied into the colour channels.
    pub premultiplied_alpha: bool,
}

/// Everything the decoder read out of a PNG's chunks.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct Info<'a> {
    /// Image width in pixels.
    pub width: u32,
    /// Image height in pixels.
    pub height: u32,
    /// Bits per sample.
    pub bit_depth: BitDepth,
    /// How colours are stored.
    pub color_type: ColorType,
    /// Whether the image is Adam7 interlaced.
    pub interlaced: bool,
    /// The `sBIT` chunk, if present.
    pub sbit: Option<Cow<'a, [u8]>>,
    /// The `tRNS` chunk, normalised the way the `png` crate normalises it: for
    /// bit depths below 16, grayscale keys are truncated to one byte and RGB
    /// keys to three. [`Info::trns_raw`] returns the untouched bytes.
    pub trns: Option<Cow<'a, [u8]>>,
    /// The `pHYs` chunk, if present.
    pub pixel_dims: Option<PixelDimensions>,
    /// The `PLTE` chunk, three bytes per entry.
    pub palette: Option<Cow<'a, [u8]>>,
    /// The `gAMA` chunk exactly as stored. Prefer [`Info::source_gamma`], which
    /// also carries the value an `sRGB` chunk implies.
    pub gama_chunk: Option<ScaledFloat>,
    /// The `cHRM` chunk exactly as stored. Prefer
    /// [`Info::source_chromaticities`].
    pub chrm_chunk: Option<SourceChromaticities>,
    /// The `bKGD` chunk, if present.
    pub bkgd: Option<Cow<'a, [u8]>>,
    /// The `fcTL` chunk of the frame currently being decoded.
    pub frame_control: Option<FrameControl>,
    /// The `acTL` chunk, if the file is an animation.
    pub animation_control: Option<AnimationControl>,
    /// Gamma of the source system, from `gAMA` or implied by `sRGB`.
    pub source_gamma: Option<ScaledFloat>,
    /// Chromaticities of the source system, from `cHRM` or implied by `sRGB`.
    pub source_chromaticities: Option<SourceChromaticities>,
    /// The `sRGB` rendering intent. Its presence also asserts that the image
    /// is in the sRGB colour space.
    pub srgb: Option<SrgbRenderingIntent>,
    /// The decompressed `iCCP` profile.
    pub icc_profile: Option<Cow<'a, [u8]>>,
    /// The `cICP` chunk, if present.
    pub coding_independent_code_points: Option<CodingIndependentCodePoints>,
    /// The `mDCv` chunk, if present.
    pub mastering_display_color_volume: Option<MasteringDisplayColorVolume>,
    /// The `cLLi` chunk, if present.
    pub content_light_level: Option<ContentLightLevelInfo>,
    /// The `eXIf` chunk, if present.
    pub exif_metadata: Option<Cow<'a, [u8]>>,
    /// Every `tEXt` chunk, in file order.
    pub uncompressed_latin1_text: Vec<TEXtChunk>,
    /// Every `zTXt` chunk, in file order.
    pub compressed_latin1_text: Vec<ZTXtChunk>,
    /// Every `iTXt` chunk, in file order.
    pub utf8_text: Vec<ITXtChunk>,

    // ---- extensions over the `png` crate ----
    /// The `tIME` chunk, if present.
    pub time: Option<Time>,
    /// Every `sPLT` chunk, in file order.
    pub splt: Vec<SuggestedPalette>,
    /// The `hIST` chunk, one frequency per palette entry.
    pub hist: Option<Vec<u16>>,
    /// The `oFFs` chunk, if present.
    pub offs: Option<ImageOffset>,
    /// The `sCAL` chunk, if present.
    pub scal: Option<PhysicalScale>,
    /// The `pCAL` chunk, if present.
    pub pcal: Option<PixelCalibration>,
    /// The `sTER` stereo layout, if present.
    pub ster: Option<StereoLayout>,
    /// Ancillary chunks the decoder did not recognise but retained.
    pub unknown_chunks: Vec<UnknownChunk>,
    /// The Apple `CgBI` marker, when this is an iOS-processed file.
    pub cgbi: Option<CgbiInfo>,
    /// The untouched `tRNS` payload, before the `png`-compatible truncation
    /// applied to [`Info::trns`].
    pub trns_original: Option<Vec<u8>>,
}

impl Default for Info<'_> {
    fn default() -> Info<'static> {
        Info {
            width: 0,
            height: 0,
            bit_depth: BitDepth::Eight,
            color_type: ColorType::Grayscale,
            interlaced: false,
            sbit: None,
            trns: None,
            pixel_dims: None,
            palette: None,
            gama_chunk: None,
            chrm_chunk: None,
            bkgd: None,
            frame_control: None,
            animation_control: None,
            source_gamma: None,
            source_chromaticities: None,
            srgb: None,
            icc_profile: None,
            coding_independent_code_points: None,
            mastering_display_color_volume: None,
            content_light_level: None,
            exif_metadata: None,
            uncompressed_latin1_text: Vec::new(),
            compressed_latin1_text: Vec::new(),
            utf8_text: Vec::new(),
            time: None,
            splt: Vec::new(),
            hist: None,
            offs: None,
            scal: None,
            pcal: None,
            ster: None,
            unknown_chunks: Vec::new(),
            cgbi: None,
            trns_original: None,
        }
    }
}

impl Info<'_> {
    /// An otherwise-default `Info` of the given size.
    #[must_use]
    pub fn with_size(width: u32, height: u32) -> Info<'static> {
        Info {
            width,
            height,
            ..Default::default()
        }
    }

    /// Build an `Info` from a parsed image header.
    #[must_use]
    pub fn from_ihdr(ihdr: &Ihdr) -> Info<'static> {
        Info {
            width: ihdr.width,
            height: ihdr.height,
            bit_depth: ihdr.bit_depth,
            color_type: ihdr.color_type,
            interlaced: ihdr.interlace.is_interlaced(),
            ..Default::default()
        }
    }

    /// The image header these fields describe.
    #[must_use]
    pub fn ihdr(&self) -> Ihdr {
        Ihdr {
            width: self.width,
            height: self.height,
            bit_depth: self.bit_depth,
            color_type: self.color_type,
            interlace: if self.interlaced {
                Interlace::Adam7
            } else {
                Interlace::None
            },
        }
    }

    /// The image's `(width, height)`.
    #[must_use]
    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Whether the file carries an `acTL` chunk.
    #[must_use]
    pub fn is_animated(&self) -> bool {
        self.animation_control.is_some()
    }

    /// The `acTL` chunk, if any.
    #[must_use]
    pub fn animation_control(&self) -> Option<&AnimationControl> {
        self.animation_control.as_ref()
    }

    /// The current frame's `fcTL` chunk, if any.
    #[must_use]
    pub fn frame_control(&self) -> Option<&FrameControl> {
        self.frame_control.as_ref()
    }

    /// Bits occupied by one pixel.
    #[must_use]
    pub fn bits_per_pixel(&self) -> usize {
        self.color_type.samples() * usize::from(self.bit_depth as u8)
    }

    /// The filter stride, `max(1, bits_per_pixel / 8)`.
    #[must_use]
    pub fn bytes_per_pixel(&self) -> usize {
        self.bpp_in_prediction().into_usize()
    }

    /// The filter stride as the enum the kernels are specialised over.
    #[must_use]
    pub fn bpp_in_prediction(&self) -> BytesPerPixel {
        BytesPerPixel::from_color_and_depth(self.color_type, self.bit_depth)
    }

    /// The number of bytes a full scanline occupies, including the filter byte.
    #[must_use]
    pub fn raw_row_length(&self) -> usize {
        self.raw_row_length_from_width(self.width)
    }

    /// The number of bytes a scanline of `width` pixels occupies, including the
    /// filter byte.
    #[must_use]
    pub fn raw_row_length_from_width(&self, width: u32) -> usize {
        self.color_type
            .raw_row_length_from_width(self.bit_depth, width)
    }

    /// The total number of raw bytes the image data stream must produce.
    ///
    /// Saturates rather than overflowing; [`Ihdr::expected_raw_bytes`] is the
    /// checked form the decoder actually uses.
    #[must_use]
    pub fn raw_bytes(&self) -> usize {
        let bytes = self.ihdr().expected_raw_bytes().unwrap_or(u64::MAX);
        usize::try_from(bytes).unwrap_or(usize::MAX)
    }

    /// The untouched `tRNS` payload, before the `png`-compatible truncation.
    #[must_use]
    pub fn trns_raw(&self) -> Option<&[u8]> {
        self.trns_original.as_deref()
    }

    /// The number of entries in the palette, or zero when there is none.
    #[must_use]
    pub fn palette_entries(&self) -> usize {
        self.palette.as_ref().map_or(0, |p| p.len() / 3)
    }

    /// Drop every borrowed field, producing an `Info` that owns its data.
    #[must_use]
    pub fn into_owned(self) -> Info<'static> {
        Info {
            width: self.width,
            height: self.height,
            bit_depth: self.bit_depth,
            color_type: self.color_type,
            interlaced: self.interlaced,
            sbit: self.sbit.map(|c| Cow::Owned(c.into_owned())),
            trns: self.trns.map(|c| Cow::Owned(c.into_owned())),
            pixel_dims: self.pixel_dims,
            palette: self.palette.map(|c| Cow::Owned(c.into_owned())),
            gama_chunk: self.gama_chunk,
            chrm_chunk: self.chrm_chunk,
            bkgd: self.bkgd.map(|c| Cow::Owned(c.into_owned())),
            frame_control: self.frame_control,
            animation_control: self.animation_control,
            source_gamma: self.source_gamma,
            source_chromaticities: self.source_chromaticities,
            srgb: self.srgb,
            icc_profile: self.icc_profile.map(|c| Cow::Owned(c.into_owned())),
            coding_independent_code_points: self.coding_independent_code_points,
            mastering_display_color_volume: self.mastering_display_color_volume,
            content_light_level: self.content_light_level,
            exif_metadata: self.exif_metadata.map(|c| Cow::Owned(c.into_owned())),
            uncompressed_latin1_text: self.uncompressed_latin1_text,
            compressed_latin1_text: self.compressed_latin1_text,
            utf8_text: self.utf8_text,
            time: self.time,
            splt: self.splt,
            hist: self.hist,
            offs: self.offs,
            scal: self.scal,
            pcal: self.pcal,
            ster: self.ster,
            unknown_chunks: self.unknown_chunks,
            cgbi: self.cgbi,
            trns_original: self.trns_original,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_size() {
        let info = Info::with_size(4, 5);
        assert_eq!(info.size(), (4, 5));
        assert_eq!(info.bit_depth, BitDepth::Eight);
        assert_eq!(info.color_type, ColorType::Grayscale);
        assert!(!info.is_animated());
        assert!(info.frame_control().is_none());
        assert!(info.animation_control().is_none());
    }

    #[test]
    fn derived_geometry_matches_the_header() {
        let ihdr = Ihdr {
            width: 7,
            height: 3,
            bit_depth: BitDepth::Four,
            color_type: ColorType::Indexed,
            interlace: Interlace::None,
        };
        let info = Info::from_ihdr(&ihdr);
        assert_eq!(info.bits_per_pixel(), 4);
        assert_eq!(info.bytes_per_pixel(), 1);
        assert_eq!(info.bpp_in_prediction(), BytesPerPixel::One);
        assert_eq!(info.raw_row_length(), 5); // ceil(7*4/8) + 1
        assert_eq!(info.raw_bytes(), 15);
        assert_eq!(info.ihdr(), ihdr);
    }

    #[test]
    fn interlaced_raw_bytes_use_the_pass_sum() {
        let ihdr = Ihdr {
            width: 1,
            height: 1,
            bit_depth: BitDepth::Eight,
            color_type: ColorType::Grayscale,
            interlace: Interlace::Adam7,
        };
        let info = Info::from_ihdr(&ihdr);
        assert!(info.interlaced);
        assert_eq!(info.raw_bytes(), 2);
    }

    #[test]
    fn into_owned_keeps_every_field() {
        let mut info = Info::with_size(1, 1);
        info.palette = Some(Cow::Borrowed(&[1, 2, 3]));
        info.trns_original = Some(vec![9]);
        info.unknown_chunks.push(UnknownChunk {
            kind: ChunkType(*b"prVt"),
            data: vec![1, 2],
            safe_to_copy: true,
        });
        let owned = info.into_owned();
        assert_eq!(owned.palette.as_deref(), Some(&[1u8, 2, 3][..]));
        assert_eq!(owned.palette_entries(), 1);
        assert_eq!(owned.trns_raw(), Some(&[9u8][..]));
        assert_eq!(owned.unknown_chunks.len(), 1);
    }
}
