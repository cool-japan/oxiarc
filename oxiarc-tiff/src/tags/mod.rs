//! The TIFF tag namespace.
//!
//! One [`Tag`] enum covers the TIFF 6.0 baseline and extensions, the old-style
//! JPEG tags (kept so a compression-6 file can be *diagnosed*), the GeoTIFF
//! keys, the EXIF/GPS/Interoperability pointers, the metadata blob tags (XMP,
//! ICC, IPTC, Photoshop) and the DNG basics. Anything else round-trips through
//! [`Tag::Unknown`].
//!
//! ```
//! use oxiarc_tiff::tags::Tag;
//!
//! assert_eq!(Tag::from_u16(256), Tag::ImageWidth);
//! assert_eq!(Tag::ImageWidth.to_u16(), 256);
//! assert_eq!(Tag::from_u16(34735), Tag::GeoKeyDirectory);
//! assert_eq!(Tag::from_u16(60123), Tag::Unknown(60123));
//! assert_eq!(Tag::Compression.default_value(), Some(1));
//! ```

mod values;

pub use values::{
    CleanFaxData, CompressionMethod, ExtraSamples, FillOrder, InkSet, NewSubfileType, Orientation,
    PhotometricInterpretation, PlanarConfiguration, Predictor, ResolutionUnit, SampleFormat,
    SubfileType, T4Options, T6Options, Threshholding, Type, YCbCrPositioning,
};

/// Builds the [`Tag`] enum, its two conversions and its name table.
macro_rules! tag_enum {
    ( $( $variant:ident = $value:literal , $doc:literal );* $(;)? ) => {
        /// A TIFF tag number, named where this crate knows it.
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        #[non_exhaustive]
        pub enum Tag {
            $(
                #[doc = concat!("Tag ", stringify!($value), ": ", $doc, ".")]
                $variant,
            )*
            /// A tag number this crate does not name; carried through verbatim.
            Unknown(u16),
        }

        impl Tag {
            /// Names a raw tag number, retaining unknown ones.
            #[must_use]
            pub const fn from_u16(value: u16) -> Self {
                match value {
                    $( $value => Self::$variant, )*
                    other => Self::Unknown(other),
                }
            }

            /// The raw tag number.
            #[must_use]
            pub const fn to_u16(self) -> u16 {
                match self {
                    $( Self::$variant => $value, )*
                    Self::Unknown(value) => value,
                }
            }

            /// The tag's canonical name, or `"Unknown"`.
            #[must_use]
            pub const fn name(self) -> &'static str {
                match self {
                    $( Self::$variant => stringify!($variant), )*
                    Self::Unknown(_) => "Unknown",
                }
            }

            /// Every tag this crate names, in ascending numeric order.
            #[must_use]
            pub const fn all() -> &'static [Tag] {
                &[ $( Self::$variant, )* ]
            }
        }
    };
}

