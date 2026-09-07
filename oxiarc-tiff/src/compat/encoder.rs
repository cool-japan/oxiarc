//! `tiff`-0.11-shaped encoder: [`TiffEncoder`], [`ImageEncoder`],
//! [`DirectoryEncoder`].
//!
//! Every method is a thin adapter over [`crate::writer::Encoder`] /
//! [`crate::writer::ImageSpec`]: this module accumulates a native
//! [`crate::ImageSpec`] across the builder calls (`rows_per_strip`,
//! `resolution`, `.encoder().write_tag(...)`, ...) and only actually opens a
//! chunk-writing session, in the native crate, once [`ImageEncoder::write_data`]
//! runs -- upstream lets those settings follow `new_image`, while the native
//! API needs them baked into the `ImageSpec` *before* the page opens, so this
//! adapter defers, rather than calling straight through like most of
//! [`super`] does.

use std::io::{Seek, Write};
use std::marker::PhantomData;

use super::colortype::ColorType;
use super::error::{TiffError, TiffResult};
use super::tags::{self, ResolutionUnit};
use crate::ifd::{Rational, SRational, Value};

/// The colour-type marker types (`Gray8`, `RGBA16`, ...), at the path
/// upstream nests them under (`compat::encoder::colortype::*`) as well as
/// [`super::colortype`] directly.
pub use super::colortype;
pub use super::colortype::TiffSample as TiffValue;
pub use crate::ifd::{Rational as RationalValue, SRational as SRationalValue};

/// The zlib effort level for [`Compression::Deflate`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeflateLevel(pub u8);

impl Default for DeflateLevel {
    fn default() -> Self {
        Self(6)
    }
}

/// The codec a page is written with, `tiff`-0.11 shaped -- a small, fixed
/// set (upstream's own encoder supports only these four; every other codec
/// this crate can write is reachable through the native
/// [`crate::writer::Compression`], not through this compat-shaped one).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Compression {
    /// No compression.
    #[default]
    Uncompressed,
    /// LZW (5).
    Lzw,
    /// Deflate (8).
    Deflate(DeflateLevel),
    /// Apple PackBits (32773).
    Packbits,
}

impl Compression {
    fn to_native(self) -> crate::writer::Compression {
        match self {
            Self::Uncompressed => crate::writer::Compression::None,
            Self::Lzw => crate::writer::Compression::Lzw,
            Self::Deflate(DeflateLevel(level)) => crate::writer::Compression::Deflate { level },
            Self::Packbits => crate::writer::Compression::PackBits,
        }
    }
}

/// [`tags::Predictor`], re-exported under this module too -- see the
/// [`super`] module docs' "Deliberate deviations" section.
pub type Predictor = tags::Predictor;

/// Selects classic (32-bit-offset) or BigTIFF (64-bit-offset) output.
///
/// Implemented only by [`TiffKindStandard`] and [`TiffKindBig`]; not meant
/// to be implemented by consumers.
pub trait TiffKind {
    /// `true` for BigTIFF.
    const IS_BIG: bool;
    #[doc(hidden)]
    fn variant_choice() -> crate::writer::VariantChoice {
        if Self::IS_BIG {
            crate::writer::VariantChoice::Big
        } else {
            crate::writer::VariantChoice::Classic
        }
    }
}

/// Classic TIFF: 32-bit offsets, ~4 GiB ceiling.
#[derive(Clone, Copy, Debug, Default)]
pub struct TiffKindStandard;
impl TiffKind for TiffKindStandard {
    const IS_BIG: bool = false;
}

/// BigTIFF: 64-bit offsets, no practical ceiling.
#[derive(Clone, Copy, Debug, Default)]
pub struct TiffKindBig;
impl TiffKind for TiffKindBig {
    const IS_BIG: bool = true;
}

