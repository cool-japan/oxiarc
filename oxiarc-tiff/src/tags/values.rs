//! Field types and the enumerated values of the tags that drive decoding.
//!
//! Every enum here has an `Unknown(u16)` arm: TIFF is an extensible format and
//! a reader that rejects an unregistered value is a reader that cannot open
//! half the files in the wild. Values are carried through, and only the code
//! that actually *needs* to understand a value refuses to proceed.
//!
//! ```
//! use oxiarc_tiff::tags::{CompressionMethod, PhotometricInterpretation, Type};
//!
//! assert_eq!(Type::from_u16(16), Some(Type::Long8));
//! assert_eq!(Type::Long8.byte_len(), 8);
//! assert_eq!(CompressionMethod::from_u16(32773), CompressionMethod::PackBits);
//! assert_eq!(
//!     PhotometricInterpretation::from_u16(9),
//!     PhotometricInterpretation::IccLab
//! );
//! ```

use crate::error::{Result, TiffError};

/// The 16 assigned IFD field types (codes 1..=13 and 16..=18).
///
/// Codes 14 and 15 have never been assigned; they surface as
/// [`crate::ifd::Value::Unknown`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(u16)]
pub enum Type {
    /// 8-bit unsigned integer.
    Byte = 1,
    /// NUL-terminated 7-bit ASCII.
    Ascii = 2,
    /// 16-bit unsigned integer.
    Short = 3,
    /// 32-bit unsigned integer.
    Long = 4,
    /// Two `LONG`s: numerator then denominator.
    Rational = 5,
    /// 8-bit signed integer.
    SByte = 6,
    /// An 8-bit byte with a tag-defined meaning.
    Undefined = 7,
    /// 16-bit signed integer.
    SShort = 8,
    /// 32-bit signed integer.
    SLong = 9,
    /// Two `SLONG`s: numerator then denominator.
    SRational = 10,
    /// IEEE-754 binary32.
    Float = 11,
    /// IEEE-754 binary64.
    Double = 12,
    /// A 32-bit IFD offset (TIFF/EP, tag 330).
    Ifd = 13,
    /// 64-bit unsigned integer (BigTIFF).
    Long8 = 16,
    /// 64-bit signed integer (BigTIFF).
    SLong8 = 17,
    /// A 64-bit IFD offset (BigTIFF).
    Ifd8 = 18,
}

impl Type {
    /// Bytes occupied by one value of this type.
    #[must_use]
    pub const fn byte_len(self) -> u64 {
        match self {
            Self::Byte | Self::Ascii | Self::SByte | Self::Undefined => 1,
            Self::Short | Self::SShort => 2,
            Self::Long | Self::SLong | Self::Float | Self::Ifd => 4,
            Self::Rational
            | Self::SRational
            | Self::Double
            | Self::Long8
            | Self::SLong8
            | Self::Ifd8 => 8,
        }
    }

    /// Recognises an on-disk type code.
    #[must_use]
    pub const fn from_u16(value: u16) -> Option<Self> {
        match value {
            1 => Some(Self::Byte),
            2 => Some(Self::Ascii),
            3 => Some(Self::Short),
            4 => Some(Self::Long),
            5 => Some(Self::Rational),
            6 => Some(Self::SByte),
            7 => Some(Self::Undefined),
            8 => Some(Self::SShort),
            9 => Some(Self::SLong),
            10 => Some(Self::SRational),
            11 => Some(Self::Float),
            12 => Some(Self::Double),
            13 => Some(Self::Ifd),
            16 => Some(Self::Long8),
            17 => Some(Self::SLong8),
            18 => Some(Self::Ifd8),
            _ => None,
        }
    }

    /// The on-disk type code.
    #[must_use]
    pub const fn to_u16(self) -> u16 {
        self as u16
    }

    /// `count * byte_len()`, with an overflow guard.
    ///
    /// # Errors
    /// [`TiffError::IntOverflow`] when the product does not fit in a `u64`.
    pub const fn value_bytes(self, count: u64) -> Result<u64> {
        match count.checked_mul(self.byte_len()) {
            Some(bytes) => Ok(bytes),
            None => Err(TiffError::IntOverflow),
        }
    }

    /// `true` for the types that hold a nested-IFD offset.
    #[must_use]
    pub const fn is_offset_like(self) -> bool {
        matches!(self, Self::Ifd | Self::Ifd8 | Self::Long | Self::Long8)
    }

