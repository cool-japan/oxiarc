//! `tiff`-0.11-shaped tag types.
//!
//! Independent of [`crate::tags`] (the native tag surface), because upstream
//! names some tags differently from the TIFF 6.0 field name this crate uses
//! elsewhere -- tag 34675 is [`Tag::IccProfile`] here (matching `tiff`) but
//! [`crate::Tag::InterColorProfile`] natively (matching the spec's own field
//! name) -- so a plain type alias would silently rename the one tag `image`
//! 0.25.10 actually names by variant (`Tag::IccProfile`, at
//! `image-0.25.10/src/codecs/tiff.rs:313`). Every conversion goes through
//! the raw `u16` tag number, which is standardised and cannot drift between
//! crates the way a variant spelling can.

use std::fmt;

macro_rules! compat_tag_enum {
    ($(($variant:ident, $value:expr, $doc:literal)),+ $(,)?) => {
        /// A TIFF tag, named where this list recognises it.
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
        #[non_exhaustive]
        pub enum Tag {
            $(
                #[doc = $doc]
                $variant,
            )+
            /// A tag this list does not name, carried through by its raw
            /// number.
            Unknown(u16),
        }

        impl Tag {
            /// The raw tag number.
            #[must_use]
            pub const fn to_u16(self) -> u16 {
                match self {
                    $( Self::$variant => $value, )+
                    Self::Unknown(value) => value,
                }
            }

            /// Names a raw tag number, falling back to [`Tag::Unknown`].
            #[must_use]
            pub const fn from_u16(value: u16) -> Self {
                match value {
                    $( $value => Self::$variant, )+
                    other => Self::Unknown(other),
                }
            }

            /// Identical to [`Tag::from_u16`] -- upstream distinguishes the
            /// two only because its `from_u16` used to return an `Option`;
            /// this crate's never did, so both names resolve the same way.
            #[must_use]
            pub const fn from_u16_exhaustive(value: u16) -> Self {
                Self::from_u16(value)
            }
        }

        impl fmt::Display for Tag {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                match self {
                    $( Self::$variant => write!(f, "{}", stringify!($variant)), )+
                    Self::Unknown(value) => write!(f, "Unknown({value})"),
                }
            }
        }
    };
}

compat_tag_enum! {
    (NewSubfileType, 254, "Bit field describing the kind of data in this subfile."),
    (SubfileType, 255, "Deprecated predecessor of `NewSubfileType`."),
    (ImageWidth, 256, "Number of columns."),
    (ImageLength, 257, "Number of rows."),
    (BitsPerSample, 258, "Bits per sample, one entry per channel."),
    (Compression, 259, "The compression scheme."),
    (PhotometricInterpretation, 262, "The colour space and sample meaning."),
    (Threshholding, 263, "How the data was thresholded, if at all."),
    (CellWidth, 264, "Width of the dithering matrix (deprecated)."),
    (CellLength, 265, "Height of the dithering matrix (deprecated)."),
    (FillOrder, 266, "Bit order within a byte."),
    (ImageDescription, 270, "A free-text description of the image."),
    (Make, 271, "The scanner manufacturer."),
    (Model, 272, "The scanner model."),
    (StripOffsets, 273, "Byte offset of each strip."),
    (Orientation, 274, "Orientation of the image with respect to rows and columns."),
    (SamplesPerPixel, 277, "Channels per pixel."),
    (RowsPerStrip, 278, "Rows in each strip."),
    (StripByteCounts, 279, "Byte count of each strip."),
    (MinSampleValue, 280, "The minimum sample value."),
    (MaxSampleValue, 281, "The maximum sample value."),
    (XResolution, 282, "Pixels per resolution unit, horizontally."),
    (YResolution, 283, "Pixels per resolution unit, vertically."),
    (PlanarConfiguration, 284, "Chunky or planar sample storage."),
    (PageName, 285, "The name of the page."),
    (XPosition, 286, "X offset of this page within a larger picture."),
    (YPosition, 287, "Y offset of this page within a larger picture."),
    (FreeOffsets, 288, "Byte offset of free-space blocks (rarely used)."),
    (FreeByteCounts, 289, "Byte count of free-space blocks (rarely used)."),
    (GrayResponseUnit, 290, "Precision of the gray-response curve."),
    (GrayResponseCurve, 291, "The gray-response curve."),
    (T4Options, 292, "Group 3 fax options."),
    (T6Options, 293, "Group 4 fax options."),
    (ResolutionUnit, 296, "The unit `XResolution`/`YResolution` are given in."),
    (PageNumber, 297, "Page number and total page count."),
    (TransferFunction, 301, "The transfer function for the image."),
    (Software, 305, "The software that created the image."),
    (DateTime, 306, "Date and time of image creation."),
    (Artist, 315, "The person who created the image."),
    (HostComputer, 316, "The computer that created the image."),
    (Predictor, 317, "The mathematical predictor applied before compression."),
    (WhitePoint, 318, "Chromaticity of the white point."),
    (PrimaryChromaticities, 319, "Chromaticities of the primaries."),
    (ColorMap, 320, "The palette, for `PhotometricInterpretation::Palette`."),
    (HalftoneHints, 321, "Halftone gray-level ranges."),
    (TileWidth, 322, "Tile width."),
    (TileLength, 323, "Tile height."),
    (TileOffsets, 324, "Byte offset of each tile."),
    (TileByteCounts, 325, "Byte count of each tile."),
    (SubIfds, 330, "Offsets to child IFDs."),
    (InkSet, 332, "The set of inks used in a separated (`CMYK`-like) image."),
    (InkNames, 333, "The names of the inks."),
    (NumberOfInks, 334, "The number of inks."),
    (DotRange, 336, "The dot-range for halftoning."),
    (TargetPrinter, 337, "The name of the target printer."),
    (ExtraSamples, 338, "The interpretation of channels past `PhotometricInterpretation`'s own."),
    (SampleFormat, 339, "The numeric format of each sample."),
    (SMinSampleValue, 340, "The minimum sample value, in `SampleFormat`'s type."),
    (SMaxSampleValue, 341, "The maximum sample value, in `SampleFormat`'s type."),
    (JpegTables, 347, "A shared JPEG abbreviated table-specification datastream."),
    (YCbCrCoefficients, 529, "The RGB to YCbCr transform matrix."),
    (YCbCrSubSampling, 530, "Horizontal and vertical chroma subsampling factors."),
    (YCbCrPositioning, 531, "Whether chroma samples are centred or co-sited."),
    (ReferenceBlackWhite, 532, "The reference black and white points."),
    (Copyright, 33432, "A copyright notice."),
    (IccProfile, 34675, "An embedded ICC colour profile."),
    (ExifIfd, 34665, "Offset to the Exif IFD."),
    (GpsIfd, 34853, "Offset to the GPS IFD."),
    (Xmp, 700, "An embedded XMP metadata packet."),
}