/// A `tiff`-0.11-shaped encoder, wrapping [`crate::writer::Encoder`].
#[derive(Debug)]
pub struct TiffEncoder<W: Write + Seek, K: TiffKind = TiffKindStandard> {
    inner: crate::writer::Encoder<W>,
    compression: Compression,
    predictor: Predictor,
    _kind: PhantomData<K>,
}

impl<W: Write + Seek> TiffEncoder<W, TiffKindStandard> {
    /// A classic-TIFF encoder.
    ///
    /// # Errors
    /// Propagates the sink's seek failure.
    pub fn new(writer: W) -> TiffResult<Self> {
        Self::new_generic(writer)
    }
}

impl<W: Write + Seek> TiffEncoder<W, TiffKindBig> {
    /// A BigTIFF encoder.
    ///
    /// # Errors
    /// Propagates the sink's seek failure.
    pub fn new_big(writer: W) -> TiffResult<Self> {
        Self::new_generic(writer)
    }
}

impl<W: Write + Seek, K: TiffKind> TiffEncoder<W, K> {
    /// An encoder for any [`TiffKind`], named to match upstream's generic
    /// constructor (`new`/`new_big` are the ergonomic, kind-specific
    /// spellings of this).
    ///
    /// # Errors
    /// Propagates the sink's seek failure.
    pub fn new_generic(writer: W) -> TiffResult<Self> {
        let inner = crate::writer::Encoder::new(writer)
            .map_err(TiffError::from_native)?
            .with_variant(K::variant_choice());
        Ok(Self {
            inner,
            compression: Compression::default(),
            predictor: Predictor::None,
            _kind: PhantomData,
        })
    }

    /// Sets the codec every subsequent page is written with.
    #[must_use]
    pub fn with_compression(mut self, compression: Compression) -> Self {
        self.compression = compression;
        self
    }

    /// Sets the predictor every subsequent page is written with.
    #[must_use]
    pub fn with_predictor(mut self, predictor: Predictor) -> Self {
        self.predictor = predictor;
        self
    }

    /// Begins a page of the given colour type and pixel dimensions.
    ///
    /// # Errors
    /// [`TiffError::UsageError`] for a spec this crate cannot represent (an
    /// unreachable case for any [`ColorType`] marker this module defines).
    pub fn new_image<C: ColorType>(
        &mut self,
        width: u32,
        height: u32,
    ) -> TiffResult<ImageEncoder<'_, W, C>> {
        Ok(ImageEncoder::new(
            &mut self.inner,
            width,
            height,
            self.compression,
            self.predictor,
        ))
    }

    /// Writes a whole page in one call.
    ///
    /// # Errors
    /// The same set as [`Self::new_image`], plus every
    /// [`ImageEncoder::write_data`] failure.
    pub fn write_image<C: ColorType>(
        &mut self,
        width: u32,
        height: u32,
        data: &[C::Inner],
    ) -> TiffResult<()> {
        self.new_image::<C>(width, height)?.write_data(data)
    }
}

/// A page under construction, returned by [`TiffEncoder::new_image`].
///
/// Builder methods (`rows_per_strip`, `resolution`, ...) accumulate onto a
/// native [`crate::ImageSpec`] this type owns; [`Self::write_data`] is the
/// point that actually opens the native chunk-writing session and streams
/// the pixels -- see the module docs for why this differs from most of
/// [`super`], which calls straight through.
#[derive(Debug)]
pub struct ImageEncoder<'a, W: Write + Seek, C: ColorType> {
    encoder: &'a mut crate::writer::Encoder<W>,
    spec: crate::ImageSpec,
    overrides: Vec<(u16, Value)>,
    _marker: PhantomData<C>,
}

impl<'a, W: Write + Seek, C: ColorType> ImageEncoder<'a, W, C> {
    fn new(
        encoder: &'a mut crate::writer::Encoder<W>,
        width: u32,
        height: u32,
        compression: Compression,
        predictor: Predictor,
    ) -> Self {
        let spec = crate::ImageSpec::new(width, height, C::NATIVE)
            .with_compression(compression.to_native())
            .with_predictor(predictor);
        Self {
            encoder,
            spec,
            overrides: Vec::new(),
            _marker: PhantomData,
        }
    }