tag_enum! {
    NewSubfileType = 254, "Bit field describing the kind of data in this subfile";
    SubfileType = 255, "Deprecated predecessor of NewSubfileType";
    ImageWidth = 256, "Number of columns";
    ImageLength = 257, "Number of rows";
    BitsPerSample = 258, "Bit depth of each channel";
    Compression = 259, "Compression scheme of the pixel data";
    PhotometricInterpretation = 262, "Colour space of the pixel data";
    Threshholding = 263, "Halftoning technique applied to bilevel data";
    CellWidth = 264, "Width of the dithering matrix";
    CellLength = 265, "Height of the dithering matrix";
    FillOrder = 266, "Logical order of bits within a byte";
    DocumentName = 269, "Name of the document this image came from";
    ImageDescription = 270, "Free-form description (ImageJ and OME use it)";
    Make = 271, "Scanner or camera manufacturer";
    Model = 272, "Scanner or camera model";
    StripOffsets = 273, "File offset of each strip";
    Orientation = 274, "Orientation of the image with respect to rows and columns";
    SamplesPerPixel = 277, "Number of channels per pixel";
    RowsPerStrip = 278, "Number of rows in each strip";
    StripByteCounts = 279, "Compressed byte count of each strip";
    MinSampleValue = 280, "Minimum component value used";
    MaxSampleValue = 281, "Maximum component value used";
    XResolution = 282, "Pixels per ResolutionUnit in the image width";
    YResolution = 283, "Pixels per ResolutionUnit in the image height";
    PlanarConfiguration = 284, "Chunky or planar channel storage";
    PageName = 285, "Name of the page this image came from";
    XPosition = 286, "X position of the image on the page";
    YPosition = 287, "Y position of the image on the page";
    FreeOffsets = 288, "Offsets of unused space (obsolete)";
    FreeByteCounts = 289, "Sizes of unused space (obsolete)";
    GrayResponseUnit = 290, "Precision of GrayResponseCurve";
    GrayResponseCurve = 291, "Optical density of each grey level";
    T4Options = 292, "CCITT Group 3 coding options";
    T6Options = 293, "CCITT Group 4 coding options";
    ResolutionUnit = 296, "Unit of XResolution and YResolution";
    PageNumber = 297, "Page number and total page count";
    TransferFunction = 301, "Transfer function for each channel";
    Software = 305, "Name and version of the writing software";
    DateTime = 306, "Creation time, \"YYYY:MM:DD HH:MM:SS\"";
    Artist = 315, "Person who created the image";
    HostComputer = 316, "Computer or operating system used";
    Predictor = 317, "Prediction scheme applied before compression";
    WhitePoint = 318, "Chromaticity of the white point";
    PrimaryChromaticities = 319, "Chromaticities of the primaries";
    ColorMap = 320, "Palette for PhotometricInterpretation 3";
    HalftoneHints = 321, "Highlight and shadow range to preserve";
    TileWidth = 322, "Tile width in pixels";
    TileLength = 323, "Tile height in pixels";
    TileOffsets = 324, "File offset of each tile";
    TileByteCounts = 325, "Compressed byte count of each tile";
    BadFaxLines = 326, "Number of lines with incorrect pixel counts";
    CleanFaxData = 327, "Whether bad fax lines were regenerated";
    ConsecutiveBadFaxLines = 328, "Longest run of consecutive bad fax lines";
    SubIfds = 330, "Offsets of child IFDs (thumbnails, reduced resolutions)";
    InkSet = 332, "Ink set used for a separated image";
    InkNames = 333, "NUL-separated names of the inks";
    NumberOfInks = 334, "Number of inks";
    DotRange = 336, "Component values for zero and full dot coverage";
    TargetPrinter = 337, "Intended printing environment";
    ExtraSamples = 338, "Meaning of each channel beyond the photometric ones";
    SampleFormat = 339, "Numeric format of the channel data";
    SMinSampleValue = 340, "Minimum sample value, in the sample's own format";
    SMaxSampleValue = 341, "Maximum sample value, in the sample's own format";
    TransferRange = 342, "Range referenced by TransferFunction";
    ClipPath = 343, "Clipping path in Adobe path format";
    XClipPathUnits = 344, "Horizontal units of ClipPath";
    YClipPathUnits = 345, "Vertical units of ClipPath";
    Indexed = 346, "Whether the image is an indexed non-palette image";
    JpegTables = 347, "Abbreviated JPEG table-specification stream";
    OpiProxy = 351, "Whether this is an OPI proxy image";
    GlobalParametersIfd = 400, "Offset of a global-parameters IFD";
    ProfileType = 401, "Profile type of a global-parameters IFD";
    FaxProfile = 402, "Fax profile a global-parameters IFD conforms to";
    CodingMethods = 403, "Coding methods used in the file";
    VersionYear = 404, "Year of the standard version";
    ModeNumber = 405, "Mode of the standard version";
    Decode = 433, "Decode range for LogL/LogLuv data";
    ImageBaseColor = 434, "Background colour for a masked image";
    JpegProc = 512, "Old-style JPEG process (obsolete)";
    JpegInterchangeFormat = 513, "Offset of an old-style JPEG interchange stream";
    JpegInterchangeFormatLength = 514, "Length of the old-style JPEG interchange stream";
    JpegRestartInterval = 515, "Old-style JPEG restart interval";
    JpegLosslessPredictors = 517, "Old-style JPEG lossless predictor selectors";
    JpegPointTransforms = 518, "Old-style JPEG point transforms";
    JpegQTables = 519, "Offsets of the old-style JPEG quantisation tables";
    JpegDcTables = 520, "Offsets of the old-style JPEG DC Huffman tables";
    JpegAcTables = 521, "Offsets of the old-style JPEG AC Huffman tables";
    YCbCrCoefficients = 529, "Luma coefficients of the YCbCr transform";
    YCbCrSubSampling = 530, "Chroma subsampling factors";
    YCbCrPositioning = 531, "Position of chroma samples relative to luma";
    ReferenceBlackWhite = 532, "Reference black and white for each channel";
    StripRowCounts = 559, "Row counts of variable-height strips (TIFF-FX)";
    Xmp = 700, "XMP metadata packet (XMLPacket)";
    ImageId = 32781, "Full-precision image identifier";
    WangAnnotation = 32932, "Wang/Eastman annotation blob";
    CfaRepeatPatternDim = 33421, "Repeat pattern dimensions of a colour filter array";
    CfaPattern = 33422, "Colour filter array geometric pattern";
    BatteryLevel = 33423, "Battery level when the image was taken";
    Copyright = 33432, "Copyright notice";
    MdFileTag = 33445, "MetaMorph file tag";
    MdScalePixel = 33446, "MetaMorph pixel scale";
    MdColorTable = 33447, "MetaMorph colour table";
    MdLabName = 33448, "MetaMorph laboratory name";
    MdSampleInfo = 33449, "MetaMorph sample information";
    MdPrepDate = 33450, "MetaMorph preparation date";
    MdPrepTime = 33451, "MetaMorph preparation time";
    MdFileUnits = 33452, "MetaMorph file units";
    ModelPixelScale = 33550, "GeoTIFF pixel scale (three DOUBLEs)";
    IptcNaa = 33723, "IPTC/NAA (IIM) metadata blob";
    IntergraphMatrix = 33920, "Intergraph transformation matrix";
    ModelTiepoint = 33922, "GeoTIFF tie points (six DOUBLEs each)";
    Site = 34016, "IT8 site name";
    ColorSequence = 34017, "IT8 colour sequence";
    It8Header = 34018, "IT8 header";
    RasterPadding = 34019, "IT8 raster padding";
    BitsPerRunLength = 34020, "IT8 bits per run length";
    BitsPerExtendedRunLength = 34021, "IT8 bits per extended run length";
    ColorTable = 34022, "IT8 colour table";
    ImageColorIndicator = 34023, "IT8 image colour indicator";
    BackgroundColorIndicator = 34024, "IT8 background colour indicator";
    ImageColorValue = 34025, "IT8 image colour value";
    BackgroundColorValue = 34026, "IT8 background colour value";
    PixelIntensityRange = 34027, "IT8 pixel intensity range";
    TransparencyIndicator = 34028, "IT8 transparency indicator";
    ColorCharacterization = 34029, "IT8 colour characterisation";
    HcUsage = 34030, "IT8 high-contrast usage";
    ModelTransformation = 34264, "GeoTIFF 4x4 transformation matrix";
    Photoshop = 34377, "Photoshop image-resource blob";
    ExifIfd = 34665, "Offset of the EXIF sub-IFD";
    InterColorProfile = 34675, "Embedded ICC profile";
    ImageLayer = 34732, "Image layer (TIFF-FX)";
    GeoKeyDirectory = 34735, "GeoTIFF key directory (SHORTs, 4 per key)";
    GeoDoubleParams = 34736, "GeoTIFF DOUBLE parameters";
    GeoAsciiParams = 34737, "GeoTIFF ASCII parameters, pipe-separated";
    GpsIfd = 34853, "Offset of the GPS sub-IFD";
    InteroperabilityIfd = 40965, "Offset of the EXIF interoperability sub-IFD";
    GdalMetadata = 42112, "GDAL XML metadata blob";
    GdalNoData = 42113, "GDAL no-data value, as ASCII";
    OceScanjobDescription = 50215, "Oce scanjob description";
    OceApplicationSelector = 50216, "Oce application selector";
    OceIdentificationNumber = 50217, "Oce identification number";
    OceImageLogicCharacteristics = 50218, "Oce image-logic characteristics";
    DngVersion = 50706, "DNG specification version this file targets";
    DngBackwardVersion = 50707, "Oldest DNG version that can read this file";
    UniqueCameraModel = 50708, "Unique, non-localised camera model name";
    LocalizedCameraModel = 50709, "Localised camera model name";
    CfaPlaneColor = 50710, "Mapping of CFA planes to colours";
    CfaLayout = 50711, "Spatial layout of the colour filter array";
    LinearizationTable = 50712, "Lookup table applied to raw values";
    BlackLevelRepeatDim = 50713, "Repeat pattern dimensions of BlackLevel";
    BlackLevel = 50714, "Zero-light encoding level";
    BlackLevelDeltaH = 50715, "Per-column black-level offsets";
    BlackLevelDeltaV = 50716, "Per-row black-level offsets";
    WhiteLevel = 50717, "Fully saturated encoding level";
    DefaultScale = 50718, "Default scale factors for non-square pixels";
    DefaultCropOrigin = 50719, "Origin of the default crop rectangle";
    DefaultCropSize = 50720, "Size of the default crop rectangle";
    ColorMatrix1 = 50721, "XYZ-to-camera matrix for illuminant 1";
    ColorMatrix2 = 50722, "XYZ-to-camera matrix for illuminant 2";
    CameraCalibration1 = 50723, "Camera calibration matrix for illuminant 1";
    CameraCalibration2 = 50724, "Camera calibration matrix for illuminant 2";
    ReductionMatrix1 = 50725, "Dimensionality-reduction matrix for illuminant 1";
    ReductionMatrix2 = 50726, "Dimensionality-reduction matrix for illuminant 2";
    AnalogBalance = 50727, "Gain applied to each channel before white balance";
    AsShotNeutral = 50728, "As-shot white balance, as neutral coordinates";
    AsShotWhiteXY = 50729, "As-shot white balance, as xy chromaticity";
    BaselineExposure = 50730, "Baseline exposure offset in stops";
    BaselineNoise = 50731, "Baseline noise relative to the reference camera";
    BaselineSharpness = 50732, "Baseline sharpness relative to the reference camera";
    BayerGreenSplit = 50733, "Difference between the two Bayer green channels";
    LinearResponseLimit = 50734, "Fraction of the range that stays linear";
    CameraSerialNumber = 50735, "Camera serial number";
    LensInfo = 50736, "Focal lengths and apertures of the lens";
    ChromaBlurRadius = 50737, "Chroma blur radius applied to the raw data";
    AntiAliasStrength = 50738, "Strength of the anti-alias filter";
    ShadowScale = 50739, "Raw-converter shadow scaling";
    DngPrivateData = 50740, "Manufacturer-private DNG data";
    MakerNoteSafety = 50741, "Whether the EXIF MakerNote is position independent";
    CalibrationIlluminant1 = 50778, "Illuminant of the first calibration";
    CalibrationIlluminant2 = 50779, "Illuminant of the second calibration";
    BestQualityScale = 50780, "Scale factor for best-quality interpolation";
    RawDataUniqueId = 50781, "Unique identifier of the raw image data";
    OriginalRawFileName = 50827, "File name of the original raw file";
    OriginalRawFileData = 50828, "Contents of the original raw file";
    ActiveArea = 50829, "Active (non-masked) area of the sensor";
    MaskedAreas = 50830, "Fully masked areas of the sensor";
    AsShotIccProfile = 50831, "ICC profile of the as-shot rendering";
    AsShotPreProfileMatrix = 50832, "Matrix applied before AsShotICCProfile";
    CurrentIccProfile = 50833, "ICC profile of the current rendering";
    CurrentPreProfileMatrix = 50834, "Matrix applied before CurrentICCProfile";
}

