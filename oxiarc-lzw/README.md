# oxiarc-lzw

Pure Rust implementation of LZW (Lempel-Ziv-Welch) compression for TIFF and GIF formats.

[![Crates.io](https://img.shields.io/crates/v/oxiarc-lzw.svg)](https://crates.io/crates/oxiarc-lzw)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)

**Version: 0.2.5 (2026-03-18) | Tests: 76 passing**

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

## Quick Start

```rust
use oxiarc_lzw::{encode, decode, Config};

// TIFF-style compression (MSB-first)
let config = Config::tiff();
let original = b"ABCABCABCABC";
let compressed = encode(original, &config);
let decompressed = decode(&compressed, &config)?;
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

### TIFF Mode

```rust
use oxiarc_lzw::Config;

let config = Config::tiff();
// MSB-first bit ordering
// 9-bit initial codes
// Early change enabled
```

### GIF Mode

```rust
let config = Config::gif(8); // 8-bit minimum code size
// LSB-first bit ordering
// Variable code width (9-12 bits)
```

## API

### High-Level Functions

```rust
use oxiarc_lzw::{encode, decode, Config};

let compressed = encode(data, &Config::tiff());
let decompressed = decode(&compressed, &Config::tiff())?;
```

### Streaming Encoder

```rust
use oxiarc_lzw::Encoder;

let mut encoder = Encoder::new(Config::tiff());
encoder.encode_bytes(data, &mut output);
encoder.finish(&mut output);
```

### Streaming Decoder

```rust
use oxiarc_lzw::Decoder;

let mut decoder = Decoder::new(Config::tiff());
decoder.decode_bytes(&compressed, &mut output)?;
```

### Streaming Encoder/Decoder (New in 0.2.5)

Streaming interfaces for processing data incrementally without buffering entire inputs:

```rust
use oxiarc_lzw::{StreamingEncoder, StreamingDecoder, Config};

// Streaming encoder - feed data in chunks
let mut encoder = StreamingEncoder::new(Config::tiff());
encoder.write_chunk(chunk1, &mut output)?;
encoder.write_chunk(chunk2, &mut output)?;
encoder.finish(&mut output)?;

// Streaming decoder - decode data in chunks
let mut decoder = StreamingDecoder::new(Config::tiff());
decoder.decode_chunk(chunk1, &mut output)?;
decoder.decode_chunk(chunk2, &mut output)?;
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
| `std` | yes | Standard library support |

```toml
[dependencies]
oxiarc-lzw = "0.2.5"
```

## Use Cases

- **TIFF images** - LZW is one of the standard TIFF compression methods
- **GIF animations** - Original GIF compression format (including full GIF LZW codec)
- **Legacy data** - Unix `.Z` files (compress/uncompress)

## Part of OxiArc

This crate is part of the [OxiArc](https://github.com/cool-japan/oxiarc) project - a Pure Rust archive/compression library ecosystem.

## License

Apache-2.0
