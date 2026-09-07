//! Error types for `oxiarc-tiff`.
//!
//! The crate uses one top-level [`TiffError`] with four thematic sub-errors so
//! callers can distinguish *"this file is broken"* ([`FormatError`]) from
//! *"this file is fine but we cannot decode it"* ([`UnsupportedError`]),
//! *"the caller made a mistake"* ([`UsageError`]) and *"a guard fired"*
//! ([`LimitError`]).
//!
//! ```
//! use oxiarc_tiff::{TiffError, UnsupportedError};
//!
//! let err = TiffError::from(UnsupportedError::NotYetAvailable {
//!     method: 5,
//!     name: "LZW",
//! });
//! assert!(err.to_string().contains("LZW"));
//! ```

use crate::tags::{Tag, Type};

/// Result alias used throughout this crate.
pub type Result<T> = core::result::Result<T, TiffError>;

/// Any error produced while reading or writing a TIFF file.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum TiffError {
    /// The underlying reader or writer failed.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// The byte stream is not a well-formed TIFF.
    #[error("malformed TIFF: {0}")]
    Format(#[from] FormatError),
    /// The file is well formed but uses a feature this build cannot handle.
    #[error("unsupported TIFF feature: {0}")]
    Unsupported(#[from] UnsupportedError),
    /// The caller used the API incorrectly.
    #[error("API misuse: {0}")]
    Usage(#[from] UsageError),
    /// A configured [`crate::Limits`] guard rejected the operation.
    #[error("limits exceeded: {0}")]
    Limits(#[from] LimitError),
    /// An internal integer conversion would have overflowed.
    #[error("integer conversion overflow")]
    IntOverflow,
}

impl TiffError {
    /// Returns `true` when the error was produced by a `Limits` guard.
    #[must_use]
    pub fn is_limits(&self) -> bool {
        matches!(self, Self::Limits(_))
    }

    /// Returns `true` when the error means "this build cannot do that yet".
    #[must_use]
    pub fn is_unsupported(&self) -> bool {
        matches!(self, Self::Unsupported(_))
    }
}

/// A structural defect in the byte stream.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum FormatError {
    /// The two magic bytes were neither `II` nor `MM`.
    #[error("TIFF byte-order signature not found (expected \"II\" or \"MM\")")]
    SignatureNotFound,
    /// Fewer bytes than the header requires were available.
    #[error("TIFF header too short: got {got} bytes")]
    HeaderTooShort {
        /// Number of bytes that were actually available.
        got: usize,
    },
    /// A BigTIFF header carried an unexpected offset size or reserved word.
    #[error("invalid BigTIFF header: offset_size={offset_size}, reserved={reserved}")]
    InvalidBigTiffHeader {
        /// Bytes 4..6 of the header: must be `8`.
        offset_size: u16,
        /// Bytes 6..8 of the header: must be `0`.
        reserved: u16,
    },
    /// The version word was neither 42 (classic) nor 43 (BigTIFF).
    #[error("unknown TIFF version {0} (expected 42 or 43)")]
    UnknownVersion(u16),
    /// A tag required to interpret the image is absent.
    #[error("required tag {0} not found")]
    RequiredTagNotFound(Tag),
    /// A required tag is present but carries zero values.
    #[error("required tag {0} is empty")]
    RequiredTagEmpty(Tag),
    /// A tag carries a field type it is not allowed to have.
    #[error("tag {tag} has invalid field type {ty}")]
    InvalidTypeForTag {
        /// The offending tag.
        tag: Tag,
        /// The type that was found.
        ty: Type,
    },
    /// Two tags disagree about a count that must match.
    #[error("tag {tag}: expected {expected} values, got {got}")]
    InconsistentSizes {
        /// The offending tag.
        tag: Tag,
        /// How many values were expected.
        expected: u64,
        /// How many values were present.
        got: u64,
    },
    /// A tag's out-of-line value lies (partly) outside the file.
    #[error("tag {tag}: value at offset {offset} (+{len}) is past EOF ({file_len})")]
    ValueOffsetOutOfBounds {
        /// The raw tag number.
        tag: u16,
        /// The declared value offset.
        offset: u64,
        /// The declared value length in bytes.
        len: u64,
        /// The size of the file.
        file_len: u64,
    },
    /// A strip/tile lies (partly) outside the file.
    #[error("chunk {index}: bytes {offset}..+{len} are past EOF ({file_len})")]
    ChunkOffsetOutOfBounds {
        /// Index of the offending strip or tile.
        index: u64,
        /// The declared chunk offset.
        offset: u64,
        /// The declared chunk length.
        len: u64,
        /// The size of the file.
        file_len: u64,
    },
    /// An IFD chain (or SubIFD tree) revisits an offset it already parsed.
    #[error("IFD cycle detected at offset {offset}")]
    IfdCycle {
        /// Offset of the IFD that was seen twice.
        offset: u64,
    },
    /// An IFD's entry count exceeds what the file can hold.
    #[error("IFD at {offset} declares {entries} entries, which does not fit in the file")]
    IfdTruncated {
        /// Offset of the IFD.
        offset: u64,
        /// The declared entry count.
        entries: u64,
    },
    /// `StripByteCounts` is absent and could not be recovered.
    #[error("StripByteCounts is missing and cannot be reconstructed")]
    StripByteCountsMissing,
    /// `StripOffsets` and `StripByteCounts` have different lengths.
    #[error("chunk offsets ({offsets}) and byte counts ({counts}) have different lengths")]
    ChunkCountMismatch {
        /// Number of offsets.
        offsets: usize,
        /// Number of byte counts.
        counts: usize,
    },
    /// The image declares a zero width or height.
    #[error("image has a zero dimension ({width}x{height})")]
    ZeroDimension {
        /// `ImageWidth`.
        width: u32,
        /// `ImageLength`.
        height: u32,
    },
    /// `TileWidth`/`TileLength` is zero.
    #[error("tile has a zero dimension ({width}x{length})")]
    ZeroTileDimension {
        /// `TileWidth`.
        width: u32,
        /// `TileLength`.
        length: u32,
    },
    /// `BitsPerSample` does not have `SamplesPerPixel` entries.
    #[error("BitsPerSample has {got} entries but SamplesPerPixel is {expected}")]
    InconsistentBitsPerSample {
        /// `SamplesPerPixel`.
        expected: u16,
        /// Length of `BitsPerSample`.
        got: usize,
    },
    /// `ColorMap` is not `3 * 2^BitsPerSample` entries long.
    #[error("ColorMap has {got} entries, expected {expected}")]
    ColorMapWrongLength {
        /// The required entry count.
        expected: usize,
        /// The actual entry count.
        got: usize,
    },
    /// A PackBits stream would have written past the end of the chunk.
    #[error("PackBits stream overruns the decoded chunk")]
    PackbitsOverrun,
    /// A PackBits stream ended in the middle of a run.
    #[error("PackBits stream ended unexpectedly")]
    PackbitsTruncated,
    /// A codec produced more bytes than the chunk geometry allows.
    #[error("chunk {index}: codec produced {got} bytes, expected {expected}")]
    ChunkSizeMismatch {
        /// Index of the offending chunk.
        index: u64,
        /// Bytes the geometry calls for.
        expected: usize,
        /// Bytes the codec produced.
        got: usize,
    },
    /// A codec reported a stream-level defect.
    #[error("compression {method}: {message}")]
    Codec {
        /// The TIFF compression tag value.
        method: u16,
        /// A human-readable description.
        message: String,
    },
    /// A subsampled YCbCr image has an illegal subsampling factor.
    #[error("illegal YCbCrSubSampling ({0}, {1})")]
    IllegalSubsampling(u16, u16),
    /// A JPEG frame's sampling factors disagree with `YCbCrSubSampling` (530).
    ///
    /// TTN2 makes the frame header authoritative, so this is only reported
    /// under [`crate::Leniency::Strict`].
    #[error(
        "JPEG frame is subsampled {}x{} but YCbCrSubSampling says {}x{}",
        coded.0, coded.1, declared.0, declared.1
    )]
    JpegSubsamplingMismatch {
        /// What tag 530 said (or its 2x2 default).
        declared: (u16, u16),
        /// What the `SOF` sampling factors say.
        coded: (u16, u16),
    },
}