impl Tag {
    /// The spec default for tags whose absence has a defined meaning.
    ///
    /// Returns the numeric default only; tags whose default is a vector (such
    /// as `BitsPerSample`) or is computed (such as `ReferenceBlackWhite`) are
    /// handled by [`crate::ImageInfo`].
    #[must_use]
    pub const fn default_value(self) -> Option<u32> {
        match self {
            Self::NewSubfileType => Some(0),
            Self::BitsPerSample => Some(1),
            Self::Compression => Some(1),
            Self::Threshholding => Some(1),
            Self::FillOrder => Some(1),
            Self::Orientation => Some(1),
            Self::SamplesPerPixel => Some(1),
            Self::RowsPerStrip => Some(u32::MAX),
            Self::MinSampleValue => Some(0),
            Self::PlanarConfiguration => Some(1),
            Self::GrayResponseUnit => Some(2),
            Self::T4Options | Self::T6Options => Some(0),
            Self::ResolutionUnit => Some(2),
            Self::Predictor => Some(1),
            Self::InkSet => Some(1),
            Self::YCbCrPositioning => Some(1),
            Self::BadFaxLines | Self::ConsecutiveBadFaxLines => Some(0),
            Self::CleanFaxData => Some(0),
            Self::Indexed => Some(0),
            _ => None,
        }
    }

