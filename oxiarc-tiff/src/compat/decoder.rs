//! `tiff`-0.11-shaped [`Decoder`] and its supporting types.
//!
//! [`Decoder<R>`] is a thin adapter over [`crate::reader::Decoder<R>`]: every
//! method here translates arguments and results, never re-implements a
//! decode step. See the [`super`] module docs for the fixed-shape rationale.

use std::io::{Read, Seek};
use std::num::NonZeroUsize;

use super::error::{TiffError, TiffFormatError, TiffResult};
use super::tags::{SampleFormat, Tag};

/// Allocation and value-size guards, `tiff`-0.11 shaped.
///
/// Defaults match upstream exactly: 256 MiB decoding buffer, 1 MiB per IFD
/// value, 128 MiB intermediate (compressed chunk) buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Cap on the fully decoded image buffer.
    pub decoding_buffer_size: usize,
    /// Cap on one out-of-line IFD value.
    pub ifd_value_size: usize,
    /// Cap on one compressed chunk buffer.
    pub intermediate_buffer_size: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            decoding_buffer_size: 256 * 1024 * 1024,
            ifd_value_size: 1024 * 1024,
            intermediate_buffer_size: 128 * 1024 * 1024,
        }
    }
}

impl Limits {
    /// No limit at all. Only for trusted input -- this is exactly the
    /// allocation-bomb surface [`crate::Limits`] exists to guard.
    #[must_use]
    pub const fn unlimited() -> Self {
        Self {
            decoding_buffer_size: usize::MAX,
            ifd_value_size: usize::MAX,
            intermediate_buffer_size: usize::MAX,
        }
    }

    pub(super) fn to_native(self) -> crate::Limits {
        crate::Limits::default()
            .with_decoding_buffer_size(self.decoding_buffer_size)
            .with_ifd_value_size(self.ifd_value_size)
            .with_intermediate_buffer_size(self.intermediate_buffer_size)
            // Upstream has no separate "total image bytes" cap distinct from
            // the decoding buffer; the native crate does (`OutputBudget`,
            // critique.md P0-8), so it is set from the same number here
            // rather than left at the native default, which could be
            // *tighter* than a caller's explicit `decoding_buffer_size` and
            // reject something upstream's `Limits` would have allowed.
            .with_max_image_bytes(self.decoding_buffer_size)
    }
}

/// Strip- or tile-chunked, `tiff`-0.11 shaped.
///
/// A plain re-export of the native type: the two shapes already coincide
/// exactly, and neither crate lets a caller construct one directly.
pub type ChunkType = crate::ChunkType;

/// The Rust type one decoded sample slot maps to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecodingSampleType {
    /// 8-bit unsigned.
    U8,
    /// 16-bit unsigned.
    U16,
    /// 32-bit unsigned.
    U32,
    /// 64-bit unsigned.
    U64,
    /// IEEE-754 binary16.
    F16,
    /// IEEE-754 binary32.
    F32,
    /// IEEE-754 binary64.
    F64,
    /// 8-bit signed.
    I8,
    /// 16-bit signed.
    I16,
    /// 32-bit signed.
    I32,
    /// 64-bit signed.
    I64,
}

impl DecodingSampleType {
    /// Translates a native [`crate::SampleType`].
    #[must_use]
    pub fn from_native(ty: crate::SampleType) -> Self {
        match ty {
            crate::SampleType::U8 => Self::U8,
            crate::SampleType::U16 => Self::U16,
            crate::SampleType::U32 => Self::U32,
            crate::SampleType::U64 => Self::U64,
            crate::SampleType::I8 => Self::I8,
            crate::SampleType::I16 => Self::I16,
            crate::SampleType::I32 => Self::I32,
            crate::SampleType::I64 => Self::I64,
            crate::SampleType::F16 => Self::F16,
            crate::SampleType::F32 => Self::F32,
            crate::SampleType::F64 => Self::F64,
        }
    }

