# oxiarc-tiff

Pure Rust TIFF 6.0 / BigTIFF reader and writer, part of the OxiArc ecosystem.

![Version](https://img.shields.io/badge/version-0.4.2-blue)
![Tests](https://img.shields.io/badge/tests-434%20passing-brightgreen)
![License](https://img.shields.io/badge/license-Apache--2.0-green)
![Status](https://img.shields.io/badge/status-complete-brightgreen)

**Version: 0.4.2 (2026-09-07) | 434 tests + 36 doctests passing (`--all-features`)**

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

`tests/compat_api.rs` reproduces the exact call sequence `image` 0.25.10
performs (dimensions, colour type, `read_image_to_buffer`, the ICC-profile
and orientation tag reads, both exhaustive-`TiffError`-match sites, the
`TiffEncoder`/`ImageEncoder` write path with an ICC tag) so a shape break
fails a compile or a test here, not downstream. `compat::encoder::colortype`
carries all thirty upstream colour-type marker types
(`Gray8` .. `CMYKA8`), not only the eight `image` names directly.

Deliberate, documented deviations (never a silent behaviour change):
`ImageEncoder`'s `resolution*` setters return `TiffResult<()>` instead of
`unwrap()`-ing internally (this crate has no `unwrap()` in library code, and
upstream's own users never call these); `DecodingBuffer::to_bytes` returns an
owned `Vec<u8>` rather than a zero-copy view (this crate is
`#![forbid(unsafe_code)]`, and a zero-copy numeric-slice-to-bytes view needs
one); `encoder::Predictor` is a re-export of `tags::Predictor` rather than a
second, independently-defined enum of the same shape. See the `compat` module
docs (`cargo doc --features compat --open`) for the complete list.

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
- **Group 3/4 uncompressed mode** (`T4Options` bit 1, `T6Options` bit 1) is
  detected and reported by name, not decoded — libtiff 4.7.1 answers the same
  data with "Uncompressed data (not supported)". The option bits are parsed and
  never written.
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

**The design target was 1.25x, and the compressed codecs do not meet it.** An
independent re-measurement on poorly-compressible (photograph-like) 4000x3000
RGB8 data, best of five, put LZW at 1.63x, Deflate at 1.24x, ZSTD at 2.77x and
LZMA at 1.12x of `tiffcp -c none`; uncompressed at 0.09x and PackBits at 0.18x.
Subtracting this crate's own no-codec decode time from each figure leaves
essentially the whole gap inside the codec crate, so closing it is work for
`oxiarc-lzw` / `oxiarc-zstd`, not for `oxiarc-tiff`.

**`rayon` on/off**, 4096x4096 Gray8, PackBits, 256x256 tiles (`tiff_rayon_decode`/
`tiff_rayon_encode` groups, `--quick`, Apple M-series). Measure these on an
**idle** machine: they are the most load-sensitive numbers in this file, since
the parallel arm needs free cores and the serial arm does not. Re-runs on a
loaded 8-core box (load average 19-32) moved the untouched *encode* pair by 3x
between consecutive runs and inverted the decode pair outright, so treat any
single reading taken under load as noise rather than as a regression:

| Group | Serial | Parallel | Speedup |
|---|---|---|---|
| decode | 6.6 ms | 4.0 ms | 1.7x |
| encode | 16.8 ms | 5.4 ms | 3.1x |

Encode gains more than decode because compression (PackBits here; the effect
is larger still for Deflate/LZMA/ZSTD) dominates encode time and parallelises
cleanly, while decode's win is capped by the serial fetch pass (one `Read +
Seek` handle) and — for codecs whose scratch lives behind `CodecState`'s
`Mutex` (Deflate, CCITT) — by that lock. See the `rayon_support` module docs
for the full breakdown of which codecs benefit.

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