    /// `true` when the tag holds an offset to another IFD.
    #[must_use]
    pub const fn is_ifd_pointer(self) -> bool {
        matches!(
            self,
            Self::SubIfds
                | Self::ExifIfd
                | Self::GpsIfd
                | Self::InteroperabilityIfd
                | Self::GlobalParametersIfd
        )
    }

    /// `true` for the six GeoTIFF tags that must round-trip byte-identically.
    #[must_use]
    pub const fn is_geotiff(self) -> bool {
        matches!(
            self,
            Self::ModelPixelScale
                | Self::ModelTiepoint
                | Self::ModelTransformation
                | Self::GeoKeyDirectory
                | Self::GeoDoubleParams
                | Self::GeoAsciiParams
        )
    }

    /// `true` for tags that carry an opaque metadata blob.
    #[must_use]
    pub const fn is_metadata_blob(self) -> bool {
        matches!(
            self,
            Self::Xmp
                | Self::InterColorProfile
                | Self::IptcNaa
                | Self::Photoshop
                | Self::GdalMetadata
        )
    }
}

impl core::fmt::Display for Tag {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Unknown(value) => write!(f, "Unknown({value})"),
            named => write!(f, "{} ({})", named.name(), named.to_u16()),
        }
    }
}

impl From<u16> for Tag {
    fn from(value: u16) -> Self {
        Self::from_u16(value)
    }
}