/// The file is well formed but this build cannot decode or encode it.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum UnsupportedError {
    /// The compression tag value is not one this crate knows at all.
    #[error("compression method {0} is not supported")]
    Compression(u16),
    /// A codec that this crate *will* support is not wired up in this build.
    ///
    /// Emitted by the codec dispatch for compression values whose decoder is
    /// scheduled but not yet present. Unlike [`Self::Compression`] this names
    /// the method, so the message is actionable.
    #[error("compression {method} ({name}) is recognised but not available in this build")]
    NotYetAvailable {
        /// The TIFF compression tag value.
        method: u16,
        /// A short human name for the method (`"LZW"`, `"Deflate"`, ...).
        name: &'static str,
    },
    /// The codec exists but its cargo feature is off.
    #[error("this codec needs the `{feature}` cargo feature of oxiarc-tiff")]
    FeatureNotCompiled {
        /// The cargo feature that must be enabled.
        feature: &'static str,
    },
    /// Old-style JPEG (compression 6) could not be reconstructed.
    #[error("old-style JPEG (compression 6) cannot be decoded: {0}")]
    OldJpeg(&'static str),
    /// The photometric interpretation is not one we can convert.
    #[error("photometric interpretation {0} is not supported")]
    Photometric(u16),
    /// The declared bit depths are not decodable.
    #[error("unsupported BitsPerSample {0:?}")]
    BitsPerSample(Vec<u16>),
    /// The declared sample format is not decodable.
    #[error("unsupported SampleFormat {0}")]
    SampleFormat(u16),
    /// The predictor is undefined for the declared bit depth.
    #[error("predictor {predictor} is undefined for {bits}-bit samples")]
    PredictorForBitDepth {
        /// The predictor tag value.
        predictor: u16,
        /// The offending bit depth.
        bits: u16,
    },
    /// The predictor is meaningless for the declared compression.
    #[error("predictor {predictor} is not valid with compression {compression}")]
    PredictorForCompression {
        /// The predictor tag value.
        predictor: u16,
        /// The compression tag value.
        compression: u16,
    },
    /// The samples do not share one `SampleFormat`.
    #[error("mixed sample formats are not supported by the typed API")]
    MixedSampleFormats,
    /// The samples do not share one bit depth.
    #[error("mixed bit depths {0:?} are not supported by the typed API")]
    MixedBitDepths(Vec<u16>),
    /// The YCbCr subsampling factors are legal but unhandled.
    #[error("YCbCr subsampling {0}x{1} is not supported")]
    Subsampling(u16, u16),
    /// The ink set is not CMYK, so no RGB conversion is defined.
    #[error("InkSet {0} has no defined RGB conversion")]
    InkSet(u16),
    /// The requested conversion is not defined for this image.
    #[error("{0}")]
    Conversion(&'static str),
}

/// The caller used the API in a way that cannot work.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum UsageError {
    /// A destination buffer was too small.
    #[error("destination buffer holds {got} bytes but {needed} are required")]
    BufferTooSmall {
        /// Bytes required.
        needed: usize,
        /// Bytes provided.
        got: usize,
    },
    /// An image index was out of range.
    #[error("image index {index} is out of range ({count} images)")]
    ImageIndexOutOfRange {
        /// The requested index.
        index: usize,
        /// How many images the file has.
        count: usize,
    },
    /// A chunk index was out of range.
    #[error("chunk index {index} is out of range ({count} chunks)")]
    ChunkIndexOutOfRange {
        /// The requested index.
        index: u64,
        /// How many chunks the image has.
        count: u64,
    },
    /// A region does not lie inside the image.
    #[error("region {x},{y} {width}x{height} is outside the {image_width}x{image_height} image")]
    RegionOutOfBounds {
        /// Region origin x.
        x: u32,
        /// Region origin y.
        y: u32,
        /// Region width.
        width: u32,
        /// Region height.
        height: u32,
        /// Image width.
        image_width: u32,
        /// Image height.
        image_height: u32,
    },
    /// A classic TIFF write would need a 64-bit offset.
    #[error("classic TIFF cannot address offset {offset}; use VariantChoice::Big")]
    ClassicTiffOverflow {
        /// The offset that did not fit in a `u32`.
        offset: u64,
    },
    /// The [`crate::ImageSpec`] is internally inconsistent.
    #[error("invalid image specification: {0}")]
    InvalidSpec(String),
    /// More or fewer chunks were written than the layout requires.
    #[error("expected {expected} chunks, {got} were written")]
    ChunkCount {
        /// Number of chunks the layout requires.
        expected: u64,
        /// Number of chunks written.
        got: u64,
    },
    /// A tag value cannot be encoded (for example an over-long count).
    #[error("tag {tag} cannot be encoded: {message}")]
    UnwritableTag {
        /// The raw tag number.
        tag: u16,
        /// Why it cannot be written.
        message: String,
    },
}

