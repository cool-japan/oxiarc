# oxiarc-jpeg [Under construction]

Pure Rust JPEG (ITU-T T.81 / ISO/IEC 10918-1) codec for OxiArc. No C, no FFI,
no `unsafe`.

![Version](https://img.shields.io/badge/version-0.4.2-blue)
![License](https://img.shields.io/badge/license-Apache--2.0-green)
![Status](https://img.shields.io/badge/status-codec%20complete-yellow)

**Version 0.4.2 (unreleased)** — the decoder and the encoder are both complete
and byte-parity verified against libjpeg-turbo. The arithmetic entropy coder
lands in follow-on work in the same development cycle.

## Overview

`oxiarc-jpeg` exists because there is no Pure Rust JPEG codec that can read and
write **abbreviated datastreams** — the tables-only / scan-only split that
TIFF's `Compression = 7` (`JPEGTables`, tag 347) is built on. `oxiarc-tiff`
needs that split; so do GeoTIFF, DICOM and every scanner that emits
JPEG-in-TIFF. The crate is also a drop-in JPEG decoder for anything that would
otherwise pull `jpeg-decoder`, `zune-jpeg` or the `image` facade.

## What is decoded

| `SOF` | Process | Status |
|---|---|---|
| `SOF0` | Baseline sequential DCT, 8-bit | decoded |
| `SOF1` | Extended sequential DCT, 8- and 12-bit | decoded |
| `SOF2` | Progressive DCT, 8- and 12-bit | decoded |
| `SOF3` | Lossless predictive, 2..=16 bit, predictors 1-7 | decoded |
| `SOF9` | Extended sequential DCT, arithmetic, 8- and 12-bit | decoded (feature `arithmetic`, on by default) |
| `SOF10` | Progressive DCT, arithmetic | decoded (feature `arithmetic`) |
| `SOF11` | Lossless predictive, arithmetic, 2..=16 bit | decoded (feature `arithmetic`) |
| `SOF5`/`6`/`7`/`13`/`14`/`15` | Hierarchical | `Unsupported::Hierarchical` (no reference encoder exists) |

Plus: restart markers with resync, `DNL`-resolved heights, one to four
components, every sampling factor in `1..=4` (including 4:1:1 and 1x2),
interleaved and non-interleaved scans, `APPn`/`COM` passthrough with `JFIF`,
EXIF, XMP, ICC (`APP2` chunk reassembly) and Adobe `APP14` recognition, and
`DAC` conditioning.

Legacy **OJPEG** (TIFF `Compression = 6`) is reconstructed and decoded by
`oxiarc_jpeg::tiff` — see below.

## Accuracy: byte parity, not a tolerance

Every stage reproduces libjpeg's arithmetic exactly:

- the inverse DCT is `jpeg_idct_islow` with `CONST_BITS = 13` and
  `PASS1_BITS = 2` (1 above 8-bit samples, as libjpeg does);
- the chroma upsamplers are libjpeg's three fancy kernels — `h2v1`, `h1v2`,
  `h2v2` — with their asymmetric rounding constants and libjpeg's
  `downsampled_width > 2` guard, and replication for every other ratio;
- YCbCr to RGB is the `SCALEBITS = 16` fixed-point form with libjpeg-turbo's
  five-decimal constants.

So 8-bit output is **byte-identical to `djpeg -dct int`**, which is what the
`jpeg-oracle` suite asserts across grayscale, 4:4:4, every subsampling ratio in
both fancy and box modes -- including the odd ones (3x1, 1x3, 3x2, 2x3) and
per-component asymmetric ones such as `-sample 2x2,1x1,2x1`, where Cb and Cr
drive *different* upsamplers in one image -- progressive (including a scan
script that forces several AC-refinement passes, progressive grayscale and
progressive frames carrying restart markers), restart intervals, optimized
Huffman tables, and 12-bit frames in both grayscale and colour. Lossless frames
round-trip the source samples exactly, in both interleaved and non-interleaved
colour scans. Four-component CMYK is checked end to end against a real `tiffcp`
fixture.

The tests covering options every libjpeg-turbo build has -- quality,
subsampling, progressive, restarts, optimized Huffman -- count their
comparisons and **fail** if `cjpeg` produced no fixture, so a renamed encoder
flag cannot turn the suite green while comparing nothing. The six that need a
build-time capability (`-lossless` and `-precision 12`) still skip with a
printed note, because their absence is a property of the local libjpeg rather
than a regression; the `cjpeg -version` banner is recorded in every failure
message so it is clear which build a result came from.

The one deliberate deviation: an out-of-range reconstruction is **clamped**
rather than run through libjpeg's wrapping range-limit table. Conforming
streams never reach the wraparound region; clamping is the safer behaviour on
corrupt input.

## What is encoded

| `SOF` | Process | Status |
|---|---|---|
| `SOF0` | Baseline sequential DCT, 8-bit | encoded |
| `SOF1` | Extended sequential DCT, 8- and 12-bit | encoded |
| `SOF2` | Progressive DCT, default and custom scan scripts | encoded |
| `SOF3` | Lossless predictive, 2..=16 bit, predictors 1-7 | encoded |

Quality 1..100 through libjpeg's scaling formula, `force_baseline`, dynamic
`DQT` `Pq` selection, standard Annex K.3 and generated (`-optimize`) Huffman
tables, box and smoothed chroma decimation at 4:4:4 / 4:2:2 / 4:4:0 / 4:2:0 /
4:1:1 and any exact integer ratio, MCU edge replication, restart intervals in
MCUs or MCU rows, `JFIF` `APP0` with density, Adobe `APP14`, EXIF / XMP / ICC /
`COM` passthrough, grayscale / YCbCr / RGB (no transform) / CMYK / YCCK, and
TIFF's abbreviated tables-only and scan-only halves.

## Arithmetic coding

The QM coder of T.81 Annex D is implemented in both directions, for all three
arithmetic processes, and is held to the same standard as the rest of the
crate: **byte identity with libjpeg**, not a tolerance.

- decoding an arithmetic stream produces exactly what `djpeg -dct int` does,
  across every subsampling ratio, quality, restart spelling, grayscale,
  12-bit and progressive scan script;
- encoding produces exactly the bytes `cjpeg -arithmetic -dct int` writes,
  including the per-scan `DAC` segment (libjpeg emits one for every table a
  scan uses, even at the T.81 default values, and none at all for a DC
  refinement scan — all four rules are reproduced).

Set it with `EncodeOptions { entropy: EntropyCoding::Arithmetic, .. }`;
`EncodeOptions::arithmetic` carries the conditioning bounds, which default to
T.81's `L = 0`, `U = 1`, `Kx = 5`.

**`SOF11` (lossless arithmetic) has no reference implementation anywhere.**
libjpeg-turbo refuses `-lossless` together with `-arithmetic` at compile time
and its arithmetic decoder has no lossless path, so this crate implements
T.81 Annex H.1.2.3 directly — the two-dimensional statistical model with 158
bins per area, conditioned on the differences coded to the left *and* above.
Its gate is exact round-tripping across every predictor, precision, point
transform, restart interval and custom conditioning, and that is stated
plainly rather than dressed up as verification against a reference.

## Legacy OJPEG (TIFF `Compression = 6`)

TIFF 6.0's withdrawn JPEG encoding stores the tables in tags and may leave a
strip with no markers at all. `oxiarc_jpeg::tiff::reconstruct_ojpeg` rebuilds
a decodable datastream from the resolved tag payloads, and `decode_ojpeg`
decodes it. All three spellings found in the wild are handled by libtiff's
strategy — scan the strip for what it already has, synthesise the rest:

| Flavour | Shape | Source of the tables |
|---|---|---|
| (a) interchange | tags 513/514 hold a whole datastream | the stream's own header |
| (b) spec form | the strip is bare entropy data | tags 519/520/521 |
| (c) hybrid | the strip starts at `SOS` or an `RSTn` | the strip first, then the tags |

The caller resolves every tag offset to bytes: reading a file is the TIFF
layer's job. No colour transform is applied — `PhotometricInterpretation`,
`ReferenceBlackWhite` and `YCbCrCoefficients` belong to the container. Note
that libtiff's *own* YCbCr conversion for an OJPEG strip differs from
libjpeg's by up to ~100 LSB on high-contrast material, so a container that
wants libjpeg's colours must do the conversion itself.

Writing `Compression = 6` is not supported and never will be: TTN2 deprecates
it and libtiff itself refuses.

## Quick start

```rust
use oxiarc_jpeg::{ColorSpace, Decoder, JpegError};

fn main() -> Result<(), JpegError> {
    let bytes: &[u8] = &oxiarc_jpeg::sample::GRAY_1X1;
    let mut decoder = Decoder::new(bytes);
    let info = decoder.read_info()?;
    assert_eq!((info.width, info.height), (1, 1));
    assert_eq!(info.output_color_space, ColorSpace::Luma);

    let pixels = decoder.decode()?;
    assert_eq!(pixels.len(), 1);
    Ok(())
}
```

### TIFF `Compression = 7`, with no buffer concatenation

```rust
use oxiarc_jpeg::{DecodeOptions, JpegError, TableSet, decode_abbreviated_into};

fn main() -> Result<(), JpegError> {
    // Tag 347 is parsed once for the whole image ...
    let tables = TableSet::parse(&oxiarc_jpeg::sample::RGB_8X8_420_TABLES)?;

    // ... and every strip or tile decodes against it directly.
    let strip: &[u8] = &oxiarc_jpeg::sample::RGB_8X8_420_SCAN;
    let mut out = vec![0u8; 8 * 8 * 3];
    let info = decode_abbreviated_into(
        Some(&tables),
        strip,
        &DecodeOptions::raw(), // raw components: TIFF owns the colour pipeline
        &mut out,
    )?;
    assert_eq!((info.width, info.height), (8, 8));
    Ok(())
}
```

A tiled reader can write straight into a sub-rectangle of the destination
image with `Decoder::decode_into_strided`, so a 50 000-tile GeoTIFF pays no
per-tile copy at all.

### Encoding

```rust
use oxiarc_jpeg::{EncodeOptions, EncodeProcess, Encoder, InputColor, Subsampling};

let pixels = vec![128u8; 64 * 48 * 3];
let mut out = Vec::new();
let mut encoder = Encoder::new(&mut out);
encoder
    .set_quality(90)
    .set_subsampling(Subsampling::S444)
    .set_process(EncodeProcess::Progressive);
encoder.add_comment(b"made by oxiarc-jpeg")?;
encoder.encode(&pixels, 64, 48, InputColor::Rgb)?;
encoder.finish()?;
# Ok::<(), oxiarc_jpeg::JpegError>(())
```

Writing the two halves of a TIFF `Compression = 7` image:

```rust
use oxiarc_jpeg::{EncodeOptions, Encoder, InputColor, TablesMode, table_set};

let options = EncodeOptions::tiff_strip(75);

// Tag 347 — byte-identical to what libtiff writes at the same quality.
let tag_347 = table_set(&options, InputColor::Rgb)?.emit(TablesMode::BOTH);

// One strip, with every table segment suppressed.
let mut strip = Vec::new();
let mut encoder = Encoder::with_options(&mut strip, options);
encoder.encode_scan_only(&vec![0u8; 64 * 16 * 3], 64, 16, InputColor::Rgb)?;
encoder.finish()?;
assert_eq!(&strip[..4], &[0xFF, 0xD8, 0xFF, 0xC0]);
# Ok::<(), oxiarc_jpeg::JpegError>(())
```

Call sites that only need `image`'s encoder can move with a `use` change:
`oxiarc_jpeg::compat::JpegEncoder::new_with_quality(writer, 85)` mirrors
`image::codecs::jpeg::JpegEncoder`.

## Features

| Feature | Default | What it does |
|---|---|---|
| `arithmetic` | on | Arithmetic entropy coding (`SOF9`/`10`/`11`). The `DAC` segment is always parsed, stored and re-emitted; this feature gates the entropy coder itself. |
| `rayon` | off | Entropy coding across restart intervals, in parallel, for both coders and in both directions. Byte-identical output either way, which `tests/parallel.rs` pins with digests of the encoded bytes and of the decoded samples, both measured in the serial configuration and asserted in both. |
| `jpeg-oracle` | off | Live differential tests against `cjpeg`/`djpeg`/`tiffcp`/Pillow. Self-skips when the tools are absent. |

The crate builds and passes its tests with `--no-default-features`, with
`--all-features`, and with each feature on its own.

## Limits and safety

Every allocation is checked against a `DecodeLimits` entry *before* it
happens: frame dimensions, pixel count, component count, scan count, the
progressive coefficient buffer, the output buffer and the input buffer.
`DecodeLimits::strict()` tightens all of them for untrusted input.

`tests/corrupt_no_panic.rs` walks every single-byte truncation of every
fixture, a dense grid of single-byte corruptions, every segment-length
mutation, a set of structural attacks (`Nf = 0`, `H/V = 0`, over-subscribed
Huffman tables at every length, a 2 000-scan progressive bomb, restart soup)
and 2 000 pseudo-random buffers, over Huffman *and* arithmetic fixtures of
every process — all under strict limits, all asserting
`Ok`-or-`Err` and a wall-clock bound. The corpus is the six embedded samples
plus the five multi-MCU streams in `tests/data` (progressive, baseline with
`DRI`, progressive with `DRI`, 12-bit and lossless), so the progressive
coefficient buffer, the restart resynchroniser, the 12-bit path and the
lossless predictors each see malformed input rather than only the 8x8 baseline
path, plus five arithmetic streams (sequential, progressive, restart,
lossless and 12-bit) that the test *builds* with this crate's own encoder
rather than committing. `tests/fixture_streams.rs` decodes the five intact
first, so the sweep cannot quietly start sweeping something that is no longer
a valid JPEG.

`tests/fuzz_seeds.rs` is the fuzz corpus generator, and nothing is committed:
it builds around 130 seeds spanning every `SOF`, both entropy coders, both
halves of the abbreviated (TIFF) pair, metadata and the awkward sizes, and
decodes every one of them on each run. Set `OXIARC_JPEG_FUZZ_SEEDS` to a
directory to have them written out:

```text
OXIARC_JPEG_FUZZ_SEEDS=$PWD/fuzz/corpus/fuzz_jpeg_decode \
  cargo test -p oxiarc-jpeg --all-features --test fuzz_seeds
```

Restart recovery is asserted, not assumed: damage confined to the first restart
interval must leave the last MCU bit-identical to a clean decode. A hand-built
progressive stream covers the one rule no encoder can witness — T.81 G.1.2.2
requires `EOBRUN` to be cleared at every restart marker, and a conforming
encoder always flushes its EOB run before `RST`, so only a synthetic stream can
tell the two behaviours apart.

## Accuracy on the encode side

The same standard applies: `tests/encode_oracle.rs` asserts **byte identity**
with `cjpeg`, not a tolerance, over

* six subsampling ratios x eleven qualities (1, 5, 10, 25, 50, 63, 75, 88, 90,
  99, 100) x six shapes including 1x1, 1x33 and 131x97;
* grayscale input, `-grayscale` from RGB and `-rgb` (no colour transform);
* `-restart N` and `-restart NB`, `-optimize`, `-smooth 1/10/50/100`;
* `-progressive` with libjpeg's default script, and a seventeen-scan `-scans`
  script asserted to contain at least three AC refinement passes;
* `-precision 12`, including the `Pq` switch that depends on the quantiser
  values rather than on the precision;
* `-lossless psv,Pt` for all seven predictors and three point transforms;
* `tiffcp`'s `JPEGTables` tag, generated from scratch rather than
  round-tripped, for YCbCr, RGB-mode, grayscale and CMYK.

Each test counts its comparisons and fails with the `cjpeg -version` banner if
it made none, so a renamed flag cannot turn the suite green while comparing
nothing.

## Performance

Decoding, measured on this machine against libjpeg-turbo 3.1.4.1 via
`tjbench`, single threaded, 512x512 synthetic source. Our figure includes
buffering the input, which `tjbench` excludes:

| Fixture | oxiarc-jpeg | `tjbench` | ratio |
|---|---|---|---|
| baseline 4:2:0 q75 | 3.10 ms (84.7 Mpx/s) | 331.7 fps (86.9 Mpx/s) | 0.97x |
| baseline 4:4:4 q95 | 9.29 ms (28.2 Mpx/s) | 105.4 fps (27.6 Mpx/s) | 1.02x |
| grayscale q75 | 1.56 ms (168 Mpx/s) | 441.5 fps (115.7 Mpx/s) | 1.45x |
| progressive q75 | 4.32 ms (60.7 Mpx/s) | — | — |

Encoding, against `cjpeg` on the same machine, minimum of 21 runs.  `cjpeg`'s
figure includes reading a PPM and writing a JPEG (about 1.3 ms of process and
file overhead), so the 512x512 ratios flatter us and the 1920x1080 row is the
honest one:

| Fixture | oxiarc-jpeg | `cjpeg` | ratio |
|---|---|---|---|
| baseline 4:2:0 q75, 512x512 | 1.94 ms (135 Mpx/s) | 2.24 ms (117 Mpx/s) | 1.15x |
| baseline 4:4:4 q95, 512x512 | 5.93 ms (44 Mpx/s) | 3.56 ms (74 Mpx/s) | 0.60x |
| baseline 4:2:0 q75, 1920x1080 | 16.4 ms (127 Mpx/s) | 6.93 ms (299 Mpx/s) | 0.42x |
| `-optimize` 4:2:0 q75, 512x512 | 2.65 ms (99 Mpx/s) | 3.20 ms (82 Mpx/s) | 1.21x |
| progressive 4:2:0 q75, 512x512 | 7.60 ms (35 Mpx/s) | 4.86 ms (54 Mpx/s) | 0.64x |
| lossless psv1, 512x512 | 10.6 ms (25 Mpx/s) | 7.15 ms (37 Mpx/s) | 0.67x |

libjpeg-turbo uses hand-written NEON for colour conversion, the forward DCT
and Huffman encoding; this crate is `#![forbid(unsafe_code)]` and has none.
The gap is widest exactly where that SIMD applies (large baseline frames) and
closes where it does not (optimised tables, progressive, lossless). Splitting
the coefficient pipeline per MCU row, so a large frame's coefficients never
leave cache, is the obvious next step and is not done.

### Arithmetic coding

Measured the same way, 512x512, single threaded. `djpeg`'s figures have 2.0 ms
of process and file overhead subtracted (measured with `djpeg -version`), so
both columns are decode time:

| Fixture | oxiarc-jpeg | `djpeg -dct int` | ratio |
|---|---|---|---|
| arithmetic 4:4:4 q95 | 26.2 ms | 31.0 ms | **1.18x** |
| arithmetic 4:2:0 q75 | 6.80 ms | 6.07 ms | **0.89x** |
| arithmetic progressive q75 | 6.74 ms | — | — |

| Fixture | oxiarc-jpeg | `cjpeg -arithmetic` | ratio |
|---|---|---|---|
| arithmetic 4:2:0 q75 encode | 6.39 ms | 5.17 ms | **0.81x** |
| arithmetic progressive q75 encode | 7.33 ms | 8.37 ms | **1.14x** |

The QM coder's inner loop is one indexed table lookup, one subtraction and a
renormalisation loop per binary decision. Padding T.81's 114-row estimation
table to 128 rows, so that a seven-bit bin index needs no bounds check, was
worth 30 % on the 4:4:4 case on its own.

### The `rayon` feature

Only the entropy coding is parallel, and only across restart intervals; the
colour conversion, the DCT and the upsampling are not. Both coders are
covered on both sides — a sequential `SOF0`/`SOF1`/`SOF9` scan whose restart
interval tiles whole MCU rows splits into bands, and each band is decoded (or
coded) into scratch of its own and merged in order, which is what keeps the
output identical and the crate free of `unsafe`.

On a 1024x1024 4:2:0 image with a restart marker per MCU row, eight threads:

| Stage | serial | `--features rayon` | ratio |
|---|---|---|---|
| decode, Huffman | 10.17 ms | 6.18 ms | **1.65x** |
| decode, arithmetic | 24.88 ms | 9.35 ms | **2.66x** |
| encode, Huffman | 8.52 ms | 6.15 ms | **1.39x** |
| encode, arithmetic | 23.97 ms | 10.81 ms | **2.22x** |

Those are Amdahl figures, not scaling figures. Entropy coding is roughly a
third of a Huffman decode, which caps that row near 1.5x however many threads
are available; it is most of an arithmetic one, which is why the QM rows gain
more. Parallelising the output stage (upsampling and colour conversion, which
are per-row and need no new synchronisation) is the obvious next step and is
not done.

The figures are the fastest of criterion's hundred samples rather than the
mean, because the machine they were measured on was shared; the mean of a
parallel run on a loaded box measures the load, not the code.

`cargo bench -p oxiarc-jpeg --all-features` regenerates every table; the
fixtures are built with `cjpeg` when it is on `PATH` and the harness falls
back to the embedded sample otherwise.

## What this crate does not do

It codes JPEG and hands metadata back verbatim. It does not interpret EXIF,
apply ICC profiles, honour orientation, resize or otherwise process images.
Hierarchical JPEG (`SOF5`/`6`/`7`/`13`/`14`/`15`) is reported as
`UnsupportedFeature::Hierarchical` rather than decoded.

## License

Apache-2.0.
