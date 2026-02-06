# oxiarc-lzw

Pure Rust implementation of LZW (Lempel-Ziv-Welch) compression for TIFF and GIF formats.

## Overview

LZW is a dictionary-based compression algorithm used in TIFF images, GIF animations, and legacy Unix compress. This implementation provides both TIFF-style (MSB-first) and GIF-style (LSB-first) bit packing.

## Features

- **Pure Rust** - No C dependencies or unsafe FFI
- **TIFF support** - MSB-first bit ordering for TIFF images
- **GIF support** - LSB-first bit ordering for GIF animations
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

## Use Cases

- **TIFF images** - LZW is one of the standard TIFF compression methods
- **GIF animations** - Original GIF compression format
- **Legacy data** - Unix `.Z` files (compress/uncompress)

## Part of OxiArc

This crate is part of the [OxiArc](https://github.com/cool-japan/oxiarc) project - a Pure Rust archive/compression library ecosystem.

## License

MIT OR Apache-2.0
