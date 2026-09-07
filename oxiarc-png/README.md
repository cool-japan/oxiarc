# oxiarc-png [In Development]

Pure Rust PNG (ISO/IEC 15948, W3C PNG 3rd Edition) and APNG codec, part of the OxiArc ecosystem.

![Version](https://img.shields.io/badge/version-0.4.2-blue)
![Tests](https://img.shields.io/badge/tests-303%20passing-brightgreen)
![License](https://img.shields.io/badge/license-Apache--2.0-green)
![Status](https://img.shields.io/badge/status-Decoder%20%2B%20Encoder%20%2B%20APNG%20complete-brightgreen)

**Version: 0.4.2 (2026-09-07) | 303 tests passing (316 with `--all-features`, i.e.
`parallel` + `png-oracle`; 30 of the 303 are doctests)**

The decoder, the encoder, and the APNG compositor are complete and validated against
CPython Pillow in both directions — including Pillow's own APNG writer/reader as an
independent cross-check of frame composition. `use oxiarc_png as png;` compiles and
runs correctly against every real-world call site catalogued in this crate's design
survey; see [Compatibility with the `png` crate](#compatibility-with-the-png-crate)
below. `parallel` (rayon-backed per-row filter selection on encode) is implemented and
off by default; a `compat-017` shim for the older `png` 0.17 signature was deliberately
not built — the design survey found zero real call sites against it — see `TODO.md`.

## Features

- **Pure Rust** — no C, no FFI, `#![forbid(unsafe_code)]`, built on `oxiarc-deflate`
- **Every colour type and bit depth, both directions** — grayscale 1/2/4/8/16, RGB 8/16,
  palette 1/2/4/8, grayscale+alpha 8/16, RGBA 8/16, each with and without `tRNS`, encoded
  and decoded, interlaced and not — including the 1x1, `width < 5` and `height == 1`
  Adam7 edge cases on the *write* side too, exercised end to end through the real
  encoder in `tests/encoder_roundtrip.rs`, not only unit-tested in isolation
- **Seven filter strategies** — `NoFilter`, `Sub`, `Up`, `Avg`, `Paeth`, `Adaptive`
  (libpng's minimum-sum-of-absolute-differences heuristic, the default) and `MinEntropy`
  (an entropy estimate, as `oxipng` does), all cross-checked against Pillow
- **One continuous zlib stream per frame** — every scanline is fed directly to
  `oxiarc_deflate::Deflater::deflate` with LZ77 history carried across rows; `IDAT`
  boundaries are a framing choice, never a stream reset (`ZlibStreamEncoder`'s automatic
  128 KiB `sync_flush` would silently cost ratio for no benefit, so it is never used here)
- **`StreamWriter`** — bounded-memory encoding for images too large to hold in memory at
  once, one `Write` call per scanline's worth of bytes
- **APNG, both directions** — `acTL`/`fcTL`/`fdAT` with correct shared sequence
  numbering; a compositor (`ApngDecoder::next_composed`) implementing all three dispose
  operators and both blend operators into an RGBA8/RGBA16 canvas, proven against
  hand-computed pixel math *and* against Pillow's own `save_all=True` APNG writer/reader
- **Bomb-proof by construction** — the raw byte count is fixed by `IHDR` before anything
  is inflated, the decompressor is never offered more output space than the remainder of
  the current scanline, and the APNG compositor's canvas allocations are checked against
  the same `DecodeLimits::max_alloc_bytes` a single frame buffer is, not left to trust
  `IHDR`'s width/height unchecked
- **Every ancillary chunk, both directions** — `PLTE`, `tRNS`, `gAMA`, `cHRM`, `sRGB`,
  `iCCP`, `cICP`, `mDCv`, `cLLi`, `sBIT`, `bKGD`, `hIST`, `pHYs`, `sPLT`, `tIME`,
  `tEXt`/`zTXt`/`iTXt`, `eXIf`, `oFFs`, `sCAL`, `pCAL`, `sTER` — plus **retention of
  unknown ancillary chunks** on decode, which the `png` crate discards
- **Apple `CgBI`** — raw-DEFLATE image data, BGR(A) channel order and the premultiplied
  alpha flag, decoded in the lenient default and rejected by name in strict mode
- **`png`-crate-shaped API, proven substitutable** — `use oxiarc_png as png;` compiles
  and runs every real-world call site this crate was designed against (`tests/compat_surface.rs`),
  decode and encode alike, with `Decoder<R: Read>` instead of `R: BufRead + Seek`
- **Bounded whole-image decode** — `decode()` allocates the frame on the caller's behalf,
  so it applies `DecodeLimits::max_alloc_bytes` before the allocation; the row-by-row path
  allocates two scanlines and is deliberately exempt

## Quick Start

```toml
[dependencies]
oxiarc-png = "0.4.2"
```

### One-shot decode

```rust,no_run
let image = oxiarc_png::decode(&std::fs::read("image.png")?)?;
println!("{}x{} {:?}/{:?}", image.width, image.height, image.color_type, image.bit_depth);
let rgba = image.to_rgba8()?;
# Ok::<(), Box<dyn std::error::Error>>(())
```

### One-shot encode

```rust,no_run
use oxiarc_png::{BitDepth, ColorType, Encoder};
let mut out = std::fs::File::create("out.png")?;
let mut encoder = Encoder::new(&mut out, 2, 2);
encoder.set_color(ColorType::Rgba);
encoder.set_depth(BitDepth::Eight);
let mut writer = encoder.write_header()?;
writer.write_image_data(&[255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 0, 255])?;
writer.finish()?;
# Ok::<(), Box<dyn std::error::Error>>(())
```

### Row by row, from any reader

```rust,no_run
use std::fs::File;
let decoder = oxiarc_png::Decoder::new(File::open("image.png")?);
let mut reader = decoder.read_info()?;
let (color, depth) = reader.output_color_type();
while let Some(row) = reader.next_row()? {
    let _pixels: &[u8] = row.data();
}
# Ok::<(), oxiarc_png::DecodingError>(())
```

### Push decoding

```rust,no_run
use oxiarc_png::{Decoded, StreamingDecoder};
let mut decoder = StreamingDecoder::new();
let mut pos = 0;
# let bytes: Vec<u8> = Vec::new();
while pos < bytes.len() {
    let (consumed, event) = decoder.update(&bytes[pos..], None)?;
    pos += consumed;
    if let Decoded::Header { width, height, .. } = event {
        println!("{width}x{height}");
    }
    if consumed == 0 {
        break;
    }
}
# Ok::<(), oxiarc_png::DecodingError>(())
```

### APNG: compose every animation frame

```rust,no_run
let mut dec = oxiarc_png::ApngDecoder::new(std::fs::File::open("anim.png")?)?;
while let Some(frame) = dec.next_composed()? {
    let _rgba: &[u8] = frame.canvas; // whole canvas, already blended and disposed
}
# Ok::<(), oxiarc_png::DecodingError>(())
```

## API Overview

| Item | Kind | Description |
|------|------|-------------|
| `decode(&[u8]) -> Result<Image>` | function | Whole-file decode in the file's own colour type |
| `decode_rgba8(&[u8]) -> Result<Image>` | function | Whole-file decode expanded to 8-bit RGBA |
| `decode_with(&[u8], DecodeOptions, Transformations)` | function | Whole-file decode with explicit policy |
| `decode_reader<R: Read>(R)` | function | The same from any reader |
| `peek_info(&[u8]) -> Result<Info>` | function | Metadata only; no image data is inflated |
| `is_png(&[u8]) -> bool` | function | Signature check |
| `Decoder<R: Read>` | struct | Pull decoder: `read_header_info`, `read_info` |
| `Reader<R: Read>` | struct | `next_frame`, `next_row`, `next_interlaced_row`, `read_row`, `next_frame_info`, `output_buffer_size`, `output_color_type`, `finish` |
| `StreamingDecoder` | struct | Push decoder: `update(buf, Option<&mut UnfilterBuf>) -> (usize, Decoded)` |
| `Encoder<'a, W: Write>` | struct | Header builder: `set_color`, `set_depth`, `set_palette`, `set_trns`, `set_compression`, `set_filter`, `set_interlaced`, `set_animated`, `write_header` |
| `Writer<W: Write>` | struct | `write_image_data`, `write_chunk`, `write_text_chunk`, `stream_writer`/`into_stream_writer` (+ `_with_size`), `set_frame_dimension`/`set_frame_position`, `finish` |
| `StreamWriter<'a, W: Write>` | struct | `Write` adapter for one frame's pixels, bounded memory; `remaining`, `finish` |
| `ApngDecoder<R: Read>` | struct | `next_subframe` (as stored), `next_composed` (fully blended canvas), `num_frames`, `num_plays` |
| `ApngEncoder<W: Write>` | struct | `write_frame(&FrameControl, &[u8])` (RGBA8 only), `set_default_image_is_first_frame`, `finish` |
| `Image` | struct | `width`, `height`, `color_type`, `bit_depth`, `data`, `info`; `to_rgba8`, `to_rgba16`, `to_rgb8`, `to_luma8` |
| `Info<'a>` | struct | Every parsed chunk, including `unknown_chunks`, `time`, `splt`, `hist`, `offs`, `scal`, `pcal`, `ster`, `cgbi` |
| `DecodeOptions` | struct | `set_strict`, `set_ignore_adler32`, `set_ignore_crc`, `set_skip_ancillary_crc_failures`, `set_retain_unknown_chunks`, `set_limits` |
| `DecodeLimits` | struct | Dimensions, pixels, allocation, chunk length, text, ICC, unknown chunks, frames, animation total — applied to the APNG compositor's canvas too, not only a single frame buffer |
| `Limits { bytes }` | struct | The `png`-shaped budget, default 64 MiB |
| `Transformations` | struct | `IDENTITY`, `EXPAND`, `ALPHA`, `STRIP_16`, `normalize_to_color8()` |
| `Filter` | enum | `NoFilter`, `Sub`, `Up`, `Avg`, `Paeth`, `Adaptive` (default), `MinEntropy` |
| `chunk` | module | `ChunkType`, all constants, `is_critical`/`is_private`/`reserved_set`/`safe_to_copy`, `write_chunk`, `ChunkIter` |
| `filter` | module | `RowFilter`, `Filter`, `unfilter`, `apply_filter`, `select_filter`, the three Paeth forms |
| `interlace` | module | `PASSES`, `pass_dimensions`, `Adam7Info`, `Adam7Iterator`, `expand_pass`, `expand_pass_splat`, `extract_pass_row` (the encoder's write-side mirror) |
| `text_metadata` | module | `TEXtChunk`, `ZTXtChunk`, `ITXtChunk`, `validate_keyword`, `DECOMPRESSION_LIMIT` |

## Compatibility with the `png` crate

`use oxiarc_png as png;` is designed to compile unchanged against every real-world call
site this crate was surveyed against — `Decoder`/`Reader`/`Encoder`/`Writer` and their
methods, `ColorType`/`BitDepth`/`Compression`/`Transformations`, palette writing, 16-bit
depths, `BufWriter`-wrapped and `Cursor`-wrapped targets. `tests/compat_surface.rs`
reproduces each surveyed call site (named after the file:line it stands in for) and
*runs* it — not just type-checks it — so a future signature change fails there before it
can break a downstream `Cargo.toml` swap.

| Area | `png` 0.18 | `oxiarc-png` |
|---|---|---|
| `Decoder<R>` bound | `R: BufRead + Seek` | `R: Read` (strictly wider) |
| Unknown ancillary chunks | discarded | retained in `Info::unknown_chunks`, capped by `DecodeLimits` |
| `tIME`/`sPLT`/`hIST`/`oFFs`/`sCAL`/`pCAL`/`sTER` | not parsed | parsed into `Info` |
| Apple `CgBI` | confusing zlib error | decoded, flagged in `Info::cgbi` |
| Adler-32 | ignored by default | ignored by default (same) |
| Ancillary CRC failure | chunk dropped | chunk dropped (same) |
| Palette index out of range | opaque black | opaque black, or an error under `set_strict` |
| Strict mode | absent | `DecodeOptions::set_strict` |
| Default `IDAT`/`fdAT` chunk size | one ~2 GiB chunk (`u32::MAX >> 1`) for any realistic image | 64 KiB, configurable via `set_idat_chunk_size` — smaller on purpose, so a chunk-aware reader need not buffer gigabytes |
| `StreamWriter` default buffer | 4 KiB | 4 KiB (same); note this differs from `write_image_data`'s 64 KiB default, so the two paths' chunk *boundaries* differ unless explicitly matched (pixel content is identical either way) |
| Interlaced encoding | not supported at all, streaming or otherwise | supported via `Writer::write_image_data` (not `StreamWriter`, which needs the whole image up front for Adam7 the same reason `png` 0.18 has no interlaced writer) |
| APNG compositing | not provided (frame-by-frame only) | `ApngDecoder::next_composed` composites dispose/blend into a canvas |
| Default image (whatever an animated encoder writes as `IDAT`) smaller than the canvas | accepted, producing a file no decoder can read — with an `fcTL` it is out-of-bounds, and under `set_sep_def_img` the `IDAT` stream is simply short of what `IHDR` demands (`png`'s own `set_frame_position` carries a `// ??? TODO ??? - The next frame is the default image`) | rejected with `OutOfBounds`; only `fdAT` sub-frames may be smaller than the canvas |
| `Encoder::with_info` frame rectangle | not validated | validated on construction (empty or out-of-canvas rectangles are refused, instead of panicking later in the row loop) |
| `sPLT` 8-bit sample > 255, `mDCv` chromaticity out of the chunk's `u16` range | n/a (neither chunk is written) | refused, never silently truncated into a different colour |

The output colour-type table is reproduced **exactly**, row by row, and pinned by
`tests/transform_table.rs`; the `image` crate matches exhaustively on it.

### Migrating from `png`

```toml
[dependencies]
png = { package = "oxiarc-png", version = "0.4" }
```

For a codebase still on `png` **0.17**, use the `v017` module instead of the
crate root — `use oxiarc_png::v017 as png;` — which restores the four names
whose shape changed in 0.18: `Reader::output_buffer_size() -> usize` (not
`Option<usize>`), and the split `FilterType` / `AdaptiveFilterType` pair with
`set_filter` / `set_adaptive_filter`. It is a module rather than a Cargo
feature on purpose: a feature that changed a public signature would unify
across the dependency graph, so one crate enabling it would reshape the API
every other crate sees.

Then leave `use png::...` and every call site alone — the table above lists what *does*
differ; anything not listed there behaves identically for the surveyed call sites. If a
project already writes `use png as _;`-style aliasing the other way, swap the dependency
name in `Cargo.toml` only; no source changes are needed for the closed set of
types/methods `tests/compat_surface.rs` exercises.

## Testing

| Suite | What it covers |
|---|---|
| `src/**` unit tests | Chunk framing, CRC, the 2^24 Paeth equivalence proof, Adam7 partitioning (both directions), filter selection and inverses, every ancillary parser and its encoder-side emitter, the push state machine's ordering and policy rules, the APNG compositor's dispose/blend math against hand-computed pixel values, canvas allocation limit checks |
| `tests/decode_basics.rs` | Every legal colour type x bit depth x interlace at six image sizes (decode side) |
| `tests/encoder_roundtrip.rs` | The same matrix through the *real* `Encoder`/`Writer` and back through the decoder — interlaced, indexed+palette, 16-bit and sub-byte depths, every filter strategy, `NoPalette` rejection |
| `tests/compat_surface.rs` | `use oxiarc_png as png;` against every real-world call site this crate was designed against, decode and encode, run (not only compiled) |
| `tests/transform_table.rs` | The output colour-type table, hand-written row by row, plus a real decode per row |
| `tests/streaming.rs` | Byte-at-a-time, chunked and interrupted reads; 1-byte `IDAT` chunks |
| `tests/robustness.rs` | Truncation at every offset, three mutations at every offset, garbage, limits, compression bombs, `CgBI`, zlib header conformance |
| `tests/apng.rs` | Two-frame decode, sequence gaps, `fdAT` without `fcTL`, out-of-bounds sub-frames |
| `tests/props.rs` | Property tests: decode-side round trips, filter inverses, chunked-feed equivalence, `expected_raw`, full encode-then-decode round trips through the real `Encoder`, and APNG composition never panicking with a full-size canvas on every frame |
| `tests/pillow_oracle.rs` (`png-oracle`) | Differential against CPython Pillow in both directions — decoder *and* encoder output, every colour/depth/interlace/filter combination, an APNG dispose/blend cross-check against Pillow's own writer and reader (plus a cross-check against Pillow's own correct `alpha_composite` primitive, since Pillow 12.1.0's APNG plugin itself has an alpha-blending bug — see `TODO.md`), and a compression-ratio regression guard against `python3 zlib.compress(level=6)` |

```bash
cargo test -p oxiarc-png
cargo test -p oxiarc-png --features parallel
cargo test -p oxiarc-png --features png-oracle   # requires python3 + numpy + Pillow
cargo bench -p oxiarc-png
```

## Feature flags

| Feature | Default | Description |
|---|---|---|
| `png-oracle` | off | Differential tests against CPython Pillow; self-skips when unavailable |
| `parallel` | off | Rayon-backed parallel per-row filter *selection* on encode; small frames fall back to serial automatically. DEFLATE always stays serial — see `TODO.md` for why `oxiarc-deflate`'s own parallel compressor can't be used for a PNG `IDAT`/`fdAT` stream |

A `compat-017` shim for the older `png` 0.17 signature was surveyed and deliberately not
built: zero real call sites against 0.17's API turned up across the ~27 projects this
crate's design was surveyed against (0.17 appears only transitively, in some lockfiles).
See `TODO.md` for the full triage rationale.

## License

Apache-2.0