macro_rules! compat_raw_enum {
    ($name:ident, $doc:literal { $($variant:ident = $value:expr, $vdoc:literal;)+ }) => {
        #[doc = $doc]
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        #[non_exhaustive]
        // Variant names deliberately match the upstream `tiff` crate's exact
        // spelling (`RGB`, `CMYK`, `IEEEFP`, ...) rather than this crate's
        // usual Rust-idiomatic casing, because this module's entire purpose
        // is byte-for-byte name compatibility for a drop-in migration.
        #[allow(clippy::upper_case_acronyms)]
        pub enum $name {
            $(
                #[doc = $vdoc]
                $variant,
            )+
            /// A value this list does not name.
            Unknown(u16),
        }

        impl $name {
            /// The raw tag value.
            #[must_use]
            pub const fn to_u16(self) -> u16 {
                match self {
                    $( Self::$variant => $value, )+
                    Self::Unknown(value) => value,
                }
            }

            /// Names a raw value, falling back to `Unknown`.
            #[must_use]
            pub const fn from_u16(value: u16) -> Self {
                match value {
                    $( $value => Self::$variant, )+
                    other => Self::Unknown(other),
                }
            }
        }
    };
}

compat_raw_enum!(CompressionMethod, "The `Compression` (259) tag's value." {
    None = 1, "Uncompressed.";
    Huffman = 2, "CCITT modified Huffman RLE.";
    Fax3 = 3, "CCITT Group 3 fax.";
    Fax4 = 4, "CCITT Group 4 fax.";
    Lzw = 5, "LZW.";
    Jpeg = 6, "Obsolete pre-TTN2 JPEG.";
    ModernJpeg = 7, "JPEG (TTN2).";
    Deflate = 8, "Deflate, Adobe registration.";
    OldDeflate = 0x80B2, "Deflate, the older private registration.";
    PackBits = 0x8005, "Apple PackBits.";
});

compat_raw_enum!(PhotometricInterpretation, "The `PhotometricInterpretation` (262) tag's value." {
    WhiteIsZero = 0, "Bilevel/greyscale where 0 is white.";
    BlackIsZero = 1, "Bilevel/greyscale where 0 is black.";
    RGB = 2, "Full-colour RGB.";
    RGBPalette = 3, "Palette colour, expanded through `ColorMap`.";
    TransparencyMask = 4, "A 1-bit mask for another image.";
    CMYK = 5, "Separated inks, CMYK by default.";
    YCbCr = 6, "Luma plus two chroma channels.";
    CIELab = 8, "1976 CIE L*a*b*.";
});

