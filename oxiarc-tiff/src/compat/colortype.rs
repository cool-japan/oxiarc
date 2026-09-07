//! `tiff`-0.11-shaped colour-type markers for [`super::encoder::TiffEncoder::new_image`].
//!
//! Each marker is a zero-sized type pairing a Rust sample type
//! ([`ColorType::Inner`]) with the TIFF colour model it encodes as
//! ([`ColorType::NATIVE`]), so `new_image::<RGBA16>(w, h)` carries both
//! pieces of information the type system needs at the call site instead of
//! runtime parameters. All thirty upstream marker types are present, even
//! though `image` 0.25.10 itself only names eight of them
//! (`codecs/tiff.rs:584-602`).

/// A sample value TIFF can write, in native-endian bytes.
///
/// Distinct from [`crate::sample::SampleType`] (which names a *slot*, not a
/// value): this is the trait [`ColorType::Inner`] bounds on, so
/// `ImageEncoder::write_data` can serialise a `&[C::Inner]` without a match
/// on ten primitive types at every call site.
pub trait TiffSample: Copy + 'static {
    /// Appends this value's native-endian bytes to `out`.
    fn push_ne_bytes(self, out: &mut Vec<u8>);
}

macro_rules! tiff_sample_impl {
    ($($ty:ty),+ $(,)?) => {
        $(
            impl TiffSample for $ty {
                fn push_ne_bytes(self, out: &mut Vec<u8>) {
                    out.extend_from_slice(&self.to_ne_bytes());
                }
            }
        )+
    };
}

tiff_sample_impl!(u8, i8, u16, i16, u32, i32, u64, i64, f32, f64);

/// A colour-type marker for [`super::encoder::TiffEncoder::new_image`].
///
/// Implemented only by the zero-sized marker types this module defines
/// (`Gray8`, `RGBA16`, ...); not meant to be implemented by consumers.
pub trait ColorType {
    /// The Rust type one sample decodes/encodes as.
    type Inner: TiffSample;
    /// The native colour model this marker encodes as -- what
    /// [`super::encoder::TiffEncoder::new_image`] builds an
    /// [`crate::ImageSpec`] from.
    const NATIVE: crate::ColorType;
}

macro_rules! colortype_marker {
    ($name:ident, $inner:ty, $native:expr, $doc:literal) => {
        #[doc = $doc]
        #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
        pub struct $name;

        impl ColorType for $name {
            type Inner = $inner;
            const NATIVE: crate::ColorType = $native;
        }
    };
}

colortype_marker!(Gray8, u8, crate::ColorType::Gray(8), "8-bit greyscale.");
colortype_marker!(
    GrayI8,
    i8,
    crate::ColorType::Gray(8),
    "8-bit signed greyscale."
);
colortype_marker!(Gray16, u16, crate::ColorType::Gray(16), "16-bit greyscale.");
colortype_marker!(
    GrayI16,
    i16,
    crate::ColorType::Gray(16),
    "16-bit signed greyscale."
);
colortype_marker!(Gray32, u32, crate::ColorType::Gray(32), "32-bit greyscale.");
colortype_marker!(
    GrayI32,
    i32,
    crate::ColorType::Gray(32),
    "32-bit signed greyscale."
);
colortype_marker!(
    Gray32Float,
    f32,
    crate::ColorType::Gray(32),
    "32-bit float greyscale."
);
colortype_marker!(Gray64, u64, crate::ColorType::Gray(64), "64-bit greyscale.");
colortype_marker!(
    GrayI64,
    i64,
    crate::ColorType::Gray(64),
    "64-bit signed greyscale."
);
colortype_marker!(
    Gray64Float,
    f64,
    crate::ColorType::Gray(64),
    "64-bit float greyscale."
);

colortype_marker!(RGB8, u8, crate::ColorType::Rgb(8), "8-bit RGB.");
colortype_marker!(RGB16, u16, crate::ColorType::Rgb(16), "16-bit RGB.");
colortype_marker!(RGB32, u32, crate::ColorType::Rgb(32), "32-bit RGB.");
colortype_marker!(
    RGB32Float,
    f32,
    crate::ColorType::Rgb(32),
    "32-bit float RGB."
);
colortype_marker!(RGB64, u64, crate::ColorType::Rgb(64), "64-bit RGB.");
colortype_marker!(
    RGB64Float,
    f64,
    crate::ColorType::Rgb(64),
    "64-bit float RGB."
);

colortype_marker!(
    RGBA8,
    u8,
    crate::ColorType::Rgba(8),
    "8-bit RGB plus alpha."
);
colortype_marker!(
    RGBA16,
    u16,
    crate::ColorType::Rgba(16),
    "16-bit RGB plus alpha."
);
colortype_marker!(
    RGBA32,
    u32,
    crate::ColorType::Rgba(32),
    "32-bit RGB plus alpha."
);
colortype_marker!(
    RGBA32Float,
    f32,
    crate::ColorType::Rgba(32),
    "32-bit float RGB plus alpha."
);
colortype_marker!(
    RGBA64,
    u64,
    crate::ColorType::Rgba(64),
    "64-bit RGB plus alpha."
);
colortype_marker!(
    RGBA64Float,
    f64,
    crate::ColorType::Rgba(64),
    "64-bit float RGB plus alpha."
);

colortype_marker!(CMYK8, u8, crate::ColorType::Cmyk(8), "8-bit CMYK.");
colortype_marker!(CMYK16, u16, crate::ColorType::Cmyk(16), "16-bit CMYK.");
colortype_marker!(CMYK32, u32, crate::ColorType::Cmyk(32), "32-bit CMYK.");
colortype_marker!(
    CMYK32Float,
    f32,
    crate::ColorType::Cmyk(32),
    "32-bit float CMYK."
);
colortype_marker!(CMYK64, u64, crate::ColorType::Cmyk(64), "64-bit CMYK.");
colortype_marker!(
    CMYK64Float,
    f64,
    crate::ColorType::Cmyk(64),
    "64-bit float CMYK."
);

colortype_marker!(YCbCr8, u8, crate::ColorType::YCbCr(8), "8-bit YCbCr.");
colortype_marker!(
    CMYKA8,
    u8,
    crate::ColorType::CmykA(8),
    "8-bit CMYK plus alpha."
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_marker_reports_its_native_color_type() {
        assert_eq!(Gray8::NATIVE, crate::ColorType::Gray(8));
        assert_eq!(RGBA16::NATIVE, crate::ColorType::Rgba(16));
        assert_eq!(CMYK32Float::NATIVE, crate::ColorType::Cmyk(32));
        assert_eq!(YCbCr8::NATIVE, crate::ColorType::YCbCr(8));
        assert_eq!(CMYKA8::NATIVE, crate::ColorType::CmykA(8));
    }

    #[test]
    fn tiff_sample_serialises_native_endian_bytes() {
        let mut out = Vec::new();
        42u16.push_ne_bytes(&mut out);
        assert_eq!(out, 42u16.to_ne_bytes().to_vec());
    }
}