/// A configured guard rejected the operation before allocating.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum LimitError {
    /// A single tag value was larger than `Limits::ifd_value_size`.
    #[error("tag {tag} value is {size} bytes, limit is {limit}")]
    IfdValueSize {
        /// The raw tag number.
        tag: u16,
        /// The declared size.
        size: u64,
        /// The configured limit.
        limit: usize,
    },
    /// An IFD declared more entries than `Limits::max_ifd_entries`.
    #[error("IFD declares {count} entries, limit is {limit}")]
    IfdEntries {
        /// The declared entry count.
        count: u64,
        /// The configured limit.
        limit: usize,
    },
    /// More IFDs were walked than `Limits::max_ifds`.
    #[error("more than {limit} IFDs")]
    IfdCount {
        /// The configured limit.
        limit: usize,
    },
    /// A decoded image would exceed `Limits::max_image_bytes`.
    #[error("decoded image would need {size} bytes, limit is {limit}")]
    ImageSize {
        /// The projected size.
        size: u64,
        /// The configured limit.
        limit: usize,
    },
    /// A scratch buffer would exceed `Limits::intermediate_buffer_size`.
    #[error("intermediate buffer of {size} bytes exceeds the {limit}-byte limit")]
    IntermediateSize {
        /// The requested size.
        size: u64,
        /// The configured limit.
        limit: usize,
    },
    /// A single decoded chunk would exceed `Limits::decoding_buffer_size`.
    #[error("decoding buffer of {size} bytes exceeds the {limit}-byte limit")]
    DecodingBufferSize {
        /// The requested size.
        size: u64,
        /// The configured limit.
        limit: usize,
    },
    /// An image declared more strips/tiles than `Limits::max_chunks`.
    #[error("image has {count} chunks, limit is {limit}")]
    ChunkCount {
        /// The declared chunk count.
        count: u64,
        /// The configured limit.
        limit: u64,
    },
    /// A dimension exceeded `Limits::max_dimension`.
    #[error("dimension {value} exceeds the limit of {limit}")]
    Dimension {
        /// The declared dimension.
        value: u64,
        /// The configured limit.
        limit: u32,
    },
    /// A file-level running total of decoded bytes was exceeded.
    #[error("cumulative decoded output of {size} bytes exceeds the {limit}-byte budget")]
    OutputBudget {
        /// The running total that tripped the guard.
        size: u64,
        /// The configured budget.
        limit: u64,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn top_level_display_wraps_the_cause() {
        let err = TiffError::from(FormatError::SignatureNotFound);
        assert!(err.to_string().starts_with("malformed TIFF:"));
        assert!(err.to_string().contains("signature"));
    }

    #[test]
    fn not_yet_available_names_the_codec() {
        let err = TiffError::from(UnsupportedError::NotYetAvailable {
            method: 32946,
            name: "Deflate",
        });
        assert!(err.is_unsupported());
        assert!(err.to_string().contains("32946"));
        assert!(err.to_string().contains("Deflate"));
    }

    #[test]
    fn limit_errors_are_recognisable() {
        let err = TiffError::from(LimitError::IfdCount { limit: 8 });
        assert!(err.is_limits());
    }

    #[test]
    fn codec_errors_name_the_method() {
        let err = TiffError::Format(FormatError::Codec {
            method: 5,
            message: "bad code".to_string(),
        });
        assert!(err.to_string().contains("compression 5"));
    }
}