compat_raw_enum!(PlanarConfiguration, "The `PlanarConfiguration` (284) tag's value." {
    Chunky = 1, "Samples interleaved within a pixel.";
    Planar = 2, "One plane per sample.";
});

compat_raw_enum!(ResolutionUnit, "The `ResolutionUnit` (296) tag's value." {
    None = 1, "No absolute unit (an aspect ratio only).";
    Inch = 2, "Pixels per inch.";
    Centimeter = 3, "Pixels per centimetre.";
});

compat_raw_enum!(SampleFormat, "The `SampleFormat` (339) tag's value." {
    Uint = 1, "Unsigned integer.";
    Int = 2, "Two's-complement signed integer.";
    IEEEFP = 3, "IEEE-754 floating point.";
    Void = 4, "Undefined bit layout.";
});

compat_raw_enum!(ExtraSamples, "One `ExtraSamples` (338) list entry." {
    Unspecified = 0, "An unspecified extra channel.";
    AssociatedAlpha = 1, "Alpha, premultiplied into the colour channels.";
    UnassociatedAlpha = 2, "Alpha, independent of the colour channels.";
});

/// [`crate::tags::Predictor`] (317), re-exported under this module too: the
/// real `tiff` crate has one `Predictor` enum shared between `tags` and
/// `encoder`, so both paths resolve to the same type here as well.
pub type Predictor = crate::tags::Predictor;

/// A raw tag offset into another IFD (`SubIFDs`, `ExifIFD`, `GPSIFD`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct IfdPointer(pub u64);

/// A tag value that has not yet been interpreted as a specific Rust type.
///
/// Native equivalent: [`crate::Value`]. This alias keeps the compat surface
/// self-contained without duplicating the (large) value-decoding logic.
pub type ValueBuffer = crate::ifd::Value;

/// The byte order a file was written in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ByteOrder {
    /// Intel, little-endian.
    LittleEndian,
    /// Motorola, big-endian.
    BigEndian,
}

impl ByteOrder {
    /// The byte order of the host this code is running on.
    #[must_use]
    pub const fn native() -> Self {
        if cfg!(target_endian = "big") {
            Self::BigEndian
        } else {
            Self::LittleEndian
        }
    }

    pub(crate) const fn from_native(endian: crate::byteorder::Endian) -> Self {
        match endian {
            crate::byteorder::Endian::Little => Self::LittleEndian,
            crate::byteorder::Endian::Big => Self::BigEndian,
        }
    }

    /// The equivalent native [`crate::byteorder::Endian`].
    #[must_use]
    pub const fn to_native(self) -> crate::byteorder::Endian {
        match self {
            Self::LittleEndian => crate::byteorder::Endian::Little,
            Self::BigEndian => crate::byteorder::Endian::Big,
        }
    }
}

impl Tag {
    pub(crate) const fn to_native(self) -> crate::tags::Tag {
        crate::tags::Tag::from_u16(self.to_u16())
    }

    pub(crate) const fn from_native(tag: crate::tags::Tag) -> Self {
        Self::from_u16(tag.to_u16())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icc_profile_and_orientation_are_named_as_image_expects() {
        assert_eq!(Tag::IccProfile.to_u16(), 34675);
        assert_eq!(Tag::Orientation.to_u16(), 274);
        assert_eq!(Tag::from_u16(34675), Tag::IccProfile);
        assert_eq!(Tag::from_u16(274), Tag::Orientation);
    }

    #[test]
    fn unknown_tags_round_trip_through_their_number() {
        assert_eq!(Tag::from_u16(60000), Tag::Unknown(60000));
        assert_eq!(Tag::Unknown(60000).to_u16(), 60000);
    }

    #[test]
    fn compression_method_matches_the_registered_values() {
        assert_eq!(CompressionMethod::from_u16(5), CompressionMethod::Lzw);
        assert_eq!(
            CompressionMethod::from_u16(7),
            CompressionMethod::ModernJpeg
        );
        assert_eq!(
            CompressionMethod::from_u16(9999),
            CompressionMethod::Unknown(9999)
        );
    }

    #[test]
    fn byte_order_round_trips_through_the_native_type() {
        assert_eq!(
            ByteOrder::from_native(crate::byteorder::Endian::Little),
            ByteOrder::LittleEndian
        );
        assert_eq!(
            ByteOrder::LittleEndian.to_native(),
            crate::byteorder::Endian::Little
        );
    }

    #[test]
    fn tag_translates_through_the_native_tag_type_by_number() {
        assert_eq!(
            Tag::IccProfile.to_native(),
            crate::tags::Tag::InterColorProfile
        );
        assert_eq!(
            Tag::from_native(crate::tags::Tag::Orientation),
            Tag::Orientation
        );
    }
}
