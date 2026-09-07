
# oxiarc-lzw [Stable]

Pure Rust implementation of LZW (Lempel-Ziv-Welch) compression for TIFF and GIF formats.

[![Crates.io](https://img.shields.io/crates/v/oxiarc-lzw.svg)](https://crates.io/crates/oxiarc-lzw)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)
![Status](https://img.shields.io/badge/status-Stable-brightgreen)

**Version: 0.4.2 (2026-09-07) | 130 tests passing: 119 via nextest (incl. libtiff/Pillow differential oracles) + 11 doctests**

## Overview

LZW is a dictionary-based compression algorithm used in TIFF images, GIF animations, and legacy Unix compress. This implementation provides both TIFF-style (MSB-first) and GIF-style (LSB-first) bit packing, with dedicated GIF LZW codec support via the `gif_lzw` module.


## Features

- **Pure Rust** - No C dependencies or unsafe FFI
- **TIFF support** - MSB-first bit ordering for TIFF images
- **GIF support** - LSB-first bit ordering for GIF animations via `gif_lzw` module
- **GIF LZW codec** - Dedicated `gif_compress`/`gif_decompress` functions conforming to GIF spec §22
- **LSB bitstream** - `bitstream_lsb` module with `LsbBitWriter`/`LsbBitReader` for GIF-compatible bit packing
- **Configurable** - Adjustable code width (9-12 bits)
- **Early change** - Code width increases before table full
- **Zero-allocation strip decode** - `decompress_tiff_into(src, &mut dst)` expands codes straight into a caller-supplied buffer through a prefix/suffix code table; no allocation per decoded code, and none at all beyond the fixed table (see below)
- **Old-style LZW** - `LzwConfig::TIFF_OLD_STYLE` decodes streams written with the standard (late) code-width change instead of TIFF's early change, i.e. writers that followed TIFF 6.0's pseudo-code literally
- **Reference interop** - TIFF-LZW streams are byte-compatible with libtiff/Pillow in both directions (differential-tested; see `tests/tiff_lzw_oracle.rs`, `tests/tiffcp_strip_decode.rs` and the pinned fixtures in `tests/data/`)
- **Property-tested** - `proptest`-based round-trip and no-panic fuzzing across arbitrary inputs

All features are implemented and tested. API is stable. `LzwConfig` implements `Default` (returning the TIFF preset), and `LzwError` is `#[non_exhaustive]` ahead of the crate's 1.0 release, so `match` expressions over it need a wildcard arm.

## Quick Start

```rust
use oxiarc_lzw::{compress, decompress, LzwConfig};

// TIFF-style compression (MSB-first)
let config = LzwConfig::TIFF;
let original = b"ABCABCABCABC";
let compressed = compress(original, config)?;
let decompressed = decompress(&compressed, original.len(), config)?;
assert_eq!(decompressed, original);
```

## Zero-allocation TIFF strip decoding (New in 0.4.2)

The code table is the classical prefix/suffix form (`prefix: u16`, `suffix: u8`,
`first: u8`, `length: u16` per code) shared by the encoder and the decoder, and
codes are expanded **backwards directly into the output**. The previous
representation (`Vec<Vec<u8>>` plus a `HashMap<Vec<u8>, u16>`) allocated once
per emitted code on decode and once per input byte on encode; both hot loops
are now allocation-free, and the decoder additionally copies a repeated string
from its earlier occurrence in the output rather than walking the prefix chain.

```rust
use oxiarc_lzw::{compress_tiff, decompress_tiff_into};

let strip = b"row0row0row1row1row2row2";
let compressed = compress_tiff(strip)?;

// The caller already knows the strip size from the image geometry.
let mut out = vec![0u8; strip.len()];
let written = decompress_tiff_into(&compressed, &mut out)?;
assert_eq!(&out[..written], strip);
```

Decoding stops as soon as `dst` is full (leftover input is ignored, matching
libtiff's `LZWDecode`), and a stream that runs out before `dst` is full is an
error rather than a silent truncation. A malicious stream can neither overrun
`dst` nor loop forever.

Measured with criterion (`benches/lzw_into_bench.rs`, which pins a copy of the
pre-0.4.2 per-code-`Vec` decoder as the `legacy_vec` baseline and asserts it
decodes to the same bytes before timing it). Two runs on the same machine under
different load gave absolute times that differed by up to 2x, so the table
reports the **ratio** of the two decoders measured within each run, which was
stable:

| strip content | `legacy_vec` -> `decompress_tiff_into` |
|---|---|
| image-like gradient | 7.7x - 16.5x |
| text | 5.6x - 11.9x |
| incompressible | 7.6x - 14.0x |
| long runs (uniform) | 7.2x - 11.3x |

A third, independent run (20 samples, 1.5 s measurement) put the worst case
at **7.25x** (256 KiB of text) and the best at **13.1x** (1 MiB image-like),
so the roadmap's ">= 3x" target holds with a wide margin on every shape.

Run it yourself with
`cargo bench -p oxiarc-lzw --bench lzw_into_bench -- tiff_strip_decode`.

### Old-style writers (no early code-width change)

Some encoders follow TIFF 6.0's own pseudo-code and grow the code width one
code later than libtiff does. `LzwConfig::TIFF_OLD_STYLE` decodes those.
Fall back to it **only when the standard rule returns an error**, and decode
the retry into a scratch buffer so a failed retry cannot destroy a good
result; cache the winning configuration per image so the retry costs one
strip, not every strip:

```rust
use oxiarc_lzw::{decompress_into, decompress_tiff_into, LzwConfig, Result};

fn decode_strip(strip: &[u8], out: &mut [u8]) -> Result<usize> {
    match decompress_tiff_into(strip, out) {
        Ok(n) => Ok(n),
        Err(standard_err) => {
            let mut scratch = vec![0u8; out.len()];
            match decompress_into(strip, &mut scratch, LzwConfig::TIFF_OLD_STYLE) {
                Ok(n) => {
                    out.copy_from_slice(&scratch);
                    Ok(n)
                }
                // Neither rule explains the strip: report the first failure.
                Err(_) => Err(standard_err),
            }
        }
    }
}
```

A **short** return (`n < out.len()`) is a short strip, not a reason to retry:
that is an ordinary outcome (libtiff warns and keeps the partial row), while
an old-style retry on such a strip fails and overwrites what was already
decoded. Note also that `TIFF_OLD_STYLE` is not libtiff's `LZWDecodeCompat`
variant, which additionally packs its codes LSB-first, and that an old-style
stream can occasionally fill the buffer under the standard rule with wrong
bytes (~2 % of random old-style strips); a caller that *knows* a file is
old-style should select the configuration outright.

## GIF LZW Codec (New in 0.2.4)

The `gif_lzw` module implements the GIF-specific variant of LZW as described in the GIF spec §22:
- LSB-first (Least Significant Bit) bit ordering
- Variable initial code size driven by `minimum_lzw_code_size` from the GIF header
- Clear code and End-of-Information (EOI) code
- Dictionary reset on overflow (max 4096 codes / 12-bit codes)

```rust
use oxiarc_lzw::gif_lzw::{gif_compress, gif_decompress};

// minimum_code_size must be 2..=11 (GIF spec §22)
let data = b"TOBEORNOTTOBEORTOBEORNOT";
let compressed = gif_compress(data, 8)?;
let decompressed = gif_decompress(&compressed, 8)?;
assert_eq!(decompressed.as_slice(), data.as_slice());
```

## LSB Bitstream (New in 0.2.4)

The `bitstream_lsb` module provides low-level LSB-first bit packing used internally by `gif_lzw`:

```rust
use oxiarc_lzw::bitstream_lsb::{LsbBitWriter, LsbBitReader};

let mut writer = LsbBitWriter::new();
writer.write_bits(0b101, 3);
writer.write_bits(0b1100, 4);
let data = writer.into_bytes();

let mut reader = LsbBitReader::new(&data);
assert_eq!(reader.read_bits(3), Some(0b101));
assert_eq!(reader.read_bits(4), Some(0b1100));
```

## Configuration

`LzwConfig` selects clear-code/early-change semantics for the generic
`compress`/`decompress`/`LzwEncoder`/`LzwDecoder` API, which always packs
codes MSB-first. GIF's LSB-first bit ordering and variable minimum code size
are handled separately by the dedicated `gif_compress`/`gif_decompress`
functions (or `LzwStreamMode::Gif` in the streaming API) — see the GIF LZW
Codec section above.

### TIFF Mode

```rust
use oxiarc_lzw::LzwConfig;

let config = LzwConfig::TIFF;
// MSB-first bit ordering
// 9-12 bit codes
// TIFF 6.0 clear codes (strip starts with ClearCode 256; table resets at
// entry 4094) and early code change — libtiff/Pillow/GDAL-compatible
```

### GIF-flavored Mode (still MSB-first)

```rust
use oxiarc_lzw::LzwConfig;

let config = LzwConfig::GIF;
// MSB-first bit ordering (same bitstream as TIFF mode)
// 9-12 bit codes
// Uses a clear code; standard (non-early) code change
```

## API

### High-Level Functions

```rust
use oxiarc_lzw::{compress, decompress, LzwConfig};

let compressed = compress(data, LzwConfig::TIFF)?;
let decompressed = decompress(&compressed, data.len(), LzwConfig::TIFF)?;
```

### Encoder / Decoder (reusable dictionary state)

`LzwEncoder`/`LzwDecoder` own a resettable dictionary, but each `encode`/
`decode` call still processes one complete buffer (see "Streaming
Encoder/Decoder" below for incremental, chunk-at-a-time I/O):

```rust
use oxiarc_lzw::{LzwEncoder, LzwConfig};

let mut encoder = LzwEncoder::new(LzwConfig::TIFF)?;
let compressed = encoder.encode(data)?;
```

```rust
use oxiarc_lzw::{LzwDecoder, LzwConfig};

let mut decoder = LzwDecoder::new(LzwConfig::TIFF)?;
let decompressed = decoder.decode(&compressed, data.len())?;
```

### Streaming Encoder/Decoder (New in 0.2.6)

`LzwStreamEncoder`/`LzwStreamDecoder` implement `std::io::Write`/
`std::io::Read`; select TIFF or GIF framing via `LzwStreamMode`. Only the
**encoder** side is incremental: it flushes an independently-decompressible
frame once its internal buffer reaches the block size, so `write`/`write_all`
calls do not accumulate the whole input before producing output. The
**decoder** is not: `LzwStreamDecoder::read` eagerly reads the entire inner
reader to EOF and decompresses every frame on the first call, serving the
result from an internal buffer on subsequent calls — memory-efficient
framing on the wire, but not incremental decoding. This decoder also speaks
only the crate's own 8-byte-per-frame length-prefixed framing (what
`LzwStreamEncoder` writes), **not** a bare TIFF/GIF LZW bitstream; to decode
or encode a real TIFF strip directly, use the crate's `decompress_tiff_into`/
`compress_tiff` functions instead (see their rustdoc for the exact strip-level
contract):

```rust
use std::io::{Read, Write};
use oxiarc_lzw::{LzwStreamEncoder, LzwStreamDecoder, LzwStreamMode};

// Streaming encoder - TIFF framing
let mut encoder = LzwStreamEncoder::new(Vec::new(), LzwStreamMode::Tiff);
encoder.write_all(data)?;
let compressed = encoder.finish()?;

// Streaming decoder
let mut decoder = LzwStreamDecoder::new(&compressed[..], LzwStreamMode::Tiff);
let mut decompressed = Vec::new();
decoder.read_to_end(&mut decompressed)?;
assert_eq!(decompressed, data);
```

## Algorithm

LZW builds a dictionary dynamically:
1. **Start with single-byte codes** (0-255)
2. **Add new patterns** to dictionary on-the-fly
3. **Variable-width codes** - Grows from 9 to 12 bits
4. **Table reset** - Clear dictionary when full (4096 entries)

### Code Structure

| Code Range | Meaning |
|------------|---------|
| 0-255 | Literal bytes |
| 256 | Clear code (reset dictionary) |
| 257-4095 | Dictionary entries |

## Features (Cargo)

| Feature | Default | Description |
|---------|---------|-------------|
| `tiff-oracle` | off | Enables differential oracle tests (`tests/tiff_lzw_oracle.rs`) that validate TIFF-LZW interop against Pillow/libtiff in both directions; tests self-skip when `python3`+Pillow are absent. Test-only — the library compiles identically either way. |

```toml
[dependencies]
oxiarc-lzw = "0.4.2"
```

## Use Cases

- **TIFF images** - LZW is one of the standard TIFF compression methods
- **GIF animations** - Original GIF compression format (including full GIF LZW codec)
- **Legacy data** - Unix `.Z` files (compress/uncompress)

## Part of OxiArc

This crate is part of the [OxiArc](https://github.com/cool-japan/oxiarc) project - a Pure Rust archive/compression library ecosystem.

## License

Apache-2.0