    /// `true` for the types that can be widened losslessly to `u64`.
    #[must_use]
    pub const fn is_unsigned_int(self) -> bool {
        matches!(
            self,
            Self::Byte | Self::Short | Self::Long | Self::Long8 | Self::Ifd | Self::Ifd8
        )
    }

    /// A short human name (`"SHORT"`, `"LONG8"`, ...).
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Byte => "BYTE",
            Self::Ascii => "ASCII",
            Self::Short => "SHORT",
            Self::Long => "LONG",
            Self::Rational => "RATIONAL",
            Self::SByte => "SBYTE",
            Self::Undefined => "UNDEFINED",
            Self::SShort => "SSHORT",
            Self::SLong => "SLONG",
            Self::SRational => "SRATIONAL",
            Self::Float => "FLOAT",
            Self::Double => "DOUBLE",
            Self::Ifd => "IFD",
            Self::Long8 => "LONG8",
            Self::SLong8 => "SLONG8",
            Self::Ifd8 => "IFD8",
        }
    }
}

impl core::fmt::Display for Type {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{} ({})", self.name(), self.to_u16())
    }
}

/// Declares an enumerated tag value with a raw `Unknown` fallback.
macro_rules! raw_enum {
    (
        $(#[$outer:meta])*
        $name:ident, $unknown_doc:literal {
            $( $variant:ident = $value:literal , $doc:literal );* $(;)?
        }
    ) => {
        $(#[$outer])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        #[non_exhaustive]
        pub enum $name {
            $(
                #[doc = concat!($doc, " (value ", stringify!($value), ").")]
                $variant,
            )*
            #[doc = $unknown_doc]
            Unknown(u16),
        }

        impl $name {
            /// Maps an on-disk value onto this enum, retaining unknown values.
            #[must_use]
            pub const fn from_u16(value: u16) -> Self {
                match value {
                    $( $value => Self::$variant, )*
                    other => Self::Unknown(other),
                }
            }

            /// The on-disk value.
            #[must_use]
            pub const fn to_u16(self) -> u16 {
                match self {
                    $( Self::$variant => $value, )*
                    Self::Unknown(value) => value,
                }
            }

            /// A short human name for diagnostics.
            #[must_use]
            pub const fn name(self) -> &'static str {
                match self {
                    $( Self::$variant => stringify!($variant), )*
                    Self::Unknown(_) => "Unknown",
                }
            }

            /// `true` when the value was not one this crate knows.
            #[must_use]
            pub const fn is_unknown(self) -> bool {
                matches!(self, Self::Unknown(_))
            }
        }

        impl core::fmt::Display for $name {
            fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
                write!(f, "{} ({})", self.name(), self.to_u16())
            }
        }
    };
}

raw_enum! {
    /// Compression tag (259) values.
    ///
    /// Every value registered by TIFF 6.0, TIFF Technical Notes, the Adobe
    /// registry and the de-facto extensions has a variant so the dispatch can
    /// name it in an error rather than falling through a wildcard.
    CompressionMethod, "A compression value outside every registry this crate knows." {
        None = 1, "Uncompressed";
        CcittRle = 2, "CCITT modified Huffman RLE";
        CcittFax3 = 3, "CCITT Group 3 fax (T.4)";
        CcittFax4 = 4, "CCITT Group 4 fax (T.6)";
        Lzw = 5, "LZW";
        OldJpeg = 6, "Obsolete pre-TTN2 JPEG";
        Jpeg = 7, "JPEG (TTN2)";
        AdobeDeflate8 = 8, "Deflate, as registered by Adobe";
        JbigTiffEp = 9, "JBIG, black and white (TIFF/EP)";
        JbigTiffEpColor = 10, "JBIG, colour (TIFF/EP)";
        Next = 32766, "NeXT 2-bit RLE";
        CcittRleWord = 32771, "CCITT RLE with word-aligned rows";
        PackBits = 32773, "Apple PackBits";
        ThunderScan = 32809, "ThunderScan 4-bit RLE";
        It8Ctpad = 32895, "IT8 CT with padding";
        It8Lw = 32896, "IT8 linework RLE";
        It8Mp = 32897, "IT8 monochrome picture";
        It8Bl = 32898, "IT8 binary line art";
        PixarFilm = 32908, "Pixar 10-bit LZW";
        PixarLog = 32909, "Pixar 11-bit ZIP";
        Deflate = 32946, "Deflate (the de-facto value)";
        Dcs = 32947, "Kodak DCS";
        Jbig = 34661, "JBIG";
        SgiLog = 34676, "SGI log luminance RLE";
        SgiLog24 = 34677, "SGI log 24-bit packed";
        Jpeg2000 = 34712, "JPEG 2000";
        NikonNef = 34713, "Nikon NEF compressed";
        Jbig2 = 34715, "JBIG2";
        Lzma = 34925, "LZMA2, framed as a complete .xz stream per chunk";
        Zstd = 50000, "Zstandard";
        Webp = 50001, "WebP";
        JpegXl = 50002, "JPEG XL";
        LercOld = 34887, "ESRI LERC";
        Lerc = 34892, "ESRI LERC (registered value)";
    }
}