    /// The equivalent native [`crate::SampleType`].
    #[must_use]
    pub const fn to_native(self) -> crate::SampleType {
        match self {
            Self::U8 => crate::SampleType::U8,
            Self::U16 => crate::SampleType::U16,
            Self::U32 => crate::SampleType::U32,
            Self::U64 => crate::SampleType::U64,
            Self::I8 => crate::SampleType::I8,
            Self::I16 => crate::SampleType::I16,
            Self::I32 => crate::SampleType::I32,
            Self::I64 => crate::SampleType::I64,
            Self::F16 => crate::SampleType::F16,
            Self::F32 => crate::SampleType::F32,
            Self::F64 => crate::SampleType::F64,
        }
    }
}

/// A decoded image or chunk, `tiff`-0.11 shaped: **exactly** the eleven
/// upstream variants, and deliberately **not** `#[non_exhaustive]` -- `image`
/// 0.25.10 matches this exhaustively with no wildcard arm
/// (`codecs/tiff.rs:391-447`), so this type freezes the moment a wildcard
/// would become mandatory. See `tests/compat_api.rs`.
///
/// [`Self::F16`] carries real `half::f16` values (upstream's own type),
/// unlike the native [`crate::Samples::F16`], which carries raw `u16` bit
/// patterns to avoid forcing the `half` dependency on every consumer of the
/// native API (D-2). The bit pattern is bit-for-bit identical either way --
/// `half::f16::from_bits`/`to_bits` is the whole conversion, applied once at
/// this boundary.
#[derive(Clone, Debug, PartialEq)]
pub enum DecodingResult {
    /// 8-bit unsigned samples.
    U8(Vec<u8>),
    /// 16-bit unsigned samples.
    U16(Vec<u16>),
    /// 32-bit unsigned samples.
    U32(Vec<u32>),
    /// 64-bit unsigned samples.
    U64(Vec<u64>),
    /// IEEE-754 binary16 samples.
    F16(Vec<half::f16>),
    /// IEEE-754 binary32 samples.
    F32(Vec<f32>),
    /// IEEE-754 binary64 samples.
    F64(Vec<f64>),
    /// 8-bit signed samples.
    I8(Vec<i8>),
    /// 16-bit signed samples.
    I16(Vec<i16>),
    /// 32-bit signed samples.
    I32(Vec<i32>),
    /// 64-bit signed samples.
    I64(Vec<i64>),
}

impl DecodingResult {
    /// Number of samples (not bytes) held.
    #[must_use]
    pub fn len(&self) -> usize {
        match self {
            Self::U8(v) => v.len(),
            Self::U16(v) => v.len(),
            Self::U32(v) => v.len(),
            Self::U64(v) => v.len(),
            Self::F16(v) => v.len(),
            Self::F32(v) => v.len(),
            Self::F64(v) => v.len(),
            Self::I8(v) => v.len(),
            Self::I16(v) => v.len(),
            Self::I32(v) => v.len(),
            Self::I64(v) => v.len(),
        }
    }

    /// `true` when [`Self::len`] is zero.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The sample type of this buffer.
    #[must_use]
    pub fn sample_type(&self) -> DecodingSampleType {
        match self {
            Self::U8(_) => DecodingSampleType::U8,
            Self::U16(_) => DecodingSampleType::U16,
            Self::U32(_) => DecodingSampleType::U32,
            Self::U64(_) => DecodingSampleType::U64,
            Self::F16(_) => DecodingSampleType::F16,
            Self::F32(_) => DecodingSampleType::F32,
            Self::F64(_) => DecodingSampleType::F64,
            Self::I8(_) => DecodingSampleType::I8,
            Self::I16(_) => DecodingSampleType::I16,
            Self::I32(_) => DecodingSampleType::I32,
            Self::I64(_) => DecodingSampleType::I64,
        }
    }

