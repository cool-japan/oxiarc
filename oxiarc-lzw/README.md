
# oxiarc-lzw [Stable]

Pure Rust implementation of LZW (Lempel-Ziv-Welch) compression for TIFF and GIF formats.

[![Crates.io](https://img.shields.io/crates/v/oxiarc-lzw.svg)](https://crates.io/crates/oxiarc-lzw)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)
![Status](https://img.shields.io/badge/status-Stable-brightgreen)

**Version: 0.4.2 (2026-08-06) | 100 tests passing (incl. libtiff/Pillow differential oracle)**

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
- **Reference interop** - TIFF-LZW streams are byte-compatible with libtiff/Pillow in both directions (differential-tested; see `tests/tiff_lzw_oracle.rs` and the pinned fixtures in `tests/data/`)
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
`std::io::Read` for incremental processing; select TIFF or GIF framing via
`LzwStreamMode` (the encoder flushes an independently-decompressible frame
once its internal buffer reaches the block size):

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