impl CompressionMethod {
    /// `true` when this crate can decode the method with the current features.
    ///
    /// Track G0 ships `None` and `PackBits`; the remaining registered codecs
    /// return [`crate::UnsupportedError::NotYetAvailable`] from the dispatch
    /// until their modules land.
    #[must_use]
    pub const fn is_available(self) -> bool {
        matches!(self, Self::None | Self::PackBits)
    }

    /// `true` for the codecs whose decoders are scheduled but not yet wired up.
    #[must_use]
    pub const fn is_scheduled(self) -> bool {
        matches!(
            self,
            Self::CcittRle
                | Self::CcittFax3
                | Self::CcittFax4
                | Self::Lzw
                | Self::OldJpeg
                | Self::Jpeg
                | Self::AdobeDeflate8
                | Self::Deflate
                | Self::Lzma
                | Self::Zstd
        )
    }

    /// `true` when a [`Predictor`] other than [`Predictor::None`] is defined
    /// for this compression.
    ///
    /// Horizontal and floating-point differencing are pre-compression
    /// transforms; combining them with a codec that has its own spatial model
    /// (JPEG, CCITT, WebP) is a spec violation rather than a no-op.
    #[must_use]
    pub const fn allows_predictor(self) -> bool {
        matches!(
            self,
            Self::None
                | Self::Lzw
                | Self::AdobeDeflate8
                | Self::Deflate
                | Self::PackBits
                | Self::Zstd
                | Self::Lzma
        )
    }
}

raw_enum! {
    /// PhotometricInterpretation tag (262) values.
    PhotometricInterpretation, "A photometric value outside the registry." {
        WhiteIsZero = 0, "Bilevel/greyscale where 0 is white";
        BlackIsZero = 1, "Bilevel/greyscale where 0 is black";
        Rgb = 2, "Full-colour RGB";
        Palette = 3, "Palette colour, expanded through ColorMap";
        TransparencyMask = 4, "A 1-bit mask for another image";
        Separated = 5, "Separated inks, CMYK by default";
        YCbCr = 6, "Luma plus two chroma channels";
        CieLab = 8, "1976 CIE L*a*b*, signed a*/b*";
        IccLab = 9, "ICC-flavoured L*a*b*, biased a*/b*";
        ItuLab = 10, "ITU-flavoured L*a*b*";
        ColorFilterArray = 32803, "Raw colour-filter-array data (DNG)";
        LinearRaw = 34892, "Demosaiced linear raw (DNG)";
    }
}

impl PhotometricInterpretation {
    /// The number of colour channels this interpretation implies, if fixed.
    #[must_use]
    pub const fn implied_samples(self) -> Option<u16> {
        match self {
            Self::WhiteIsZero
            | Self::BlackIsZero
            | Self::Palette
            | Self::TransparencyMask
            | Self::ColorFilterArray => Some(1),
            Self::Rgb | Self::YCbCr | Self::CieLab | Self::IccLab | Self::ItuLab => Some(3),
            Self::Separated => Some(4),
            Self::LinearRaw | Self::Unknown(_) => None,
        }
    }
}

raw_enum! {
    /// PlanarConfiguration tag (284) values.
    PlanarConfiguration, "A planar configuration outside the registry." {
        Chunky = 1, "All channels of a pixel are stored together";
        Planar = 2, "Each channel is stored in its own plane of chunks";
    }
}

raw_enum! {
    /// Predictor tag (317) values.
    Predictor, "A predictor outside the registry." {
        None = 1, "No prediction";
        Horizontal = 2, "Horizontal differencing over whole samples";
        FloatingPoint = 3, "Byte-plane transpose plus byte-wise differencing";
    }
}

raw_enum! {
    /// ResolutionUnit tag (296) values.
    ResolutionUnit, "A resolution unit outside the registry." {
        None = 1, "No absolute unit";
        Inch = 2, "Inches";
        Centimeter = 3, "Centimetres";
    }
}

