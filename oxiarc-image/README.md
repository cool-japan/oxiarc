# oxiarc-image [Stable]

Thin `image`-crate-shaped facade over `oxiarc-png`, `oxiarc-jpeg` and `oxiarc-tiff`, part of the OxiArc ecosystem.

![Version](https://img.shields.io/badge/version-0.4.2-blue)
![Tests](https://img.shields.io/badge/tests-122%20passing-brightgreen)
![License](https://img.shields.io/badge/license-Apache--2.0-green)
![Status](https://img.shields.io/badge/status-Stable-brightgreen)

**Version: 0.4.2 (2026-09-08) | 158 tests passing (145 unit + integration via
`cargo nextest`, 13 doctests) | Pure Rust, `#![forbid(unsafe_code)]`**

`oxiarc-image` exists so that a project depending on the `image` crate only for
PNG/JPEG/TIFF I/O — not `imageops`, not GIF/WebP/BMP, not `GenericImageView` — can
switch with a one-line `Cargo.toml` rename and keep every `use image::...` call site
unchanged, for the surface this crate covers. It is a **format I/O facade**, not an
image-processing library: there is no resize, blur, rotate, crop or any other
`imageops`-shaped operation, and there never will be inside this crate.

## Features

- **Pure Rust** — no C, no FFI, `#![forbid(unsafe_code)]`; every decode bottoms out in
  one of `oxiarc-png`/`oxiarc-jpeg`/`oxiarc-tiff`'s own bounded decoders
- **Format sniffing** — magic-byte detection for PNG/JPEG/TIFF (both TIFF byte orders,
  classic and BigTIFF), extension and MIME-type lookup, an `ImageFormat` enum with all
  fifteen variants `image` has so `match` statements and extension tables keep compiling
- **`ImageReader<R: BufRead + Seek>`** — `open`/`new`/`with_guessed_format`/`format`/
  `decode`/`into_dimensions`, plus the free functions `open`, `load_from_memory`,
  `load_from_memory_with_format`, `image_dimensions`
- **`DynamicImage`** — all ten `image` 0.25 variants (`Luma8`/`LumaA8`/`Rgb8`/`Rgba8`/
  …16/`Rgb32F`/`Rgba32F`), `to_rgba8`/`to_rgb8`/`to_luma8`/`to_rgba16`/`into_*`
  conversions, `width`/`height`/`dimensions`/`color`, `save`/`save_with_format`/
  `write_to`/`write_with_encoder`
- **`ImageBuffer<P, Vec<S>>`** — generic over `Luma`/`LumaA`/`Rgb`/`Rgba` pixels over
  `u8`/`u16`/`f32` samples, `from_raw`/`into_raw`/`as_raw`/`dimensions`/`get_pixel`/
  `put_pixel`/`pixels`/`from_fn`/`new`, plus the ten `image`-shaped type aliases
  (`RgbImage`, `Rgba16Image`, `GrayImage`, …)
- **`codecs::{png,jpeg,tiff}::{Encoder,Decoder}`** — `image`-shaped names
  (`PngEncoder`/`PngDecoder`/`JpegEncoder`/`JpegDecoder`/`TiffEncoder`/`TiffDecoder`)
  wrapping the native `oxiarc-*` codecs, implementing the `ImageEncoder`/`ImageDecoder`
  traits
- **32-bit float TIFF** — `Rgb32F`/`Rgba32F` round-trip through TIFF (the only one of
  the three formats with a floating-point sample format); PNG/JPEG report `Rgb32F`/
  `Rgba32F` as a named `Unsupported` error rather than silently narrowing
- **`ColorType`/`ExtendedColorType`** and an `ImageError`/`ImageResult` shaped after
  `image` 0.25's six top-level variants, with `From` conversions from every codec
  crate's own error type

## What this is not

No `imageops::*`, no `GenericImage`/`GenericImageView`, no `resize`/`blur`/`crop`/
`rotate*`/`flip*`/`filter3x3`, no animation, no GIF/WebP/BMP/ICO/HDR/EXR/farbfeld/AVIF/
QOI/PNM/TGA/DDS codec (`ImageFormat` recognises all of them by name; decoding or
encoding one is a named `ImageError::Unsupported`, never a compile error and never a
panic).

## Quick Start

```toml
[dependencies]
oxiarc-image = "0.4"
```

```rust
use oxiarc_image::{ColorType, DynamicImage, ImageBuffer, Rgb};

// Decode.
let image = oxiarc_image::open("photo.png")?;
assert_eq!(image.color(), ColorType::Rgb8);
let rgba: Vec<u8> = image.to_rgba8().into_raw();

// Encode.
let buf: ImageBuffer<Rgb<u8>> = ImageBuffer::from_fn(4, 4, |x, _y| Rgb::new(x as u8 * 60, 0, 0));
DynamicImage::ImageRgb8(buf).save("out.png")?;
# Ok::<(), oxiarc_image::ImageError>(())
```

Migrating an existing `image`-only call site is a one-line `Cargo.toml` rename for the
surface this crate covers:

```toml
[dependencies]
image = { package = "oxiarc-image", version = "0.4" }
```

## API Overview

| Item | Kind | Description |
|---|---|---|
| `open`, `load_from_memory`, `load_from_memory_with_format`, `image_dimensions` | fn | Top-level decode entry points, sniffing PNG/JPEG/TIFF by content or explicit format |
| `ImageReader<R: BufRead + Seek>` | struct | `open`/`new`/`with_guessed_format`/`format`/`decode`/`into_dimensions` |
| `ImageFormat` | enum | 15 variants (`image`-shaped); `can_decode`/`can_encode`/`from_extension`/`from_path`/`from_mime_type`/`to_mime_type`/`extension` |
| `guess_format` | fn | Magic-byte sniffing, `Unsupported` on no match |
| `DynamicImage` | enum | 10 typed-buffer variants; conversions, `save*`/`write_to`/`write_with_encoder` |
| `ImageBuffer<P>` | struct | Width/height/samples container; `Pixel` = `Luma`/`LumaA`/`Rgb`/`Rgba` over `Primitive` = `u8`/`u16`/`f32` |
| `ColorType` / `ExtendedColorType` | enum | 10-variant buffer colour types / wider raw-encode colour types (29 variants + `Unknown`) |
| `ImageDecoder` / `ImageEncoder` | trait | `dimensions`/`color_type`/`total_bytes`/`read_image`; `write_image` |
| `codecs::png::{PngDecoder, PngEncoder, CompressionType, FilterType}` | module | Direct PNG codec access with PNG-specific compression/filter options |
| `codecs::jpeg::{JpegDecoder, JpegEncoder}` | module | Direct JPEG codec access; `new_with_quality` |
| `codecs::tiff::{TiffDecoder, TiffEncoder}` | module | Direct TIFF codec access; carries `Rgb32F`/`Rgba32F` |
| `ImageError` / `ImageResult` | enum / alias | 6 top-level variants (`Decoding`/`Encoding`/`Parameter`/`Limits`/`Unsupported`/`IoError`), `std::error::Error` |

## Feature Flags

None beyond the workspace defaults — this is a thin, always-on facade over
`oxiarc-png`/`oxiarc-jpeg`/`oxiarc-tiff`'s own default feature sets; it does not enable
any of those crates' opt-in features (`parallel`, `arithmetic` toggles, `compat`, `mmap`,
`rayon`, …). `cargo tree -p oxiarc-image --all-features` has no `half` and no banned
crate in it.

## Migrating from `image`

| `image` 0.25 item | Covered here | Notes |
|---|---|---|
| `image::open`, `load_from_memory`, `load_from_memory_with_format` | yes | PNG/JPEG/TIFF only |
| `ImageReader::{open,new,with_guessed_format,format,decode,into_dimensions}` | yes | `R: BufRead + Seek`, same as upstream |
| `ImageFormat` (all 15 variants), `from_extension`/`from_path`/`from_mime_type`/`to_mime_type` | yes | only Png/Jpeg/Tiff decode or encode |
| `DynamicImage` (all 10 variants), `to_rgba8`/`to_rgb8`/`to_luma8`/`to_rgba16`/`into_*`/`width`/`height`/`dimensions`/`color` | yes | see the crate docs for the exact conversion set; grayscale uses `image` 0.25's own sRGB/Rec. 709 luma weights, and `tests/conversions.rs` pins every method's exact output samples |
| `DynamicImage::from_decoder` | yes | assembles a `DynamicImage` from any `ImageDecoder` |
| `save`/`save_with_format`/`write_to`/`write_with_encoder` | yes | |
| `ImageBuffer<P, Vec<S>>`, `RgbImage`/`RgbaImage`/`GrayImage`/… type aliases | yes | container is always `Vec<S>`, not the fully generic `Container` of upstream |
| `ImageBuffer::{from_raw,into_raw,as_raw,dimensions,get_pixel,put_pixel,pixels,from_fn,new}` | yes | `get_pixel`/`pixels` return **owned** pixels, not references — this crate is `#![forbid(unsafe_code)]`, so there is no safe zero-copy `&[Subpixel] -> &Pixel` cast; `ImageBuffer::pixel_slice`/`get_pixel_mut_channels` are the zero-copy escape hatches |
| `codecs::{png,jpeg,tiff}::{Encoder,Decoder}`, `ImageEncoder`/`ImageDecoder` traits | yes | trimmed: no ICC/EXIF/XMP encoder passthrough, no `ImageDecoderRect`, `ImageDecoder` is not object-safe (no `Box<dyn ImageDecoder>`); `read_image` returns `ImageError::Parameter` on a wrong-length buffer where `image` asserts and panics |
| `codecs::png::{PngEncoder::new_with_quality, PngDecoder}` | yes | `CompressionType`/`FilterType` mirror `image`'s own enums |
| `codecs::jpeg::JpegEncoder::{new, new_with_quality}` | yes | quality `1..=100`; **16-bit JPEG encode is not exposed here** — `oxiarc_jpeg::Encoder::encode_u16` exists at the native-crate level but this facade's `JpegEncoder` only calls `encode` (a documented deviation, not a gap: `image` itself has no 16-bit JPEG encode path either) |
| `codecs::tiff::{TiffEncoder::new, TiffDecoder}` | yes | carries `Rgb32F`/`Rgba32F` |
| `ColorType`, `ExtendedColorType` | yes | |
| `ImageError`/`ImageResult`, six variants | yes | opaque wrapper structs are trimmed of HDR/CICP-only kinds |
| `imageops::*`, `GenericImage(View)`, `resize`/`blur`/`crop`/`rotate*`/`flip*`/`filter3x3`/animation | **no** | out of scope by design — this is an I/O facade, not an image-processing library |
| GIF/WebP/BMP/ICO/HDR/EXR/farbfeld/AVIF/QOI/PNM/TGA/DDS codecs | **no** | `ImageFormat` recognises all of them by name; decoding/encoding one is a named `ImageError::Unsupported` |

### Format-specific deviations worth knowing

- **PNG has no float sample format.** `DynamicImage::ImageRgb32F`/`ImageRgba32F` written
  to PNG return `ImageError::Unsupported` rather than silently narrowing to 8/16-bit —
  matching PNG's own specification (Table 11.1 lists only integer bit depths), and
  matching what `image`'s own PNG encoder does with a float source.
- **JPEG is always lossy and has no alpha channel.** `DynamicImage::write_to(..,
  ImageFormat::Jpeg)` narrows any coloured source through `to_rgb8()` and any grayscale
  source through `to_luma8()` before it reaches the encoder — an `Rgba8` or 16-bit
  source round-trips to `Rgb8`/`L8`, never back to its own colour type. This is `image`'s
  own JPEG behaviour too, not a shortfall of this crate.
- **JPEG CMYK/YCCK decode is a named `Unsupported`.** `ColorType` has no CMYK variant —
  neither does `image` 0.25's own JPEG decoder. *Encoding* `ExtendedColorType::Cmyk8`
  through `codecs::jpeg::JpegEncoder` directly (below `DynamicImage`, which has no CMYK
  variant to reach it from) still succeeds; decoding that file back through
  `codecs::jpeg::JpegDecoder` is the named `Unsupported` error.
- **TIFF `Cmyk8`/`Cmyk16` are encode-only** for the same reason — reachable only through
  `codecs::tiff::TiffEncoder` directly, and a TIFF written that way still decodes
  successfully (through the generic RGBA fallback, to `Rgba8`, not byte-for-byte as
  CMYK).
- **TIFF 32-bit-per-channel is always interpreted as IEEE float, never integer**, because
  this crate's `ColorType` has no unsigned-32-bit variant — a genuine 32-bit integer RGB
  TIFF (rare, but real) is recognised by its own declared `SampleFormat` tag and correctly
  falls back to the generic RGBA8 path instead of being misread as float.
- **`write_with_encoder` does not auto-convert, and JPEG drops alpha silently there.**
  `image::DynamicImage::write_with_encoder` converts the image to whatever colour type
  the encoder prefers through a sealed hook; this crate has no such hook and hands the
  encoder its own colour type. An encoder that cannot accept the layout at all (JPEG with
  a 16-bit or float source, PNG with a float one) returns a named
  `ImageError::Unsupported` — but JPEG *can* accept a four-channel buffer through
  `oxiarc_jpeg::InputColor::Rgba`, and the alpha is then dropped during colour conversion,
  so `write_with_encoder(JpegEncoder::new(w))` on an `Rgba8`/`LumaA8` image **succeeds and
  loses the alpha**. (`image` 0.25's `JpegEncoder::write_image` accepts only `L8`/`Rgb8`
  through the trait and rejects `Rgba8` outright; this crate is a superset there.)
  `write_to(.., ImageFormat::Jpeg)` never has this problem — it narrows explicitly first.
- **Grayscale uses sRGB / Rec. 709 luma weights, not BT.601.** `to_luma8`/`to_luma16` use
  `(2126 R + 7152 G + 722 B) / 10000` with truncating integer division — `image` 0.25's
  `SRGB_LUMA`/`SRGB_LUMA_DIV` exactly — so a caller who swapped `image` for this crate
  gets the same grayscale bytes, not merely a plausible grayscale. (The two coefficient
  sets differ by up to 22 levels on a saturated primary: pure red is 54, not 76.) No JPEG
  output byte is affected by the choice: `write_to(.., ImageFormat::Jpeg)` routes only
  *grayscale* sources through `to_luma8`, and both coefficient sets sum to exactly their
  divisor, so `luma(l, l, l) == l` under either. Parity
  is exact for the four 8-bit variants; a 16-bit or float source narrows to 8-bit before
  `to_luma8` computes luma, where `image` computes at source width and narrows the
  result, so those can differ by a level — use `to_luma16` when that matters.
- **`ImageBuffer::get_pixel`/`pixels` return owned pixels, not references** — see the API
  Overview table above; `pixel_slice`/`get_pixel_mut_channels` are the zero-copy escape
  hatches this crate needs in place of `&Pixel`/`&mut Pixel` because it is
  `#![forbid(unsafe_code)]`.

## Guarding untrusted input

Every decode path bottoms out in one of `oxiarc-png`/`oxiarc-jpeg`/`oxiarc-tiff`'s own
bounded decoders — this crate adds no additional buffering of its own before
dispatching, so the same `DecodeLimits`/`Limits` guarantees those crates document apply
here. That matters most for `DynamicImage::from_decoder`, which allocates
`decoder.total_bytes()` up front: a hand-built PNG or TIFF header declaring 65535×65535
(17 GB implied) is rejected by the underlying decoder's *constructor*, before that
allocation is ever reached — `tests/adversarial.rs`'s
`absurd_declared_dimensions_are_rejected_before_any_allocation` builds exactly those
files and proves it, up to `u32::MAX` in both axes.

Decode failures — including a truncated file, a malformed segment, or a length mismatch
between a file's own header and what its codec actually produced — are always one of
`ImageError`'s six variants, never a panic. Two test files carry that guarantee:

- `tests/error_mapping.rs` — error propagation through the public entry points (a missing
  file, an unrecognised extension, a truncated file per format, and a value-level
  malformed-segment error with its `source()` chain intact).
- `tests/adversarial.rs` — bulk mutation sweeps over real encodings of all three formats:
  truncation at *every* offset, single-byte corruption at every offset with three bit
  patterns, byte drops and inserts at every offset, randomised truncation plus trailing
  garbage, degenerate 0–4-byte buffers, one-byte-at-a-time and 2/3/7/64-byte-chunk
  readers (whose output must match a single-read decode exactly), and re-encoding
  whatever a corrupted decode produced back through all three encoders. Anything that
  does decode is additionally checked for dimension/sample-count agreement, so a
  silently half-filled buffer fails as loudly as a panic would.

## Testing

158 tests total: 87 unit tests (`src/`, one `#[cfg(test)] mod tests` per module —
including exhaustive proofs that the integer sample-scaling helpers agree with the float
formula they replaced on all 256 `u8` and all 65 536 `u16` inputs, and the JPEG
precision-rescale endpoints for every sample width from 1 to 16 bits), 58 integration
tests, and 13 doctests. The integration suites are:

- `tests/roundtrip.rs` — every `ColorType` through PNG and TIFF byte-exact, JPEG within a
  lossy tolerance, plus the documented narrowing/encode-only cases.
- `tests/conversions.rs` — a golden table pinning the *exact* output samples of all eight
  `to_*` and four `into_*` conversions for all ten `DynamicImage` variants, so a rewrite
  of the conversion internals has to prove it changed nothing.
- `tests/sniffing.rs` — magic-byte detection, extension-vs-content disagreement.
- `tests/error_mapping.rs` — error propagation through the public entry points, including
  a real `source()` chain check.
- `tests/adversarial.rs` — the malformed-input sweeps described under "Guarding untrusted
  input" above.

```bash
cargo test -p oxiarc-image --all-features
cargo nextest run -p oxiarc-image --all-features
cargo test --doc -p oxiarc-image --all-features
```

Measured 2026-09-07.

## Part of OxiArc

`oxiarc-image` is part of the [OxiArc](https://github.com/cool-japan/oxiarc) Pure Rust
archive/compression ecosystem, sitting alongside `oxiarc-png`, `oxiarc-jpeg` and
`oxiarc-tiff` (which it wraps) and `oxiarc-http`, `oxiarc-deflate`, `oxiarc-archive` and
the other codec crates.

## Documentation

Full API documentation: `cargo doc -p oxiarc-image --open`. The crate-level rustdoc
(`src/lib.rs`) carries the same migration table as this README, plus a decoding and an
encoding example.

## License

Apache-2.0.
