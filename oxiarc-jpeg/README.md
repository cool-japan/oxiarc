# oxiarc-jpeg [Under construction]

Pure Rust JPEG (ITU-T T.81 / ISO/IEC 10918-1) codec for OxiArc. No C, no FFI,
no `unsafe`.

![Version](https://img.shields.io/badge/version-0.4.2-blue)
![License](https://img.shields.io/badge/license-Apache--2.0-green)
![Status](https://img.shields.io/badge/status-decoder%20complete-yellow)

**Version 0.4.2 (unreleased)** — the decoder half of the crate is complete and
byte-parity verified. The encoder and the arithmetic entropy coder land in
follow-on work in the same development cycle.

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
| `SOF9`/`10`/`11` | Arithmetic entropy coding | `Unsupported::ArithmeticCoding` (follow-on) |
| `SOF5`/`6`/`7`/`13`/`14`/`15` | Hierarchical | `Unsupported::Hierarchical` (no reference encoder exists) |

Plus: restart markers with resync, `DNL`-resolved heights, one to four
components, every sampling factor in `1..=4` (including 4:1:1 and 1x2),
interleaved and non-interleaved scans, `APPn`/`COM` passthrough with `JFIF`,
EXIF, XMP, ICC (`APP2` chunk reassembly) and Adobe `APP14` recognition, and
`DAC` parse-and-store so the arithmetic decoder can be added without touching
the parser.

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
both fancy and box modes, progressive (including a scan script that forces
several AC-refinement passes), restart intervals, optimized Huffman tables and
12-bit frames. Lossless frames round-trip the source samples exactly, in both
interleaved and non-interleaved colour scans. Four-component CMYK is checked
end to end against a real `tiffcp` fixture.

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

## Features

| Feature | Default | What it does |
|---|---|---|
| `arithmetic` | on | Arithmetic entropy coding (`SOF9`/`10`/`11`). The `DAC` segment is always parsed, stored and re-emitted; this feature gates the entropy coder itself. |
| `jpeg-oracle` | off | Live differential tests against `cjpeg`/`djpeg`/`tiffcp`/Pillow. Self-skips when the tools are absent. |

## Limits and safety

Every allocation is checked against a `DecodeLimits` entry *before* it
happens: frame dimensions, pixel count, component count, scan count, the
progressive coefficient buffer, the output buffer and the input buffer.
`DecodeLimits::strict()` tightens all of them for untrusted input.

`tests/corrupt_no_panic.rs` walks every single-byte truncation of every
embedded fixture, a dense grid of single-byte corruptions, every segment-length
mutation, a set of structural attacks (`Nf = 0`, `H/V = 0`, over-subscribed
Huffman tables at every length, a 2 000-scan progressive bomb, restart soup)
and 2 000 pseudo-random buffers — all under strict limits, all asserting
`Ok`-or-`Err` and a wall-clock bound.

## Performance

Measured on this machine against libjpeg-turbo 3.1.4.1 via `tjbench`, single
threaded, 512x512 synthetic source. Our figure includes buffering the input,
which `tjbench` excludes:

| Fixture | oxiarc-jpeg | `tjbench` | ratio |
|---|---|---|---|
| baseline 4:2:0 q75 | 3.10 ms (84.7 Mpx/s) | 331.7 fps (86.9 Mpx/s) | 0.97x |
| baseline 4:4:4 q95 | 9.29 ms (28.2 Mpx/s) | 105.4 fps (27.6 Mpx/s) | 1.02x |
| grayscale q75 | 1.56 ms (168 Mpx/s) | 441.5 fps (115.7 Mpx/s) | 1.45x |
| progressive q75 | 4.32 ms (60.7 Mpx/s) | — | — |

`cargo bench -p oxiarc-jpeg` regenerates these; the fixtures are built with
`cjpeg` when it is on `PATH` and the harness falls back to the embedded sample
otherwise.

## What this crate does not do

It decodes JPEG and hands metadata back verbatim. It does not interpret EXIF,
apply ICC profiles, honour orientation, resize or otherwise process images.

## License

Apache-2.0.