raw_enum! {
    /// SampleFormat tag (339) values.
    SampleFormat, "A sample format outside the registry." {
        Uint = 1, "Unsigned integer";
        Int = 2, "Two's-complement signed integer";
        IeeeFp = 3, "IEEE-754 floating point";
        Void = 4, "Undefined data format";
        ComplexInt = 5, "Complex integer";
        ComplexIeeeFp = 6, "Complex IEEE floating point";
    }
}

raw_enum! {
    /// FillOrder tag (266) values.
    FillOrder, "A fill order outside the registry." {
        Msb2Lsb = 1, "The high-order bit of each byte comes first";
        Lsb2Msb = 2, "The low-order bit of each byte comes first";
    }
}

raw_enum! {
    /// Orientation tag (274) values.
    ///
    /// Exposed on [`crate::ImageInfo`] and never applied automatically.
    Orientation, "An orientation outside the registry." {
        TopLeft = 1, "Row 0 top, column 0 left";
        TopRight = 2, "Row 0 top, column 0 right (mirrored)";
        BottomRight = 3, "Row 0 bottom, column 0 right (rotated 180 degrees)";
        BottomLeft = 4, "Row 0 bottom, column 0 left (flipped vertically)";
        LeftTop = 5, "Row 0 left, column 0 top (transposed)";
        RightTop = 6, "Row 0 right, column 0 top (rotated 90 degrees clockwise)";
        RightBottom = 7, "Row 0 right, column 0 bottom (transverse)";
        LeftBottom = 8, "Row 0 left, column 0 bottom (rotated 270 degrees clockwise)";
    }
}

raw_enum! {
    /// ExtraSamples tag (338) values.
    ExtraSamples, "An extra-sample kind outside the registry." {
        Unspecified = 0, "Unspecified extra data";
        AssociatedAlpha = 1, "Premultiplied alpha";
        UnassociatedAlpha = 2, "Straight (non-premultiplied) alpha";
    }
}

raw_enum! {
    /// YCbCrPositioning tag (531) values.
    YCbCrPositioning, "A chroma positioning outside the registry." {
        Centered = 1, "Chroma samples sit at the centre of their block";
        Cosited = 2, "Chroma samples are co-sited with the first luma sample";
    }
}

raw_enum! {
    /// InkSet tag (332) values.
    InkSet, "An ink set outside the registry." {
        Cmyk = 1, "The four process inks in CMYK order";
        NotCmyk = 2, "Some other ink set, described by InkNames";
    }
}

raw_enum! {
    /// Threshholding tag (263) values.
    Threshholding, "A threshholding mode outside the registry." {
        NoDithering = 1, "No dithering or halftoning was applied";
        OrderedDither = 2, "An ordered dither or halftone was applied";
        Randomized = 3, "A randomised process was applied";
    }
}

raw_enum! {
    /// CleanFaxData tag (327) values.
    CleanFaxData, "A clean-fax value outside the registry." {
        Clean = 0, "No bad lines";
        Regenerated = 1, "Bad lines were regenerated by the receiver";
        Unclean = 2, "Bad lines exist and were not regenerated";
    }
}

raw_enum! {
    /// SubfileType tag (255) values — the deprecated predecessor of tag 254.
    SubfileType, "A subfile type outside the registry." {
        FullResolution = 1, "Full-resolution image data";
        ReducedResolution = 2, "Reduced-resolution image data";
        SinglePage = 3, "A single page of a multi-page image";
    }
}

/// NewSubfileType tag (254): a 32-bit bit field.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct NewSubfileType(pub u32);

impl NewSubfileType {
    /// Bit 0: this is a reduced-resolution version of another image.
    pub const REDUCED_RESOLUTION: u32 = 1;
    /// Bit 1: this is one page of a multi-page image.
    pub const PAGE: u32 = 2;
    /// Bit 2: this is a transparency mask for another image.
    pub const TRANSPARENCY_MASK: u32 = 4;

    /// Wraps a raw bit field.
    #[must_use]
    pub const fn from_u32(bits: u32) -> Self {
        Self(bits)
    }

    /// The raw bit field.
    #[must_use]
    pub const fn to_u32(self) -> u32 {
        self.0
    }

    /// `true` when bit 0 is set.
    #[must_use]
    pub const fn is_reduced_resolution(self) -> bool {
        self.0 & Self::REDUCED_RESOLUTION != 0
    }

