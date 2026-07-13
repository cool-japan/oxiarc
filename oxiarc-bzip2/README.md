
# oxiarc-bzip2 [Stable]

Pure Rust implementation of BZip2 compression/decompression algorithm.

![Version](https://img.shields.io/badge/version-0.4.0-blue)
![License](https://img.shields.io/badge/license-Apache--2.0-green)
![Status](https://img.shields.io/badge/status-Stable-brightgreen)

**Version 0.4.0** (2026-07-13) — 108 tests passing.

## Overview

BZip2 is a high-compression algorithm based on the Burrows-Wheeler Transform (BWT) and Huffman coding, offering better compression ratios than DEFLATE at the cost of speed.


## Features

- **Pure Rust** - No C dependencies or unsafe FFI
- **Reference interop** - Multi-table Huffman encoding (libbz2's `sendMTFValues` clustering: 2-6 tables, real per-group selectors); output verified byte-for-byte against `bzip2 -d`
- **Multi-stream decode** - Concatenated `.bz2` files (pbzip2, lbzip2, `cat a.bz2 b.bz2`) decode in full, like `bzip2 -d`; trailing garbage is an error, never silent loss
- **Legacy randomised blocks** - Streams from bzip2 <= 0.9.0 with the randomised bit set are de-randomised (libbz2 `BZ2_rNums` schedule)
- **Bomb guard** - `decompress_with_limit` caps output size for untrusted input
- **Parallel compression** - Multi-threaded block compression with Rayon
- **Compression levels 1-9** - Adjustable block sizes (100KB-900KB)
- **Streaming API** - Process data in chunks; the encoder buffers small writes into full-size blocks
- **One-shot API** - Convenient functions for simple cases
- **Property-tested** - `proptest`-based round-trip and no-panic fuzzing across arbitrary inputs and every compression level
- **Oracle-tested** - Differential suite against the system `bzip2` CLI in both directions (feature `bzip2-oracle`; self-skips when the binary is absent)

All features are implemented and tested. API is stable.

## Quick Start

```rust
use oxiarc_bzip2::{compress, decompress, CompressionLevel};

// Compress data
let original = b"Hello, World! ".repeat(100);
let compressed = compress(&original, CompressionLevel::new(9))?;

// Decompress data (`decompress` takes any `Read`, so slice the `Vec<u8>`)
let decompressed = decompress(&compressed[..])?;
assert_eq!(decompressed, original);
```

## Compression Levels

| Level | Block Size | Use Case |
|-------|------------|----------|
| 1 | 100KB | Fast compression |
| 5 | 500KB | Balanced (default) |
| 9 | 900KB | Best compression |

## Parallel Compression

```rust
use oxiarc_bzip2::compress_parallel;

// Use all available CPU cores
let compressed = compress_parallel(&data, CompressionLevel::new(9))?;
```

## Algorithm

BZip2 uses a multi-stage pipeline:
1. **Burrows-Wheeler Transform** - Reversible permutation for better compressibility
2. **Move-to-Front** - Converts repeated characters to small integers
3. **Run-Length Encoding** - Compresses runs of zeros
4. **Huffman Coding** - Final entropy coding stage

## Features

| Feature | Default | Description |
|---------|---------|-------------|
| `default` | yes | Core BZip2 compression/decompression |
| `parallel` | no | Multi-threaded block compression via Rayon |
| `bzip2-oracle` | no | Differential tests against the system `bzip2` CLI (tests self-skip if absent) |

## Examples

```sh
cargo run -p oxiarc-bzip2 --example roundtrip
```

Round-trips data through every compression level (1-9) via `compress`/`decompress`.

## Part of OxiArc

This crate is part of the [OxiArc](https://github.com/cool-japan/oxiarc) project - a Pure Rust archive/compression library ecosystem.

Add to your `Cargo.toml`:

```toml
[dependencies]
oxiarc-bzip2 = "0.4.0"
```

With parallel compression enabled:

```toml
[dependencies]
oxiarc-bzip2 = { version = "0.4.0", features = ["parallel"] }
```

## License

Apache-2.0