    /// A borrowed, mutable view of this buffer, for reuse across chunks.
    #[must_use]
    pub fn as_buffer(&mut self, start: usize) -> DecodingBuffer<'_> {
        match self {
            Self::U8(v) => DecodingBuffer::U8(&mut v[start..]),
            Self::U16(v) => DecodingBuffer::U16(&mut v[start..]),
            Self::U32(v) => DecodingBuffer::U32(&mut v[start..]),
            Self::U64(v) => DecodingBuffer::U64(&mut v[start..]),
            Self::F16(v) => DecodingBuffer::F16(&mut v[start..]),
            Self::F32(v) => DecodingBuffer::F32(&mut v[start..]),
            Self::F64(v) => DecodingBuffer::F64(&mut v[start..]),
            Self::I8(v) => DecodingBuffer::I8(&mut v[start..]),
            Self::I16(v) => DecodingBuffer::I16(&mut v[start..]),
            Self::I32(v) => DecodingBuffer::I32(&mut v[start..]),
            Self::I64(v) => DecodingBuffer::I64(&mut v[start..]),
        }
    }

    fn from_native(samples: crate::Samples) -> Self {
        match samples {
            crate::Samples::U8(v) => Self::U8(v),
            crate::Samples::U16(v) => Self::U16(v),
            crate::Samples::U32(v) => Self::U32(v),
            crate::Samples::U64(v) => Self::U64(v),
            crate::Samples::I8(v) => Self::I8(v),
            crate::Samples::I16(v) => Self::I16(v),
            crate::Samples::I32(v) => Self::I32(v),
            crate::Samples::I64(v) => Self::I64(v),
            crate::Samples::F16(bits) => {
                Self::F16(bits.into_iter().map(half::f16::from_bits).collect())
            }
            crate::Samples::F32(v) => Self::F32(v),
            crate::Samples::F64(v) => Self::F64(v),
        }
    }
}