    /// Overrides the strip height (rows per strip); ignored if the page ends
    /// up tiled by some other call this compat surface does not expose.
    ///
    /// # Errors
    /// Never fails; `Result` is kept to match upstream's fallible signature
    /// (a real TIFF encoder can reject `0`, but this crate treats it as "one
    /// strip", the same as libtiff).
    pub fn rows_per_strip(mut self, rows: u32) -> TiffResult<Self> {
        self.spec = self.spec.with_layout(crate::writer::Layout::Strips {
            rows_per_strip: rows,
        });
        Ok(self)
    }

    /// Sets `XResolution` and `YResolution` together.
    #[must_use]
    pub fn resolution(mut self, x: Rational, y: Rational) -> Self {
        let unit = self.current_resolution_unit();
        self.spec = self.spec.with_resolution(x, y, unit);
        self
    }

    /// Sets `XResolution` alone, keeping any previously-set `YResolution`.
    #[must_use]
    pub fn x_resolution(mut self, x: Rational) -> Self {
        let (_, y, unit) = self.spec.resolution.unwrap_or((
            Rational { num: 0, den: 1 },
            Rational { num: 0, den: 1 },
            ResolutionUnit::Inch.into(),
        ));
        self.spec = self.spec.with_resolution(x, y, unit);
        self
    }

    /// Sets `YResolution` alone, keeping any previously-set `XResolution`.
    #[must_use]
    pub fn y_resolution(mut self, y: Rational) -> Self {
        let (x, _, unit) = self.spec.resolution.unwrap_or((
            Rational { num: 0, den: 1 },
            Rational { num: 0, den: 1 },
            ResolutionUnit::Inch.into(),
        ));
        self.spec = self.spec.with_resolution(x, y, unit);
        self
    }

    /// Sets `ResolutionUnit`.
    #[must_use]
    pub fn resolution_unit(mut self, unit: ResolutionUnit) -> Self {
        let (x, y, _) = self.spec.resolution.unwrap_or((
            Rational { num: 0, den: 1 },
            Rational { num: 0, den: 1 },
            unit.into(),
        ));
        self.spec = self.spec.with_resolution(x, y, unit.into());
        self
    }

    fn current_resolution_unit(&self) -> crate::tags::ResolutionUnit {
        self.spec
            .resolution
            .map(|(_, _, unit)| unit)
            .unwrap_or(ResolutionUnit::Inch.into())
    }

    /// A handle for setting arbitrary extra tags (ICC profiles, XMP, ...)
    /// before [`Self::write_data`] finalises the page.
    pub fn encoder(&mut self) -> DirectoryEncoder<'_> {
        DirectoryEncoder {
            overrides: &mut self.overrides,
        }
    }

    /// Serialises `data` and writes the page.
    ///
    /// # Errors
    /// [`TiffError::UsageError`] when `data`'s length disagrees with the
    /// page's declared dimensions, plus every native encode/I/O failure.
    pub fn write_data(mut self, data: &[C::Inner]) -> TiffResult<()> {
        for (tag, value) in self.overrides.drain(..) {
            self.spec = self.spec.with_extra_tag(tag, value);
        }
        let mut bytes = Vec::with_capacity(std::mem::size_of_val(data));
        for value in data {
            value.push_ne_bytes(&mut bytes);
        }
        self.encoder
            .write_image(&self.spec, &bytes)
            .map_err(TiffError::from_native)
    }
}

/// A handle for writing arbitrary tags onto a page under construction,
/// returned by [`ImageEncoder::encoder`].
#[derive(Debug)]
pub struct DirectoryEncoder<'a> {
    overrides: &'a mut Vec<(u16, Value)>,
}