    /// `true` when bit 1 is set.
    #[must_use]
    pub const fn is_page(self) -> bool {
        self.0 & Self::PAGE != 0
    }

    /// `true` when bit 2 is set.
    #[must_use]
    pub const fn is_transparency_mask(self) -> bool {
        self.0 & Self::TRANSPARENCY_MASK != 0
    }
}

/// T4Options tag (292): the CCITT Group 3 option bit field.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct T4Options(pub u32);

impl T4Options {
    /// Bit 0: two-dimensional coding is permitted.
    pub const TWO_DIMENSIONAL: u32 = 1;
    /// Bit 1: uncompressed mode is permitted.
    pub const UNCOMPRESSED: u32 = 2;
    /// Bit 2: EOL codes are byte-aligned with fill bits.
    pub const BYTE_ALIGNED_EOL: u32 = 4;

    /// Wraps a raw bit field.
    #[must_use]
    pub const fn from_u32(bits: u32) -> Self {
        Self(bits)
    }

    /// The raw bit field.
    #[must_use]
    pub const fn to_u32(self) -> u32 {
        self.0
    }

    /// `true` when 2D coding is permitted.
    #[must_use]
    pub const fn two_dimensional(self) -> bool {
        self.0 & Self::TWO_DIMENSIONAL != 0
    }

    /// `true` when uncompressed mode is permitted.
    #[must_use]
    pub const fn uncompressed(self) -> bool {
        self.0 & Self::UNCOMPRESSED != 0
    }

    /// `true` when EOL codes are byte-aligned.
    #[must_use]
    pub const fn byte_aligned_eol(self) -> bool {
        self.0 & Self::BYTE_ALIGNED_EOL != 0
    }
}

/// T6Options tag (293): the CCITT Group 4 option bit field.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct T6Options(pub u32);

impl T6Options {
    /// Bit 1: uncompressed mode is permitted.
    pub const UNCOMPRESSED: u32 = 2;

    /// Wraps a raw bit field.
    #[must_use]
    pub const fn from_u32(bits: u32) -> Self {
        Self(bits)
    }

    /// The raw bit field.
    #[must_use]
    pub const fn to_u32(self) -> u32 {
        self.0
    }