/// A borrowed, mutable view into one [`DecodingResult`], for decoding
/// straight into a caller-owned, reused buffer.
#[derive(Debug, PartialEq)]
pub enum DecodingBuffer<'a> {
    /// 8-bit unsigned samples.
    U8(&'a mut [u8]),
    /// 16-bit unsigned samples.
    U16(&'a mut [u16]),
    /// 32-bit unsigned samples.
    U32(&'a mut [u32]),
    /// 64-bit unsigned samples.
    U64(&'a mut [u64]),
    /// IEEE-754 binary16 samples.
    F16(&'a mut [half::f16]),
    /// IEEE-754 binary32 samples.
    F32(&'a mut [f32]),
    /// IEEE-754 binary64 samples.
    F64(&'a mut [f64]),
    /// 8-bit signed samples.
    I8(&'a mut [i8]),
    /// 16-bit signed samples.
    I16(&'a mut [i16]),
    /// 32-bit signed samples.
    I32(&'a mut [i32]),
    /// 64-bit signed samples.
    I64(&'a mut [i64]),
}

impl DecodingBuffer<'_> {
    /// Bytes this buffer occupies.
    #[must_use]
    pub fn byte_len(&self) -> usize {
        match self {
            Self::U8(v) => std::mem::size_of_val(*v),
            Self::U16(v) => std::mem::size_of_val(*v),
            Self::U32(v) => std::mem::size_of_val(*v),
            Self::U64(v) => std::mem::size_of_val(*v),
            // `half::f16` is a two-byte type, same as the raw bits it wraps.
            Self::F16(v) => v.len() * 2,
            Self::F32(v) => std::mem::size_of_val(*v),
            Self::F64(v) => std::mem::size_of_val(*v),
            Self::I8(v) => std::mem::size_of_val(*v),
            Self::I16(v) => std::mem::size_of_val(*v),
            Self::I32(v) => std::mem::size_of_val(*v),
            Self::I64(v) => std::mem::size_of_val(*v),
        }
    }

    /// This buffer's samples, serialised to native-endian bytes.
    ///
    /// **Deviation from upstream** (documented, per compat's D-6 pattern):
    /// upstream's `as_bytes`/`as_bytes_mut` return a zero-copy `&[u8]`/`&mut
    /// [u8]` view, which needs an unsafe reinterpret-cast for every arm
    /// wider than a byte. This crate is `#![forbid(unsafe_code)]`
    /// throughout (critique.md T-4's "pay the one extra pass" ruling), so
    /// this returns an owned copy instead. Every real use of the upstream
    /// method (serialising a decoded buffer out) works identically either
    /// way; only a caller relying on in-place *byte-level* mutation through
    /// the returned view would need to change.
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        match self {
            Self::U8(v) => v.to_vec(),
            Self::U16(v) => v.iter().flat_map(|s| s.to_ne_bytes()).collect(),
            Self::U32(v) => v.iter().flat_map(|s| s.to_ne_bytes()).collect(),
            Self::U64(v) => v.iter().flat_map(|s| s.to_ne_bytes()).collect(),
            Self::F16(v) => v.iter().flat_map(|s| s.to_ne_bytes()).collect(),
            Self::F32(v) => v.iter().flat_map(|s| s.to_ne_bytes()).collect(),
            Self::F64(v) => v.iter().flat_map(|s| s.to_ne_bytes()).collect(),
            Self::I8(v) => v.iter().map(|s| *s as u8).collect(),
            Self::I16(v) => v.iter().flat_map(|s| s.to_ne_bytes()).collect(),
            Self::I32(v) => v.iter().flat_map(|s| s.to_ne_bytes()).collect(),
            Self::I64(v) => v.iter().flat_map(|s| s.to_ne_bytes()).collect(),
        }
    }
}

/// What [`Decoder::read_image_to_buffer`] actually produced, since a
/// caller-supplied [`DecodingResult`] may be a different shape (or too
/// small) for the image just read.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct BufferLayoutPreference {
    /// Total samples across every plane.
    pub len: usize,
    /// The tag-level numeric format the samples were decoded as.
    pub sample_format: SampleFormat,
    /// The concrete Rust type, when [`DecodingResult`] could represent it
    /// directly (always `Some` for anything this crate produces).
    pub sample_type: Option<DecodingSampleType>,
    /// Samples per image row, per plane, when the layout is regular.
    pub row_stride: Option<NonZeroUsize>,
    /// Number of planes (1 for chunky).
    pub planes: usize,
    /// Samples between the start of consecutive planes, when `planes > 1`.
    pub plane_stride: Option<NonZeroUsize>,
    /// Total samples a full, successful decode needs -- may exceed `len`
    /// when the caller's buffer was undersized.
    pub complete_len: usize,
}

/// A strip or tile, named the way upstream's `image_coding_unit_layout` /
/// `read_coding_unit_bytes` name one: by kind and index, independent of
/// whether the underlying image happens to be strip- or tile-chunked.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TiffCodingUnit {
    /// Strip `index` of a strip-chunked image.
    Strip(u32),
    /// Tile `index` of a tile-chunked image.
    Tile(u32),
}

impl TiffCodingUnit {
    /// The chunk index this coding unit names, independent of kind.
    #[must_use]
    pub fn index(self) -> u32 {
        match self {
            Self::Strip(i) | Self::Tile(i) => i,
        }
    }
}

/// A `tiff`-0.11-shaped decoder, wrapping [`crate::reader::Decoder`].
///
/// Every method translates to the native API; see the [`super`] module docs.
#[derive(Debug)]
pub struct Decoder<R: Read + Seek> {
    inner: crate::reader::Decoder<R>,
    limits: Limits,
    has_more_cached: bool,
}

impl<R: Read + Seek> Decoder<R> {
    /// Opens `r` and prepares to read the first image.
    ///
    /// # Errors
    /// The same set as [`crate::reader::Decoder::new`], translated.
    pub fn new(r: R) -> TiffResult<Decoder<R>> {
        let mut inner = crate::reader::Decoder::new(r).map_err(TiffError::from_native)?;
        let has_more_cached = inner.more_images().unwrap_or(false);
        Ok(Self {
            inner,
            limits: Limits::default(),
            has_more_cached,
        })
    }

    /// Applies allocation guards. Valid only before the first read.
    #[must_use]
    pub fn with_limits(mut self, limits: Limits) -> Decoder<R> {
        self.limits = limits;
        self.inner = self.inner.with_limits(limits.to_native());
        self
    }

    /// Width and height of the current image.
    ///
    /// # Errors
    /// [`TiffError::FormatError`] when the required tags are missing, plus
    /// I/O failures.
    pub fn dimensions(&mut self) -> TiffResult<(u32, u32)> {
        self.inner.dimensions().map_err(TiffError::from_native)
    }

    /// The colour model of the current image.
    ///
    /// # Errors
    /// The same set as [`Self::dimensions`].
    pub fn colortype(&mut self) -> TiffResult<super::ColorType> {
        self.inner
            .color_type()
            .map(super::ColorType::from_native)
            .map_err(TiffError::from_native)
    }

    /// The chunk-navigation pointer for the current image.
    #[must_use]
    pub fn ifd_pointer(&self) -> Option<super::tags::IfdPointer> {
        self.inner
            .ifd_pointer()
            .map(|p| super::tags::IfdPointer(p.get()))
    }

    /// `true` when another image follows the current one in the IFD chain.
    #[must_use]
    pub fn more_images(&self) -> bool {
        self.has_more_cached
    }

    /// Advances to the next image in the IFD chain.
    ///
    /// # Errors
    /// [`TiffError::FormatError`] when there is no next image, plus I/O
    /// failures.
    pub fn next_image(&mut self) -> TiffResult<()> {
        let advanced = self.inner.next_image().map_err(TiffError::from_native)?;
        if !advanced {
            return Err(TiffError::FormatError(TiffFormatError::Other(
                "no more images in this file".to_string(),
            )));
        }
        self.has_more_cached = self.inner.more_images().unwrap_or(false);
        Ok(())
    }

    /// Jumps directly to image `index` (0-based) in the IFD chain.
    ///
    /// # Errors
    /// The same set as [`Self::next_image`].
    pub fn seek_to_image(&mut self, index: usize) -> TiffResult<()> {
        self.inner
            .seek_to_image(index)
            .map_err(TiffError::from_native)?;
        self.has_more_cached = self.inner.more_images().unwrap_or(false);
        Ok(())
    }

    /// The byte order this file was written in.
    #[must_use]
    pub fn byte_order(&self) -> super::tags::ByteOrder {
        super::tags::ByteOrder::from_native(self.inner.endian())
    }

    /// The chunk shape (strips or tiles) of the current image.
    ///
    /// # Errors
    /// The same set as [`Self::dimensions`].
    pub fn get_chunk_type(&mut self) -> TiffResult<ChunkType> {
        self.inner.chunk_type().map_err(TiffError::from_native)
    }

    /// Number of chunks the current image is stored as, whichever kind.
    ///
    /// Upstream distinguishes `strip_count`/`tile_count` by name only; both
    /// answer the same underlying chunk count.
    ///
    /// # Errors
    /// [`TiffError::IntSizeError`] when the count does not fit a `u32`, plus
    /// the same set as [`Self::dimensions`].
    pub fn strip_count(&mut self) -> TiffResult<u32> {
        self.chunk_count_u32()
    }

    /// See [`Self::strip_count`].
    ///
    /// # Errors
    /// The same set as [`Self::strip_count`].
    pub fn tile_count(&mut self) -> TiffResult<u32> {
        self.chunk_count_u32()
    }

    fn chunk_count_u32(&mut self) -> TiffResult<u32> {
        let count = self.inner.chunk_count().map_err(TiffError::from_native)?;
        u32::try_from(count).map_err(|_| TiffError::IntSizeError)
    }

    /// Pixel dimensions of every chunk (the last row/column may be padded).
    ///
    /// # Errors
    /// The same set as [`Self::dimensions`].
    pub fn chunk_dimensions(&mut self) -> TiffResult<(u32, u32)> {
        self.inner
            .chunk_dimensions()
            .map_err(TiffError::from_native)
    }

    /// Pixel dimensions actually populated for chunk `index` (edge chunks
    /// are narrower/shorter than [`Self::chunk_dimensions`]).
    ///
    /// # Errors
    /// The same set as [`Self::dimensions`].
    pub fn chunk_data_dimensions(&mut self, chunk_index: u32) -> TiffResult<(u32, u32)> {
        self.inner
            .chunk_data_dimensions(u64::from(chunk_index))
            .map_err(TiffError::from_native)
    }

    /// Decodes chunk `chunk_index` at its full coded size.
    ///
    /// # Errors
    /// The same set as [`Self::dimensions`], plus every decode failure.
    pub fn read_chunk(&mut self, chunk_index: u32) -> TiffResult<DecodingResult> {
        self.inner
            .read_chunk(u64::from(chunk_index))
            .map(DecodingResult::from_native)
            .map_err(TiffError::from_native)
    }

    /// Decodes chunk `chunk_index` into caller-owned native-endian bytes.
    ///
    /// # Errors
    /// The same set as [`Self::read_chunk`], plus a too-small `buffer`.
    pub fn read_chunk_bytes(&mut self, chunk_index: u32, buffer: &mut [u8]) -> TiffResult<usize> {
        let samples = self.read_chunk(chunk_index)?;
        let bytes = samples_to_native_bytes(&samples);
        if buffer.len() < bytes.len() {
            return Err(TiffError::UsageError(super::error::usage_error(format!(
                "buffer too small: needed {}, got {}",
                bytes.len(),
                buffer.len()
            ))));
        }
        buffer[..bytes.len()].copy_from_slice(&bytes);
        Ok(bytes.len())
    }

    /// Decodes the whole current image.
    ///
    /// # Errors
    /// The same set as [`Self::read_chunk`].
    pub fn read_image(&mut self) -> TiffResult<DecodingResult> {
        self.inner
            .read_image()
            .map(DecodingResult::from_native)
            .map_err(TiffError::from_native)
    }

    /// Decodes the whole current image into a caller-owned, reused buffer,
    /// reporting the layout actually produced.
    ///
    /// `image` 0.25.10's own entry point (`codecs/tiff.rs:375-381`).
    ///
    /// # Errors
    /// The same set as [`Self::read_image`].
    pub fn read_image_to_buffer(
        &mut self,
        result: &mut DecodingResult,
    ) -> TiffResult<BufferLayoutPreference> {
        let decoded = self.read_image()?;
        let layout = BufferLayoutPreference {
            len: decoded.len(),
            sample_format: sample_format_of(&decoded),
            sample_type: Some(decoded.sample_type()),
            row_stride: None,
            planes: 1,
            plane_stride: None,
            complete_len: decoded.len(),
        };
        *result = decoded;
        Ok(layout)
    }

    /// Decodes the whole current image into caller-owned native-endian
    /// bytes.
    ///
    /// # Errors
    /// The same set as [`Self::read_image`], plus a too-small `buffer`.
    pub fn read_image_bytes(&mut self, buffer: &mut [u8]) -> TiffResult<()> {
        let samples = self.read_image()?;
        let bytes = samples_to_native_bytes(&samples);
        if buffer.len() < bytes.len() {
            return Err(TiffError::UsageError(super::error::usage_error(format!(
                "buffer too small: needed {}, got {}",
                bytes.len(),
                buffer.len()
            ))));
        }
        buffer[..bytes.len()].copy_from_slice(&bytes);
        Ok(())
    }

    /// Reads a named tag, erroring when it is absent.
    ///
    /// # Errors
    /// [`TiffError::FormatError`] (`RequiredTagNotFound`) when the tag is
    /// absent, plus the same set as [`Self::dimensions`].
    pub fn get_tag(&mut self, tag: Tag) -> TiffResult<super::tags::ValueBuffer> {
        self.inner
            .get_tag(tag.to_native())
            .map_err(TiffError::from_native)
    }

    /// Reads a named tag, `Ok(None)` when it is absent.
    ///
    /// # Errors
    /// The same set as [`Self::dimensions`] (never `RequiredTagNotFound`).
    pub fn find_tag(&mut self, tag: Tag) -> TiffResult<Option<super::tags::ValueBuffer>> {
        self.inner
            .find_tag(tag.to_native())
            .map_err(TiffError::from_native)
    }

    /// Reads a named tag as raw bytes (the `UNDEFINED`/`BYTE` payload) --
    /// `image` 0.25.10's ICC-profile accessor (`codecs/tiff.rs:313`).
    ///
    /// # Errors
    /// The same set as [`Self::find_tag`].
    pub fn get_tag_u8_vec(&mut self, tag: Tag) -> TiffResult<Vec<u8>> {
        Ok(self
            .inner
            .get_tag_bytes(tag.to_native())
            .map_err(TiffError::from_native)?
            .unwrap_or_default())
    }

    /// Reads a named tag as an ASCII string.
    ///
    /// # Errors
    /// The same set as [`Self::find_tag`].
    pub fn get_tag_ascii_string(&mut self, tag: Tag) -> TiffResult<String> {
        Ok(self
            .inner
            .get_tag_ascii(tag.to_native())
            .map_err(TiffError::from_native)?
            .unwrap_or_default())
    }

    /// Every tag in the current image's directory.
    ///
    /// # Errors
    /// The same set as [`Self::dimensions`].
    pub fn tag_iter(&mut self) -> TiffResult<Vec<(Tag, super::tags::ValueBuffer)>> {
        Ok(self
            .inner
            .all_tags()
            .map_err(TiffError::from_native)?
            .into_iter()
            .map(|(tag, value)| (Tag::from_native(tag), value))
            .collect())
    }

    /// Consumes this decoder, returning the underlying reader.
    #[must_use]
    pub fn into_inner(self) -> R {
        self.inner.into_inner()
    }
}

fn samples_to_native_bytes(result: &DecodingResult) -> Vec<u8> {
    match result {
        DecodingResult::U8(v) => v.clone(),
        DecodingResult::U16(v) => v.iter().flat_map(|s| s.to_ne_bytes()).collect(),
        DecodingResult::U32(v) => v.iter().flat_map(|s| s.to_ne_bytes()).collect(),
        DecodingResult::U64(v) => v.iter().flat_map(|s| s.to_ne_bytes()).collect(),
        DecodingResult::F16(v) => v.iter().flat_map(|s| s.to_bits().to_ne_bytes()).collect(),
        DecodingResult::F32(v) => v.iter().flat_map(|s| s.to_ne_bytes()).collect(),
        DecodingResult::F64(v) => v.iter().flat_map(|s| s.to_ne_bytes()).collect(),
        DecodingResult::I8(v) => v.iter().map(|s| *s as u8).collect(),
        DecodingResult::I16(v) => v.iter().flat_map(|s| s.to_ne_bytes()).collect(),
        DecodingResult::I32(v) => v.iter().flat_map(|s| s.to_ne_bytes()).collect(),
        DecodingResult::I64(v) => v.iter().flat_map(|s| s.to_ne_bytes()).collect(),
    }
}

fn sample_format_of(result: &DecodingResult) -> SampleFormat {
    match result {
        DecodingResult::U8(_)
        | DecodingResult::U16(_)
        | DecodingResult::U32(_)
        | DecodingResult::U64(_) => SampleFormat::Uint,
        DecodingResult::I8(_)
        | DecodingResult::I16(_)
        | DecodingResult::I32(_)
        | DecodingResult::I64(_) => SampleFormat::Int,
        DecodingResult::F16(_) | DecodingResult::F32(_) | DecodingResult::F64(_) => {
            SampleFormat::IEEEFP
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decoding_result_matches_exhaustively_with_no_wildcard() {
        // Compiles only as long as `DecodingResult` has exactly these eleven
        // variants and stays non-`#[non_exhaustive]` -- the same guarantee
        // `tests/compat_api.rs` pins at the crate boundary.
        fn describe(r: &DecodingResult) -> &'static str {
            match r {
                DecodingResult::U8(_) => "u8",
                DecodingResult::U16(_) => "u16",
                DecodingResult::U32(_) => "u32",
                DecodingResult::U64(_) => "u64",
                DecodingResult::F16(_) => "f16",
                DecodingResult::F32(_) => "f32",
                DecodingResult::F64(_) => "f64",
                DecodingResult::I8(_) => "i8",
                DecodingResult::I16(_) => "i16",
                DecodingResult::I32(_) => "i32",
                DecodingResult::I64(_) => "i64",
            }
        }
        assert_eq!(describe(&DecodingResult::U8(vec![1])), "u8");
    }

    #[test]
    fn f16_round_trips_bit_for_bit_through_the_native_boundary() {
        let bits: Vec<u16> = vec![0x3C00, 0xBC00, 0x7C00]; // 1.0, -1.0, +inf
        let native = crate::Samples::F16(bits.clone());
        let DecodingResult::F16(values) = DecodingResult::from_native(native) else {
            panic!("expected F16");
        };
        let round_tripped: Vec<u16> = values.iter().map(|v| v.to_bits()).collect();
        assert_eq!(round_tripped, bits);
    }

    #[test]
    fn limits_default_matches_upstream() {
        let limits = Limits::default();
        assert_eq!(limits.decoding_buffer_size, 256 * 1024 * 1024);
        assert_eq!(limits.ifd_value_size, 1024 * 1024);
        assert_eq!(limits.intermediate_buffer_size, 128 * 1024 * 1024);
    }

    #[test]
    fn tiff_coding_unit_index_is_kind_independent() {
        assert_eq!(TiffCodingUnit::Strip(3).index(), 3);
        assert_eq!(TiffCodingUnit::Tile(3).index(), 3);
    }
}