impl DirectoryEncoder<'_> {
    /// Sets tag `tag` to `value`, overriding any built-in value this crate
    /// would otherwise compute for it.
    ///
    /// Takes the native [`crate::Value`] directly rather than upstream's
    /// generic `TiffValue`-bounded parameter -- see the [`super`] module
    /// docs' scoping note. [`Self::write_tag_u8_vec`] covers the one
    /// concrete call `image` 0.25.10 makes (an ICC profile,
    /// `codecs/tiff.rs:531`).
    ///
    /// # Errors
    /// Never fails; `Result` is kept to match upstream's fallible signature.
    pub fn write_tag(&mut self, tag: tags::Tag, value: Value) -> TiffResult<()> {
        self.overrides.push((tag.to_u16(), value));
        Ok(())
    }

    /// Sets tag `tag` to an `UNDEFINED`-typed byte string -- an ICC profile,
    /// an arbitrary binary blob.
    ///
    /// # Errors
    /// Never fails.
    pub fn write_tag_u8_vec(&mut self, tag: tags::Tag, bytes: Vec<u8>) -> TiffResult<()> {
        self.write_tag(tag, Value::Undefined(bytes))
    }

    /// Sets tag `tag` to an ASCII string.
    ///
    /// # Errors
    /// Never fails.
    pub fn write_tag_ascii(&mut self, tag: tags::Tag, text: String) -> TiffResult<()> {
        self.write_tag(tag, Value::Ascii(text))
    }
}

impl From<ResolutionUnit> for crate::tags::ResolutionUnit {
    fn from(unit: ResolutionUnit) -> Self {
        crate::tags::ResolutionUnit::from_u16(unit.to_u16())
    }
}

/// A signed rational, `tiff`-0.11 shaped -- a plain re-export of the native
/// type, whose shape (`num`/`den` public `i32` fields) already matches.
pub type SRationalCompat = SRational;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Decoder;
    use std::io::Cursor;

    #[test]
    fn write_image_round_trips_through_the_native_decoder() {
        let pixels: Vec<u8> = (0..16).collect();
        let mut buffer = Cursor::new(Vec::new());
        TiffEncoder::new(&mut buffer)
            .expect("encoder")
            .write_image::<super::super::colortype::Gray8>(4, 4, &pixels)
            .expect("write");
        let bytes = buffer.into_inner();

        let mut decoder = Decoder::new(Cursor::new(bytes)).expect("decoder");
        assert_eq!(decoder.dimensions().expect("dims"), (4, 4));
        let samples = decoder.read_image().expect("read");
        assert_eq!(samples.as_u8(), Some(pixels.as_slice()));
    }

    #[test]
    fn bigtiff_kind_selects_the_big_variant() {
        let mut buffer = Cursor::new(Vec::new());
        let pixels: Vec<u8> = vec![1, 2, 3, 4];
        TiffEncoder::new_big(&mut buffer)
            .expect("big encoder")
            .write_image::<super::super::colortype::Gray8>(2, 2, &pixels)
            .expect("write");
        let bytes = buffer.into_inner();

        let decoder = Decoder::new(Cursor::new(bytes)).expect("decoder");
        assert_eq!(decoder.variant(), crate::header::Variant::Big);
    }

    #[test]
    fn a_registered_icc_tag_round_trips() {
        let mut buffer = Cursor::new(Vec::new());
        let pixels: Vec<u8> = vec![9, 9, 9, 9];
        let mut encoder = TiffEncoder::new(&mut buffer).expect("encoder");
        let mut page = encoder
            .new_image::<super::super::colortype::Gray8>(2, 2)
            .expect("new image");
        page.encoder()
            .write_tag_u8_vec(tags::Tag::IccProfile, vec![1, 2, 3])
            .expect("write tag");
        page.write_data(&pixels).expect("write data");
        let bytes = buffer.into_inner();

        let mut decoder = Decoder::new(Cursor::new(bytes)).expect("decoder");
        let icc = decoder.icc_profile().expect("icc").expect("present");
        assert_eq!(icc, vec![1, 2, 3]);
    }
}
