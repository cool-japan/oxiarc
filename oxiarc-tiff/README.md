# oxiarc-tiff

Pure Rust TIFF 6.0 / BigTIFF reader and writer, part of the OxiArc ecosystem.

![Version](https://img.shields.io/badge/version-0.4.2-blue)
![Tests](https://img.shields.io/badge/tests-535%20passing-brightgreen)
![License](https://img.shields.io/badge/license-Apache--2.0-green)
![Status](https://img.shields.io/badge/status-complete-brightgreen)

**Version: 0.4.2 (2026-09-08) | 535 tests + 38 doctests passing (`--all-features`)**

No C, no FFI, `#![forbid(unsafe_code)]`, no `unwrap()` in library code.

Every codec, the reader, the writer, the `tiff`-0.11-shaped `compat` facade,
parallel (`rayon`) decode/encode and memory-mapped (`mmap`) reading are
complete. See [Migrating from the `tiff` crate](#migrating-from-the-tiff-crate)
if you are moving off `tiff`/`image`.

## Features

- **Containers** — classic TIFF (32-bit offsets) and BigTIFF (64-bit) in either
  byte order, multi-page IFD chains with loop detection, SubIFD trees with
  their own visited set and depth cap, and the EXIF / GPS / Interoperability
  sub-IFDs
- **Tags** — the TIFF 6.0 baseline and extensions, GeoTIFF (33550 / 33922 /
  34264 / 34735-34737), EXIF 34665, GPS 34853, XMP 700, ICC 34675, IPTC 33723,
  Photoshop 34377 and the DNG basics; unknown tags **and unknown field types**
  are retained verbatim as `Value::Unknown { ty_raw, bytes }` so a round trip
  is byte-identical
- **Field types** — all 18, including `LONG8` / `SLONG8` / `IFD8`, inline
  versus out-of-line values, `count * size` overflow guards and lazy value
  loading
- **Geometry** — strips and tiles, chunky and planar, 1/2/4/8/12/16/24/32/64-bit
  samples, heterogeneous `BitsPerSample`, `FillOrder` 2, YCbCr subsampling,
  `RowsPerStrip` absent (2^32-1), missing `StripByteCounts` recovery, `SHORT`
  and `LONG` offset arrays
- **Transforms** — predictor 2 (horizontal differencing with whole-sample carry
  propagation, in the *file's* byte order, stride `SamplesPerPixel` for chunky
  and 1 for planar) and predictor 3 (floating-point byte-plane transpose)
- **Colour** — `MinIsWhite` inversion, palette expansion through `ColorMap`,
  YCbCr to RGB with `YCbCrCoefficients` / `ReferenceBlackWhite` and subsampling
  reconstruction, CMYK / Separated and CIELab passthrough, `ExtraSamples` with
  associated-alpha un-premultiplication
- **Codecs** — uncompressed (1), PackBits (32773), LZW (5, with the pre-1993
  code-width rule as a fallback), Deflate (8 and 32946), CCITT RLE (2),
  Group 3 (3) and Group 4 (4) plus word-aligned RLE (32771), and behind cargo
  features ZSTD (50000), LZMA (34925) and JPEG (7, with best-effort old-style
  JPEG 6). **Every one of them encodes as well as decodes.** A registered value
  with no in-crate codec reports `UnsupportedError::Compression(n)` and one
  whose feature is off reports `FeatureNotCompiled { feature }`; there is no
  silent wildcard. Out-of-tree codecs (LERC, WebP, JPEG XL) plug in through the
  `Codec` trait and a `CodecRegistry`
- **Guards** — `Limits { max_image_bytes, decoding_buffer_size, max_ifds,
  max_ifd_entries, ... }` checked *before* every allocation, plus a file-level
  `OutputBudget` because each strip resets its codec
- **Oracles** — a `tiff-oracle` feature runs differential tests against libtiff
  (`tiffcp`, `tiffinfo`), `tifffile` and Pillow, in both directions, and
  self-skips when a tool is absent
- **`compat`** — a `tiff`-0.11.3-shaped `Decoder` / `DecodingResult` /
  `TiffEncoder` / `ImageEncoder` facade for a mechanical migration off the
  `tiff` crate (and, transitively, `image`'s `tiff` codec); see
  [Migrating from the `tiff` crate](#migrating-from-the-tiff-crate)
- **`rayon`** — parallel strip/tile decode (`Decoder::read_image_parallel`
  and friends) and encode (`Encoder::write_image_parallel`), byte-identical
  to the serial paths, off by default so the crate stays wasm32-buildable
- **`mmap`** — `Decoder::from_path`, a memory-mapped `Read + Seek` source
  built on `oxiarc_core::mmap::MappedFile`, good for windowed reads of large
  tiled (COG-style) images

## Quick Start

```toml
[dependencies]
oxiarc-tiff = "0.4.2"
```

### Reading

```rust
use oxiarc_tiff::{ColorType, Decoder, Samples};
use std::fs::File;
use std::io::BufReader;

let mut decoder = Decoder::new(BufReader::new(File::open("input.tif")?))?;
println!("{} page(s)", decoder.image_count()?);

let (width, height) = decoder.dimensions()?;
let colour = decoder.color_type()?;
match decoder.read_image()? {
    Samples::U8(pixels) => println!("{width}x{height} {colour:?}, {} bytes", pixels.len()),
    Samples::U16(pixels) => println!("{width}x{height} {colour:?}, {} samples", pixels.len()),
    other => println!("sample type {:?}", other.sample_type()),
}

// A window out of a large tiled image touches only the tiles it needs.
let window = decoder.read_region(64, 64, 256, 256)?;

// Metadata is exposed, never interpreted.
let description = decoder.get_tag_ascii(oxiarc_tiff::Tag::ImageDescription)?;
let icc = decoder.icc_profile()?;
```

### Writing

```rust
use oxiarc_tiff::{ColorType, Compression, Encoder, ImageSpec, Layout};
use std::io::Cursor;

let pixels: Vec<u8> = (0..32u32 * 32 * 3).map(|i| (i % 251) as u8).collect();
let spec = ImageSpec::new(32, 32, ColorType::Rgb(8))
    .with_compression(Compression::PackBits)
    .with_layout(Layout::Tiles { width: 16, length: 16 });

let mut buffer = Cursor::new(Vec::new());
let mut encoder = Encoder::new(&mut buffer)?;
encoder.write_image(&spec, &pixels)?;
encoder.finish()?;
```

### Streaming rows, multi-page

```rust
use oxiarc_tiff::{ColorType, Encoder, ImageSpec, Layout};
use std::io::Cursor;

let mut buffer = Cursor::new(Vec::new());
let mut encoder = Encoder::new(&mut buffer)?;
{
    let spec = ImageSpec::new(8, 4, ColorType::Gray(8))
        .with_layout(Layout::Strips { rows_per_strip: 2 });
    let mut page = encoder.new_image(&spec)?;
    for row in 0..4u8 {
        page.write_rows(&[row; 8])?;
    }
    page.finish()?;
}
encoder.write_image(&ImageSpec::new(2, 2, ColorType::Gray(8)), &[0, 1, 2, 3])?;
encoder.finish()?;
```

### Parallel decode/encode (`rayon` feature)

Byte-identical output to the serial path; only the CPU-bound decompress or
compress step is spread across a thread pool — I/O and placement stay serial.
See the [`rayon_support` module docs](https://docs.rs/oxiarc-tiff) for the
full design and which codecs' shared scratch (`CodecState`) limits the win.

```rust
use oxiarc_tiff::{ColorType, Decoder, Encoder, ImageSpec};
use std::io::Cursor;

# let bytes = {
#     let mut buffer = Cursor::new(Vec::new());
#     let mut encoder = Encoder::new(&mut buffer)?;
#     encoder.write_image(&ImageSpec::new(64, 64, ColorType::Gray(8)), &[0u8; 64 * 64])?;
#     encoder.finish()?;
#     buffer.into_inner()
# };
let mut decoder = Decoder::new(Cursor::new(bytes))?;
let pixels = decoder.read_image_parallel()?; // identical to read_image()

let spec = ImageSpec::new(64, 64, ColorType::Gray(8));
let mut out = Cursor::new(Vec::new());
Encoder::new(&mut out)?.write_image_parallel(&spec, &pixels.to_native_bytes())?;
# Ok::<(), oxiarc_tiff::TiffError>(())
```

### Memory-mapped reading (`mmap` feature)

```rust,no_run
use oxiarc_tiff::Decoder;

let mut decoder = Decoder::from_path("large_cog.tif")?;
let window = decoder.read_region(4096, 4096, 512, 512)?; // one seek+read pair per tile
# Ok::<(), oxiarc_tiff::TiffError>(())
```

## Migrating from the `tiff` crate

The `compat` feature mirrors `tiff` 0.11.3's module tree closely enough that
most migrations are an import-path swap:

```diff
-use tiff::decoder::{Decoder, DecodingResult};
-use tiff::{ColorType, TiffError};
-use tiff::tags::Tag;
+use oxiarc_tiff::compat::decoder::{Decoder, DecodingResult};
+use oxiarc_tiff::compat::{ColorType, TiffError};
+use oxiarc_tiff::compat::tags::Tag;
```

```toml
[dependencies]
-tiff = "0.11"
+oxiarc-tiff = { version = "0.4.2", features = ["compat", "all-codecs"] }
```

What is frozen, byte-for-byte, because downstream code (`image` 0.25.10's
`codecs/tiff.rs` included) matches it exhaustively with no wildcard arm:

- `compat::decoder::DecodingResult` — exactly the eleven upstream variants
  (`U8/U16/U32/U64/I8/I16/I32/I64/F16/F32/F64`), `F16` holding real
  `half::f16` values (the native `Samples::F16` stays raw `u16` bits — see
  its own docs — `half` is pulled in only by this feature)
- `compat::ColorType` — all ten upstream variants, including
  `Multiband { bit_depth, num_samples }`
- `compat::TiffError` — exactly the six upstream variants
  (`IoError`/`FormatError`/`IntSizeError`/`UsageError`/`UnsupportedError`/`LimitsExceeded`)

`tests/compat_api.rs` reproduces the calls `image` 0.25.10 makes against this
module — `Limits` then `Decoder::new` + `with_limits`, dimensions, colour
type, `find_tag_unsigned_vec::<u16>(SampleFormat)`, `read_image_to_buffer`,
the ICC-profile and orientation tag reads (`get_tag_u8_vec`, `into_u16`),
both exhaustive-`TiffError`-match sites, and the `TiffEncoder`/`ImageEncoder`
write path with an ICC tag — so a shape break fails a compile or a test here,
not downstream. It is a **shape** pin, not a claim that `image` builds against
this module unmodified: `image`'s planar branch chains
`as_bytes().chunks_exact(..).collect()` off a *borrowed* byte view, which
`#![forbid(unsafe_code)]` cannot hand back (see the deviations below).
`compat::encoder::colortype` carries all thirty upstream colour-type marker
types (`Gray8` .. `CMYKA8`), not only the eight `image` names directly.

`BufferLayoutPreference`'s `len`, `row_stride`, `plane_stride` and
`complete_len` are **byte** counts, exactly as upstream documents and consumes
them, so the idiom upstream documents on `read_image_to_buffer`
(`if buffer.byte_len() < layout.complete_len { /* a plane is missing */ }`)
compares like with like.

Deliberate, documented deviations (never a silent behaviour change):
`ImageEncoder`'s setters are by-value builders returning `Self`
(`rows_per_strip` returns `TiffResult<Self>`) where upstream's take
`&mut self`, return `()` and `unwrap()` internally — this crate has no
`unwrap()` in library code, and upstream's own users never call these;
`DecodingBuffer::to_bytes` is named `to_bytes`, not upstream's `as_bytes`, and
returns an owned `Vec<u8>` rather than a zero-copy view (this crate is
`#![forbid(unsafe_code)]`, and a zero-copy numeric-slice-to-bytes view needs
one — use `byte_len()` when only the length is wanted); `encoder::Predictor`
is a re-export of `tags::Predictor` rather than a second,
independently-defined enum of the same shape. See the `compat` module docs
(`cargo doc --features compat --open`) for the complete list.

If you are migrating through `image` rather than `tiff` directly, the sibling
[`oxiarc-image`](https://docs.rs/oxiarc-image) crate is the facade for
`image` itself (`open`/`load_from_memory`/`DynamicImage`-shaped types); this
crate's `compat` feature is the layer under it for the `tiff` codec
specifically.

## API Overview

| Item | Kind | Description |
|------|------|-------------|
| `Decoder<R: Read + Seek>` | struct | Multi-image reader: `image_count`, `next_image`, `seek_to_image`, `info`, `read_image`, `read_region`, `read_strip`/`read_tile` (raw and decoded), typed tag accessors, `sub_ifd_tree`, `exif_directory`, `geo_tags` |
| `Encoder<W: Write + Seek>` | struct | Multi-page writer: `new_image` (streaming), `write_image`, `with_endian`, `with_variant`, `finish`; offsets patched after the data is written |
| `ImageSpec` | struct | Builder for one page: colour type, bit depths, sample formats, photometric, planar config, compression, predictor, fill order, layout, colormap, extra samples, resolution, YCbCr subsampling, arbitrary extra tags |
| `ImageInfo` | struct | Parsed geometry and semantics of one IFD, incl. `expected_total_bytes()`, chunk origins and coded/valid dimensions |
| `Samples` | enum | `U8/U16/U32/U64/I8/I16/I32/I64/F16/F32/F64` typed pixel buffers (`F16` keeps raw `u16` bits; `f16_bits_to_f32` converts) |
| `Directory` / `Entry` / `Value` | struct/enum | IFD access; `Value::Unknown { ty_raw, bytes }` retains unknown field types |
| `Tag` / `Type` | enum | Documented tag and field-type tables with raw fallbacks |
| `Limits` / `OutputBudget` | struct | Pre-allocation guards and the file-level decoded-byte budget |
| `Codec` / `CodecRegistry` | trait/struct | Out-of-tree codec plug-in point |
| `CodecContext` / `CodecState` | struct | What a codec is told about a chunk, and what it keeps across the chunks of one image (the LZW code-width rule, the inflate window, the fax changing-element buffers) |
| `TiffError` | enum | `Format` / `Unsupported` / `Limits` / `Usage` / `Io`, all `#[non_exhaustive]` |
| `Decoder::from_path` | method | (`mmap` feature) A memory-mapped `Decoder<Cursor<MappedFile>>` — `MmapDecoder` names the type |
| `Decoder::read_image_parallel` and friends | method | (`rayon` feature) Parallel decode, byte-identical to the serial method |
| `Encoder::write_image_parallel` | method | (`rayon` feature) Parallel encode, byte-identical to `write_image` |
| `compat::decoder::Decoder` / `DecodingResult` | struct/enum | (`compat` feature) `tiff`-0.11-shaped reader; see [Migrating from the `tiff` crate](#migrating-from-the-tiff-crate) |
| `compat::encoder::TiffEncoder` / `ImageEncoder` | struct | (`compat` feature) `tiff`-0.11-shaped writer, incl. `TiffKindBig` for BigTIFF |

## Feature Flags

| Feature | Default | Description |
|---------|---------|-------------|
| `deflate` | **yes** | Compression 8 / 32946 through `oxiarc-deflate` |
| `lzw` | **yes** | Compression 5 through `oxiarc-lzw` |
| `ccitt` | **yes** | Compression 2 / 3 / 4 / 32771, in-crate |
| `packbits` | **yes** | A no-op: PackBits and uncompressed are always compiled. The feature exists so `features = ["packbits"]` keeps working |
| `zstd` | no | Compression 50000 through `oxiarc-zstd` |
| `lzma` | no | Compression 34925 (one complete `.xz` stream per chunk) through `oxiarc-lzma` |
| `jpeg` | no | Compression 7 and best-effort 6 through `oxiarc-jpeg` |
| `all-codecs` | no | Every codec above |
| `compat` | no | The `tiff`-0.11-shaped `compat::{decoder, encoder, tags}` facade. Pulls in `half` (for `DecodingResult::F16(Vec<half::f16>)`); the native API never needs it |
| `rayon` | no | Parallel strip/tile decode/encode (`*_parallel` methods), through `rayon`. Off by default so the crate stays wasm32-buildable |
| `mmap` | no | `Decoder::from_path`, a memory-mapped reader, through `oxiarc-core`'s `mmap` feature |
| `tiff-oracle` | no | Differential tests against libtiff `tiffcp`/`tiffinfo`, `tifffile` and Pillow. Every test self-skips when its tool is missing. Never needed for library use. |

`--no-default-features` is a supported, tested configuration and means "no
codec beyond uncompressed and PackBits": every other value then reports
`FeatureNotCompiled` with the feature to turn on.

## Interoperability notes

- A predictor is honoured by libtiff only for the codecs that install its
  predictor hooks (LZW, Deflate, ZSTD, LZMA, LERC). Writing `Predictor::Horizontal`
  with `Compression::None` or `PackBits` produces a file this crate reads back
  exactly and libtiff misreads — `tiff_oracle::libtiff_ignores_the_predictor_for_uncompressed_data`
  pins that behaviour so it is never mistaken for our bug.
- `FillOrder = 2` reverses the bits of every byte of the **compressed** chunk,
  at *every* bit depth. That is where libtiff does it (`TIFFFillStrip` on read,
  `TIFFFlushData1` on write) and it was measured against libtiff 4.7.1 at 1, 8,
  16, 32 and 64 bits (integer and float), in strips and tiles: a
  `tiffcp -c packbits -f lsb2msb` strip has its PackBits control bytes reversed
  too, so decoding it without reversing first yields the wrong *length*, not
  merely the wrong bits. The CCITT codecs are the exception and consume the tag
  themselves. `read_chunk_raw` deliberately does **not** apply the reversal.
- A `YCbCr` page with no `YCbCrSubSampling` tag reads back as 2x2 subsampled
  (the TIFF 6.0 default), so the encoder always writes tag 530.
- `Orientation`, `ImageDescription` (ImageJ, OME) and the GeoTIFF *keys* are
  carried and exposed, never interpreted.
- **The CCITT codes are photometric-agnostic.** Measured against libtiff 4.7.1:
  `tiffcp -c g3` and `-c g4` write byte-identical strips for a `MinIsWhite` and
  a `MinIsBlack` page holding the same bits, and a `tiffcp -c none` round trip
  returns those bits for both. A coded *white* run is a run of zero bits,
  always; `PhotometricInterpretation` decides how the bits are displayed, not
  how they are coded. Our G3-1D and G4 output is byte-identical to `tiffcp`'s;
  our G3-2D output is smaller, because libtiff re-sends a one-dimensional row
  every K rows for fax error resilience, which TIFF does not require (libtiff
  reads ours back with identical pixels).
- **Group 3/4 uncompressed mode** (T.4 §4.2.1.3.2, `T4Options` bit 1 /
  `T6Options` bit 1) is decoded *and* written, which libtiff cannot do:
  libtiff 4.7.1 answers the same data with "Uncompressed data (not supported)".
  The mode is read unconditionally — the entrance code is unambiguous, so a
  file that carries it without declaring it in the option tag still decodes —
  and written only when
  [`ImageSpec::with_ccitt_uncompressed(true)`](https://docs.rs/oxiarc-tiff/latest/oxiarc_tiff/struct.ImageSpec.html#method.with_ccitt_uncompressed)
  asks for it, because a file that used it uninvited would be unreadable in
  libtiff. The encoder prices both codings of every row exactly and takes the
  mode only where it is smaller, so turning it on can only shrink a page: a
  512x64 dithered bilevel page more than halves in all three dialects, while a
  page of long runs comes out byte-identical to one written with the mode off.
- **A greyscale-plus-alpha JPEG page round-trips chunky.** JPEG defines no
  *named* colour space with two components; `oxiarc-jpeg` 0.4.2 added
  libjpeg's `JCS_UNKNOWN` layout for exactly this case (`ColorSpace::
  Unknown(2)`: sequential ids `1`/`2`, no subsampling, no colour transform,
  no `JFIF`/Adobe marker), so the `ImageSpec::validate` refusal and the
  `plan()` arm that produced it — both present through 0.4.1 — are gone.
  `PlanarConfiguration::Planar` (each channel its own single-component
  frame) still works too, if that shape is preferred for some other reason.
  Interoperability, measured rather than assumed: libtiff's own `tiffinfo`
  reports the page cleanly (`Samples/Pixel: 2`, `Extra Samples: 1<unassoc-
  alpha>`), and `tiffcp -c none` makes libtiff's *own* embedded libjpeg
  actually decode the two-component entropy-coded scan (not merely parse its
  header, which is as far as a bare `djpeg` can get for this shape — no
  output module of its own accepts a colour space it cannot map to
  grayscale or RGB) — the result is byte-identical to this crate's own
  decode of the same file.
- `Compression::Deflate` writes tag value **8**, the Adobe registration libtiff,
  GDAL and `tifffile` all use; 32946 is read identically.
- A JPEG page's `SOF` sampling factors are authoritative over
  `YCbCrSubSampling` (TTN2). A disagreement is an error only under
  `Leniency::Strict`, because a file with no tag 530 defaults to 2x2 in the
  TIFF model and rejecting every 4:4:4 stream in such a file would break real
  images. `RowsPerStrip` must be a multiple of `8 * Vmax` for a JPEG page, which
  `ImageSpec::validate` enforces — libtiff refuses anything else.
- TIFF JPEG carries no `JFIF` and no `Adobe` marker: colour lives in
  `PhotometricInterpretation`. Chunks are decoded as raw components and the
  TIFF matrix (`YCbCrCoefficients`, `ReferenceBlackWhite`) is applied by this
  crate; a `Separated` page is passed through **without** the Adobe inversion.

## Benchmarks

`cargo bench -p oxiarc-tiff` (Apple M-series, 1024x1024, release):

| Group | Case | Throughput |
|---|---|---|
| `tiff_decode` | `gray8_strips_none` | 7.3 GiB/s |
| `tiff_decode` | `gray8_tiles_none` | 6.9 GiB/s |
| `tiff_decode` | `gray8_strips_packbits` | 2.7 GiB/s |
| `tiff_decode` | `rgb8_strips_none` | 5.7 GiB/s |
| `tiff_decode` | `rgb8_strips_packbits` | 4.6 GiB/s |
| `tiff_predictor` | `gray16_horizontal` | 457 MiB/s |
| `tiff_predictor` | `float32_floating_point` | 1.44 GiB/s |
| `tiff_read_region` | 512x512 window of a 2048x2048 tiled image | 2.6 GiB/s |
| `tiff_encode` | `gray8_none` | 9.8 GiB/s |
| `tiff_encode` | `gray8_packbits` | 1.14 GiB/s |
| `tiff_codec_decode` | `gray8_lzw` | 262 MiB/s |
| `tiff_codec_decode` | `gray8_deflate` | 934 MiB/s |
| `tiff_codec_decode` | `rgb8_deflate_predictor` | 584 MiB/s |
| `tiff_codec_decode` | `bilevel_g4` | 245 MiB/s |
| `tiff_codec_decode` | `bilevel_g3_2d` | 292 MiB/s |
| `tiff_codec_decode` | `gray8_zstd` | 441 MiB/s |
| `tiff_codec_decode` | `gray8_lzma` | 84 MiB/s |
| `tiff_codec_decode` | `gray8_jpeg` | 82 MiB/s |
| `tiff_codec_decode` | `ycbcr_jpeg_420` | 149 MiB/s |

An LZW strip decodes **8.6x** faster than the dictionary-of-`Vec` path it
replaced (228 us against 1.97 ms for a 64 KiB strip, `cargo bench -- tiff_lzw_strip`
at full precision); `benches/tiff_bench.rs` contains that older data structure so
the comparison is measured rather than asserted. The design target was 3x.

Against libtiff 4.7.1 on a 4000x3000 RGB8 image (36 MB of pixels), best of five,
where `tiffcp -c none` is charged with an encode and a file write we do not do:

| codec | `tiffcp -c none` | ours, decode only |
|---|---|---|
| uncompressed | 64.8 ms | **4.1 ms** |
| PackBits | 65.1 ms | **6.4 ms** |
| LZW | 124 ms | 164 ms |
| Deflate | 60 ms | 75 ms |
| ZSTD | 63 ms | 135 ms |
| LZMA | 470 ms | 722 ms |

Read honestly: the uncompressed and PackBits paths are an order of magnitude
ahead, and the compressed codecs are 1.3x to 2.1x of `tiffcp`'s wall clock. The
TIFF layer is not where that time goes — the same image decodes in 4.1 ms with
no codec at all — it is inside the shared `oxiarc-lzw` / `oxiarc-zstd` /
`oxiarc-lzma` decoders, which is where the next round of work belongs.

The design target was 1.25x. An independent re-measurement on
poorly-compressible (photograph-like) 4000x3000 RGB8 data, best of five, put LZW
at 1.63x, Deflate at 1.24x, ZSTD at 2.77x and LZMA at 1.12x of `tiffcp -c none`;
uncompressed at 0.09x and PackBits at 0.18x. Subtracting this crate's own
no-codec decode time from each figure left essentially the whole gap inside the
codec crate, so closing it was work for `oxiarc-lzw` / `oxiarc-zstd`, not for
`oxiarc-tiff`. **That work has since been done — see the 2026-09-08 table
below**, where both LZW rows, RGB8 Deflate and Gray16 ZSTD now meet the target.
The two paragraphs above are kept as the historical record of how it looked
before.

A third measurement, 4096x4096, 16-row strips, medians of nine **interleaved**
rounds at load average 10-64, taken with the per-image decoder pools in place,
put LZW at 2.41x, Deflate at 1.58x, ZSTD at 6.24x, LZMA at 1.15x and JPEG at
2.37x of `tiffcp -c none`, and concluded that "ZSTD is the outlier by a wide
margin". **Those figures are superseded and that conclusion is no longer true.**
They were measured before `oxiarc-lzw`, `oxiarc-deflate` and `oxiarc-zstd` were
each rewritten underneath this crate. They are kept here as the "before" column
so the movement is visible rather than quietly edited away.

**Re-measured 2026-09-08.** Same geometry — 4096x4096, 16-row strips, every
fixture written by `tiffcp -m 0 -r 16 -c <codec>` — medians of 15
**interleaved** rounds (one `tiffcp` and one decode of the same fixture back to
back, so a load spike hits both arms of a pair), **three independent runs** at
load average **110 down to 20** on 8 cores. Ratios and absolute times are given
as two separate tables, because they come from different aggregations and a
single mixed table would contain numbers that do not divide into each other.

**Ratios**, one column per run, and the median of the three run medians — this
is the column the gate is judged on:

| fixture | codec | run 1 (load 110→57) | run 2 (47→30) | run 3 (31→20) | median | before | <= 1.25x |
|---|---|---|---|---|---|---|---|
| RGB8 | uncompressed | 0.25x | 0.15x | 0.19x | **0.19x** | 0.11x | met |
| RGB8 | PackBits | 0.18x | 0.15x | 0.16x | **0.16x** | 0.20x | met |
| RGB8 | LZW | 1.28x | 1.07x | 1.07x | **1.07x** | 2.41x | **met** (was missed) |
| RGB8 | Deflate | 1.13x | 1.14x | 1.16x | **1.14x** | 1.58x | **met** (was missed) |
| RGB8 | ZSTD | 1.65x | 1.45x | 1.75x | 1.65x | 6.24x | missed |
| RGB8 | LZMA | 1.33x | 1.32x | 1.27x | 1.32x | 1.15x | **missed** (was met) |
| RGB8 | JPEG | 1.90x | 1.95x | 1.71x | 1.90x | 2.37x | missed |
| Gray16 | uncompressed | 0.13x | 0.24x | 0.11x | **0.13x** | - | met |
| Gray16 | LZW | 0.84x | 1.13x | 1.00x | **1.00x** | 2.13x | **met** (was missed) |
| Gray16 | Deflate | 1.24x | 1.33x | 1.40x | 1.33x | 1.39x | missed |
| Gray16 | ZSTD | 0.87x | 1.12x | 0.96x | **0.96x** | 5.07x | **met** (was missed) |
| Gray16 | LZMA | 1.32x | 1.35x | 1.31x | 1.32x | 1.16x | **missed** (was met) |
| bilevel | uncompressed | 2.73x | 5.32x | 5.30x | 5.30x | - | unlike work |
| bilevel | Group 3 | 3.90x | 4.96x | 5.04x | 4.96x | 3.40x | missed |
| bilevel | Group 3 2D | 4.17x | 4.80x | 4.89x | 4.80x | 3.33x | missed |
| bilevel | Group 4 | 3.80x | 4.83x | 4.88x | 4.83x | 2.68x | missed |

**Absolute times**, all from run 3 alone (load 31→20, the quietest of the
three) so that every number in this table is internally consistent and its own
quotient is its own row's ratio. They are not portable off this machine:

| fixture | codec | `tiffcp -c none` | ours, decode only | run 3 ratio |
|---|---|---|---|---|
| RGB8 | uncompressed | 33.5 ms | 6.2 ms | 0.19x |
| RGB8 | PackBits | 40.9 ms | 6.7 ms | 0.16x |
| RGB8 | LZW | 195.3 ms | 209.8 ms | 1.07x |
| RGB8 | Deflate | 128.0 ms | 149.0 ms | 1.16x |
| RGB8 | ZSTD | 90.7 ms | 159.1 ms | 1.75x |
| RGB8 | LZMA | 952.0 ms | 1208.5 ms | 1.27x |
| RGB8 | JPEG | 54.8 ms | 93.5 ms | 1.71x |
| Gray16 | uncompressed | 37.5 ms | 4.0 ms | 0.11x |
| Gray16 | LZW | 94.3 ms | 94.1 ms | 1.00x |
| Gray16 | Deflate | 82.9 ms | 116.1 ms | 1.40x |
| Gray16 | ZSTD | 53.5 ms | 51.4 ms | 0.96x |
| Gray16 | LZMA | 475.2 ms | 621.3 ms | 1.31x |
| bilevel | uncompressed | 5.5 ms | 29.3 ms | 5.30x |
| bilevel | Group 3 | 9.2 ms | 46.5 ms | 5.04x |
| bilevel | Group 3 2D | 9.8 ms | 47.8 ms | 4.89x |
| bilevel | Group 4 | 9.9 ms | 48.1 ms | 4.88x |

**Four rows crossed the gate into "met"** — both LZW rows, RGB8 Deflate and
Gray16 ZSTD — and ZSTD, the codec that was 5-6x, is now 0.96x on Gray16 and
1.65x on RGB8. That is the three codec rewrites landing: `oxiarc-zstd`
(~7x on TIFF-strip shapes), `oxiarc-lzw` (1.25x-2.6x) and `oxiarc-deflate`
(1.04x-3.3x), each measured independently in its own crate.

**Why these ratios are believable on a machine at load 20-110.** PackBits, LZMA,
JPEG and the three CCITT codecs were not touched by any of those rewrites, so
they are the control: they should reproduce the earlier band, and they do
(PackBits 0.16x against 0.20x, JPEG 1.90x against 2.37x). The movers are far
outside that drift — ZSTD by 3.8x-5.3x, LZW by 2.1x-2.3x.

**Two rows read honestly rather than favourably.** *LZMA moved the wrong way*
(1.15x-1.16x to 1.31x-1.32x) although nothing in `oxiarc-lzma` changed. The
`tiffcp` arm settles what happened, because libtiff did not change either, so
where its own time is close the two fixtures are comparable:

| codec | `tiffcp` then | `tiffcp` now | |
|---|---|---|---|
| LZW | 215.8 ms | 195.3 ms | 0.90x |
| Deflate | 146.7 ms | 128.0 ms | 0.87x |
| ZSTD | 76.1 ms | 90.7 ms | 1.19x |
| uncompressed | 69.6 ms | 33.5 ms | 0.48x |
| **LZMA** | **2026 ms** | **952 ms** | **0.47x** |

The three rewritten codecs sit within ~15% on the reference arm, which is what
makes their movement trustworthy. LZMA's reference arm **halved** on nominally
identical geometry with an unchanged decoder — so that row is measuring a
different fixture, not a regression. (Part of it is load: the uncompressed
reference row moved by the same factor. But LZMA moved twice as far as any of
the three codecs whose ratios we are relying on.) This fixture compresses
2.5x/3.1x where the earlier perf sources compressed ~3.7x. *The bilevel rows still compare unlike work*: `tiffcp` copies 2 MB of
packed bits and this crate expands them to 16 MB of one-byte-per-pixel samples,
which is the whole 5.30x uncompressed row. Net of that expansion the fax codecs
run at 1.87x-1.90x, reproducing the earlier "roughly 1.5-2x" figure.

Subtracting each arm's own no-codec baseline isolates the codec, but on a shared
machine it is a difference of two large noisy numbers and carries no useful
ratio of its own — `tiffcp`'s RGB8 ZSTD codec time is `90.7 - 33.5`, two ~10%
measurements subtracted. The ratio table above is the robust one, which is why
the gate is judged on it. For the record, run 3's subtraction gives, for RGB8:
LZW 161.8 ms against 203.6 ms, Deflate 94.5 ms against 142.8 ms, ZSTD 57.2 ms
against 152.9 ms, and a strip loop of about 6 ms on our side either way.

Where the remaining gap lives is now answered by each codec crate's own A/B
harness rather than by a strip-level estimate. On TIFF-strip-shaped payloads
they measure: **LZW 269-525 MB/s** (`oxiarc-lzw`, `examples/lzw_vs_libtiff.rs`,
4096-pixel-wide strips), **ZSTD 520-950 MB/s** (`oxiarc-zstd`,
`examples/decode_throughput.rs`, the `tiff288k`/`tiff1m` shapes) and **Deflate
250-580 MB/s** (`oxiarc-deflate`, `examples/inflate_ab.rs`, `rgb8-image-rows`
and `png-filtered-rows`). The earlier estimate of 122-144 MB/s for LZW,
293-298 MB/s for Deflate and 104-118 MB/s for ZSTD predates all three rewrites.

Group 4 decode got faster after this round: the 2D row decoder's `b1`/`b2`
search restarted at changing element zero for every code word, which is
quadratic in the number of runs per row. Resuming the search where the previous
one stopped (legitimate, because `a0` never moves backwards inside a row) makes
it linear. How much that is worth depends entirely on the page — the two
searches were timed against each other directly, interleaved, medians of five,
on three 4096x4096 Group 4 pages (release build, shared machine at load 22):

| page | resumable | restarting | ratio |
|---|---|---|---|
| a few long runs per row (a scan of text) | 9.0 ms | 9.2 ms | 1.03x |
| the benches' bilevel fixture | 7.9 ms | 37.8 ms | **4.8x** |
| hundreds of runs per row (halftone) | 55.4 ms | 1360 ms | **24.6x** |

So it is free on the pages fax coding was designed for and decisive on the ones
it was not — which is the shape "quadratic in runs per row" predicts.

**`rayon` on/off — parallel decode is not a uniform win; it depends on the
codec *and* on how compressible the data is.** Release build, 8 cores; serial
and parallel measured *interleaved in the same loop*, medians of nine rounds.
The first block is 4096x4096 Gray8 at load average 6-10, the second the same
size at 33-46, so compare each arm with the other arm of its own row, never
across blocks:

| Fixture | Serial | Parallel | Ratio |
|---|---|---|---|
| LZW, 256x256 tiles, incompressible | 114 ms | 32 ms | **3.6x faster** |
| LZW, 256x256 tiles, compressible | 26.6 ms | 10.4 ms | **2.6x faster** |
| PackBits, 256x256 tiles, compressible | 2.6 ms | 2.4 ms | 1.09x |
| PackBits, 256x256 tiles, incompressible | 3.8 ms | 4.2 ms | 0.90x |
| uncompressed, 64-row strips | 2.1 ms | 3.1 ms | **0.66x (slower)** |

The codecs that keep per-image scratch, 32-row strips, remeasured after the
single-slot caches became pools:

| Fixture | Serial | Parallel | Ratio |
|---|---|---|---|
| Deflate, Gray8 | 77.7 ms | 34.6 ms | **2.24x faster** |
| LZW, Gray8 | 166.4 ms | 78.2 ms | **2.13x faster** |
| Group 4, bilevel | 81.9 ms | 27.0 ms | **3.04x faster** |
| Group 3 2D, bilevel | 69.0 ms | 34.0 ms | **2.03x faster** |

The win is exactly the CPU cost of a chunk's decompress step; the serial fetch
pass (one `Read + Seek` handle), the `memcpy` placement and the thread-pool
dispatch are overhead on top of it. In two groups:

- **Worth it** — Deflate, LZW and both fax codecs measured above, and by the
  same argument ZSTD, LZMA and JPEG. Deflate used to sit at 0.98x and the fax
  codecs had no gain at all, because one cached decoder behind a `Mutex` made
  every worker queue for the codec; giving each image a *pool* of decoders is
  what turned those rows into 2-3x. LZW came out faster in every run, including
  a repeat at load average 30 on 8 cores where it still managed 1.3x.
- **No reliable gain, sometimes a loss** — uncompressed and PackBits. These are
  `memcpy`-bound, so overhead is a large fraction of the total; repeated runs
  straddled 1.0 (PackBits ranged 0.32x-1.12x with data compressibility and free
  cores). Use plain `read_image` for these.

Absolute times move with machine load — the parallel arm needs free cores and
the serial arm does not, so a busy box penalises it even in an interleaved
measurement. The compressed-codec rows above held under every load tested.

Encode is the easier direction: every chunk's compression is independent CPU
work with no shared lock, so it parallelises for every codec. The
`tiff_rayon_encode` bench group measured 16.8 ms serial vs 5.4 ms parallel
(3.1x) on a PackBits tile fixture; as criterion numbers taken under load they
are far less trustworthy than the interleaved decode table above, so
re-measure on an idle machine before quoting them.

## Status

Complete: reader, writer, geometry, sampling, predictors, colour, **every
codec** (uncompressed, PackBits, LZW, Deflate, CCITT RLE / G3 / G4, ZSTD,
LZMA, JPEG, old-style JPEG) in both directions, the `tiff`-0.11-shaped
`compat` facade, parallel (`rayon`) decode/encode, memory-mapped (`mmap`)
reading, a `CodecRegistry` plugin worked example, and a fuzz-seed generator
for the five planned targets (the targets themselves are added by the
workspace's Wave 3 track, in `fuzz/`). Covered by the 40-row edge-case
register, per-codec proptests, truncation and bit-flip sweeps, libtiff /
`tifffile` / Pillow oracles that check both directions for each codec, and
`tests/compat_api.rs` pinning the `compat` shape against `image` 0.25.10's
real call sequence. See `TODO.md`.

## Part of OxiArc

This crate is part of the [OxiArc](https://github.com/cool-japan/oxiarc) project
— a Pure Rust archive and compression library ecosystem.

## Documentation

Full API documentation: <https://docs.rs/oxiarc-tiff>

## License

Apache-2.0