    /// `true` when uncompressed mode is permitted.
    #[must_use]
    pub const fn uncompressed(self) -> bool {
        self.0 & Self::UNCOMPRESSED != 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_assigned_type_code_round_trips() {
        for code in [1u16, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 16, 17, 18] {
            let ty = Type::from_u16(code).expect("assigned code");
            assert_eq!(ty.to_u16(), code);
            assert!(!ty.name().is_empty());
        }
        assert_eq!(Type::from_u16(0), None);
        assert_eq!(Type::from_u16(14), None);
        assert_eq!(Type::from_u16(15), None);
        assert_eq!(Type::from_u16(19), None);
    }

    #[test]
    fn type_byte_lengths_match_the_spec() {
        let expected: [(Type, u64); 16] = [
            (Type::Byte, 1),
            (Type::Ascii, 1),
            (Type::Short, 2),
            (Type::Long, 4),
            (Type::Rational, 8),
            (Type::SByte, 1),
            (Type::Undefined, 1),
            (Type::SShort, 2),
            (Type::SLong, 4),
            (Type::SRational, 8),
            (Type::Float, 4),
            (Type::Double, 8),
            (Type::Ifd, 4),
            (Type::Long8, 8),
            (Type::SLong8, 8),
            (Type::Ifd8, 8),
        ];
        for (ty, len) in expected {
            assert_eq!(ty.byte_len(), len, "{ty}");
        }
    }

    #[test]
    fn value_bytes_guards_overflow() {
        assert_eq!(Type::Short.value_bytes(3).expect("small"), 6);
        let err = Type::Double
            .value_bytes(u64::MAX / 4)
            .expect_err("overflowing count");
        assert!(matches!(err, TiffError::IntOverflow));
    }

    #[test]
    fn unknown_values_are_retained_not_rejected() {
        let compression = CompressionMethod::from_u16(60000);
        assert_eq!(compression, CompressionMethod::Unknown(60000));
        assert_eq!(compression.to_u16(), 60000);
        assert!(compression.is_unknown());
        assert!(!compression.is_available());
        assert!(!compression.is_scheduled());
        let photometric = PhotometricInterpretation::from_u16(999);
        assert!(photometric.is_unknown());
        assert_eq!(photometric.implied_samples(), None);
    }

    #[test]
    fn every_registered_compression_value_round_trips() {
        let values = [
            1u16, 2, 3, 4, 5, 6, 7, 8, 9, 10, 32766, 32771, 32773, 32809, 32895, 32896, 32897,
            32898, 32908, 32909, 32946, 32947, 34661, 34676, 34677, 34712, 34713, 34715, 34887,
            34892, 34925, 50000, 50001, 50002,
        ];
        for value in values {
            let method = CompressionMethod::from_u16(value);
            assert!(!method.is_unknown(), "compression {value} must be named");
            assert_eq!(method.to_u16(), value);
        }
    }

    #[test]
    fn only_none_and_packbits_are_available_in_this_track() {
        assert!(CompressionMethod::None.is_available());
        assert!(CompressionMethod::PackBits.is_available());
        for method in [
            CompressionMethod::Lzw,
            CompressionMethod::Deflate,
            CompressionMethod::AdobeDeflate8,
            CompressionMethod::Zstd,
            CompressionMethod::Lzma,
            CompressionMethod::Jpeg,
            CompressionMethod::OldJpeg,
            CompressionMethod::CcittRle,
            CompressionMethod::CcittFax3,
            CompressionMethod::CcittFax4,
        ] {
            assert!(!method.is_available(), "{method} must not claim support");
            assert!(method.is_scheduled(), "{method} must be scheduled");
        }
    }

    #[test]
    fn predictor_compatibility_follows_the_spec() {
        assert!(CompressionMethod::Lzw.allows_predictor());
        assert!(CompressionMethod::Deflate.allows_predictor());
        assert!(CompressionMethod::None.allows_predictor());
        assert!(!CompressionMethod::Jpeg.allows_predictor());
        assert!(!CompressionMethod::CcittFax4.allows_predictor());
        assert!(!CompressionMethod::Webp.allows_predictor());
    }

    #[test]
    fn photometric_channel_counts() {
        assert_eq!(PhotometricInterpretation::Rgb.implied_samples(), Some(3));
        assert_eq!(
            PhotometricInterpretation::Separated.implied_samples(),
            Some(4)
        );
        assert_eq!(
            PhotometricInterpretation::Palette.implied_samples(),
            Some(1)
        );
        assert_eq!(PhotometricInterpretation::LinearRaw.implied_samples(), None);
    }

    #[test]
    fn simple_value_enums_round_trip() {
        assert_eq!(
            PlanarConfiguration::from_u16(2),
            PlanarConfiguration::Planar
        );
        assert_eq!(Predictor::from_u16(3), Predictor::FloatingPoint);
        assert_eq!(ResolutionUnit::from_u16(3), ResolutionUnit::Centimeter);
        assert_eq!(SampleFormat::from_u16(3), SampleFormat::IeeeFp);
        assert_eq!(FillOrder::from_u16(2), FillOrder::Lsb2Msb);
        assert_eq!(Orientation::from_u16(8), Orientation::LeftBottom);
        assert_eq!(ExtraSamples::from_u16(1), ExtraSamples::AssociatedAlpha);
        assert_eq!(YCbCrPositioning::from_u16(2), YCbCrPositioning::Cosited);
        assert_eq!(InkSet::from_u16(2), InkSet::NotCmyk);
        assert_eq!(Threshholding::from_u16(3), Threshholding::Randomized);
        assert_eq!(CleanFaxData::from_u16(2), CleanFaxData::Unclean);
        assert_eq!(SubfileType::from_u16(3), SubfileType::SinglePage);
        assert_eq!(
            format!("{}", SampleFormat::IeeeFp),
            "IeeeFp (3)".to_string()
        );
    }

    #[test]
    fn bit_fields_decode_their_flags() {
        let sub = NewSubfileType::from_u32(0b101);
        assert!(sub.is_reduced_resolution());
        assert!(!sub.is_page());
        assert!(sub.is_transparency_mask());
        assert_eq!(sub.to_u32(), 5);

        let t4 = T4Options::from_u32(0b101);
        assert!(t4.two_dimensional());
        assert!(!t4.uncompressed());
        assert!(t4.byte_aligned_eol());
        assert_eq!(t4.to_u32(), 5);

        let t6 = T6Options::from_u32(2);
        assert!(t6.uncompressed());
        assert_eq!(t6.to_u32(), 2);
        assert_eq!(T6Options::default().to_u32(), 0);
    }

    #[test]
    fn type_display_names_the_code() {
        assert_eq!(format!("{}", Type::Long8), "LONG8 (16)");
    }
}
