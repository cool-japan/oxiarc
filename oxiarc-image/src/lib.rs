//! Thin `image`-crate-shaped facade over the OxiArc PNG, JPEG and TIFF
//! codecs.
//!
//! Part of the [OxiArc](https://github.com/cool-japan/oxiarc) Pure Rust
//! archive/compression ecosystem. `oxiarc-image` exists so that a project
//! depending on the `image` crate only for PNG/JPEG/TIFF I/O can switch with
//! a one-line `Cargo.toml` rename:
//!
//! ```toml
//! [dependencies]
//! image = { package = "oxiarc-image", version = "0.4" }
//! ```
//!
//! and keep every `use image::...` call site unchanged for the surface this
//! crate covers — see [`crate#migrating-from-image`] below for exactly what
//! that surface is.
//!
//! # What this is not
//!
//! This is a **format I/O facade**, not an image-processing library: there
//! is no resize, blur, rotate, crop, colour adjustment, or any other
//! `imageops`-shaped operation, and there never will be inside this crate —
//! that is the `image` crate's `imageops` module and its own dependency
//! tree, well outside "read and write PNG/JPEG/TIFF". A project that needs
//! both keeps `image` for processing and swaps only its I/O path, or waits
//! for a separate OxiArc processing crate.
//!
//! Only three formats are decoded or encoded: PNG, JPEG and TIFF. Every
//! other `image::ImageFormat` variant (GIF, WebP, BMP, ...) is represented
//! in [`ImageFormat`] too, so `match` statements and extension tables keep
//! compiling, but dispatching to one of them returns
//! [`ImageError::Unsupported`] by name — see [`ImageFormat::can_decode`].
//!
//! # Decoding
//!
//! ```
//! use oxiarc_image::{ColorType, DynamicImage};
//!
//! let png_bytes = sample_png();
//! let image = oxiarc_image::load_from_memory(&png_bytes).expect("decode");
//! assert_eq!(image.dimensions(), (2, 2));
//! assert_eq!(image.color(), ColorType::Rgba8);
//! let rgba: Vec<u8> = image.to_rgba8().into_raw();
//! assert_eq!(rgba.len(), 2 * 2 * 4);
//!
//! # fn sample_png() -> Vec<u8> {
//! #     use oxiarc_image::{ImageBuffer, Rgba};
//! #     let buf: ImageBuffer<Rgba<u8>> = ImageBuffer::from_fn(2, 2, |x, y| {
//! #         Rgba::new((x * 80) as u8, (y * 80) as u8, 40, 255)
//! #     });
//! #     let mut out = Vec::new();
//! #     DynamicImage::ImageRgba8(buf).write_to(std::io::Cursor::new(&mut out), oxiarc_image::ImageFormat::Png).expect("encode");
//! #     out
//! # }
//! ```
//!
//! # Encoding
//!
//! ```
//! use oxiarc_image::{ImageBuffer, Rgb};
//!
//! let buf: ImageBuffer<Rgb<u8>> = ImageBuffer::from_fn(4, 4, |x, _y| Rgb::new(x as u8 * 60, 0, 0));
//! let image = oxiarc_image::DynamicImage::ImageRgb8(buf);
//!
//! let path = std::env::temp_dir().join("oxiarc_image_doctest_encode.png");
//! image.save(&path).expect("save");
//! let reread = oxiarc_image::open(&path).expect("reopen");
//! assert_eq!(reread.dimensions(), (4, 4));
//! # std::fs::remove_file(&path).ok();
//! ```
//!
//! # Migrating from `image`
//!
//! | `image` 0.25 item | Covered here | Notes |
//! |---|---|---|
//! | `image::open`, `load_from_memory`, `load_from_memory_with_format` | yes | PNG/JPEG/TIFF only |
//! | `ImageReader::{open,new,with_guessed_format,format,decode,into_dimensions}` | yes | `R: BufRead + Seek`, same as upstream |
//! | `ImageFormat` (all 15 variants), `from_extension`/`from_path`/`from_mime_type`/`to_mime_type` | yes | only Png/Jpeg/Tiff decode or encode |
//! | `DynamicImage` (all 10 variants), `to_rgba8`/`to_rgb8`/`to_luma8`/`to_rgba16`/`into_*`/`width`/`height`/`dimensions`/`color` | yes | see [`DynamicImage`] for the exact conversion set; grayscale uses `image`'s own sRGB/Rec. 709 luma weights, see [`DynamicImage::to_luma8`] |
//! | `save`/`save_with_format`/`write_to`/`write_with_encoder` | yes | |
//! | `ImageBuffer<P, Vec<S>>`, `RgbImage`/`RgbaImage`/`GrayImage`/... type aliases | yes | container is always `Vec<S>` (not the fully generic `Container` of upstream) |
//! | `ImageBuffer::{from_raw,into_raw,as_raw,dimensions,get_pixel,put_pixel,pixels,from_fn,new}` | yes | `get_pixel`/`pixels` return **owned** pixels, not references — see [`ImageBuffer`] docs |
//! | `codecs::{png,jpeg,tiff}::{Encoder,Decoder}`, `ImageEncoder`/`ImageDecoder` traits | yes | trimmed: no ICC/EXIF/XMP encoder passthrough, no `ImageDecoderRect`; [`ImageDecoder::read_image`] errors on a wrong-length buffer where `image` panics |
//! | `ColorType`, `ExtendedColorType` | yes | |
//! | `ImageError`/`ImageResult`, six variants | yes | opaque wrapper structs are trimmed of HDR/CICP-only kinds |
//! | `imageops::*`, `GenericImage(View)`, `resize`/`blur`/`crop`/`rotate*`/`flip*`/`filter3x3`/animation | **no** | out of scope by design, see the crate-level "What this is not" above |
//! | GIF/WebP/BMP/... codecs | **no** | [`ImageFormat`] recognises them by name; decoding/encoding one is a named [`ImageError::Unsupported`] |
//!
//! # Guarding untrusted input
//!
//! Every decode path bottoms out in one of `oxiarc-png`/`oxiarc-jpeg`/
//! `oxiarc-tiff`'s own bounded decoders — this crate adds no additional
//! buffering of its own before dispatching, so the same
//! `DecodeLimits`/`Limits` guarantees those crates document apply here.
//! [`DynamicImage::from_decoder`] does allocate
//! [`ImageDecoder::total_bytes`] up front, but only *after* the decoder's
//! own constructor has accepted the header, and that constructor is where a
//! hostile size is rejected: `tests/adversarial.rs` builds PNG and TIFF
//! headers declaring up to `u32::MAX` in both axes and proves every one is
//! an error before the allocation is reached.
//!
//! Decode failures are always one of [`ImageError`]'s six variants, never a
//! panic. `tests/adversarial.rs` carries the bulk evidence: truncation at
//! every offset, single-byte corruption at every offset, byte drops and
//! inserts, randomised truncation with trailing garbage, degenerate 0-4
//! byte buffers, and one-byte-at-a-time readers whose output must match a
//! single-read decode exactly — for real encodings of all three formats.

#![warn(missing_docs)]
#![warn(clippy::all)]
#![forbid(unsafe_code)]

pub mod buffer;
pub mod codecs;
pub mod color;
pub mod dynamic;
pub mod error;
pub mod format;
pub mod reader;
pub mod traits;

pub use buffer::{
    Gray16Image, GrayAlpha16Image, GrayAlphaImage, GrayImage, ImageBuffer, Rgb16Image, Rgb32FImage,
    RgbImage, Rgba16Image, Rgba32FImage, RgbaImage,
};
pub use color::{ColorType, ExtendedColorType, Luma, LumaA, Pixel, Primitive, Rgb, Rgba};
pub use dynamic::DynamicImage;
pub use error::{ImageError, ImageFormatHint, ImageResult};
pub use format::{ImageFormat, guess_format};
pub use reader::{
    ImageReader, image_dimensions, load_from_memory, load_from_memory_with_format, open,
};
pub use traits::{ImageDecoder, ImageEncoder};