impl From<Tag> for u16 {
    fn from(tag: Tag) -> Self {
        tag.to_u16()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn every_named_tag_round_trips() {
        for tag in Tag::all() {
            assert_eq!(Tag::from_u16(tag.to_u16()), *tag, "{tag}");
            assert_ne!(tag.name(), "Unknown");
        }
    }

    #[test]
    fn tag_numbers_are_unique_and_ascending() {
        let mut seen = BTreeSet::new();
        let mut previous = 0u16;
        for tag in Tag::all() {
            let value = tag.to_u16();
            assert!(seen.insert(value), "duplicate tag number {value}");
            assert!(
                value > previous,
                "tag table must be ascending: {value} follows {previous}"
            );
            previous = value;
        }
    }

    #[test]
    fn the_required_baseline_tags_are_present() {
        let required = [
            254u16, 255, 256, 257, 258, 259, 262, 263, 264, 265, 266, 269, 270, 271, 272, 273, 274,
            277, 278, 279, 280, 281, 282, 283, 284, 285, 286, 287, 288, 289, 290, 291, 292, 293,
            296, 297, 301, 305, 306, 315, 316, 317, 318, 319, 320, 321, 322, 323, 324, 325, 326,
            327, 328, 330, 332, 333, 334, 336, 337, 338, 339, 340, 341, 342, 343, 344, 345, 346,
            347, 351,
        ];
        for value in required {
            assert!(
                !matches!(Tag::from_u16(value), Tag::Unknown(_)),
                "baseline tag {value} must be named"
            );
        }
    }

    #[test]
    fn extension_families_are_present() {
        // Old-style JPEG, YCbCr, GeoTIFF, EXIF/GPS/Interop, blobs, DNG.
        for value in [
            512u16, 513, 514, 515, 517, 518, 519, 520, 521, 529, 530, 531, 532, 700, 33550, 33723,
            33922, 34264, 34377, 34665, 34675, 34735, 34736, 34737, 34853, 40965, 42112, 42113,
            50706, 50707, 50708, 50709, 50710, 50711, 50712, 50713, 50714, 50717, 50718, 50719,
            50720, 50721, 50722, 50723, 50724, 50727, 50728, 50729, 50730, 50733, 50739, 50740,
            50741, 50778, 50779, 50781, 50827,
        ] {
            assert!(
                !matches!(Tag::from_u16(value), Tag::Unknown(_)),
                "extension tag {value} must be named"
            );
        }
    }

    #[test]
    fn unknown_tags_are_carried_not_lost() {
        let tag = Tag::from_u16(60123);
        assert_eq!(tag, Tag::Unknown(60123));
        assert_eq!(tag.to_u16(), 60123);
        assert_eq!(format!("{tag}"), "Unknown(60123)");
        assert_eq!(u16::from(tag), 60123);
        assert_eq!(Tag::from(60123u16), tag);
    }

    #[test]
    fn spec_defaults_match_tiff_6() {
        assert_eq!(Tag::Compression.default_value(), Some(1));
        assert_eq!(Tag::SamplesPerPixel.default_value(), Some(1));
        assert_eq!(Tag::BitsPerSample.default_value(), Some(1));
        assert_eq!(Tag::RowsPerStrip.default_value(), Some(u32::MAX));
        assert_eq!(Tag::PlanarConfiguration.default_value(), Some(1));
        assert_eq!(Tag::Predictor.default_value(), Some(1));
        assert_eq!(Tag::FillOrder.default_value(), Some(1));
        assert_eq!(Tag::ResolutionUnit.default_value(), Some(2));
        assert_eq!(Tag::PhotometricInterpretation.default_value(), None);
        assert_eq!(Tag::ImageWidth.default_value(), None);
    }

    #[test]
    fn tag_classification_helpers() {
        assert!(Tag::SubIfds.is_ifd_pointer());
        assert!(Tag::ExifIfd.is_ifd_pointer());
        assert!(Tag::GpsIfd.is_ifd_pointer());
        assert!(Tag::InteroperabilityIfd.is_ifd_pointer());
        assert!(!Tag::ImageWidth.is_ifd_pointer());
        for tag in [
            Tag::ModelPixelScale,
            Tag::ModelTiepoint,
            Tag::ModelTransformation,
            Tag::GeoKeyDirectory,
            Tag::GeoDoubleParams,
            Tag::GeoAsciiParams,
        ] {
            assert!(tag.is_geotiff(), "{tag}");
        }
        assert!(Tag::Xmp.is_metadata_blob());
        assert!(Tag::InterColorProfile.is_metadata_blob());
        assert!(Tag::IptcNaa.is_metadata_blob());
        assert!(Tag::Photoshop.is_metadata_blob());
        assert!(!Tag::ImageLength.is_metadata_blob());
    }

    #[test]
    fn display_names_the_number() {
        assert_eq!(format!("{}", Tag::StripOffsets), "StripOffsets (273)");
    }
}
