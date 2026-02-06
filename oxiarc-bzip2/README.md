# oxiarc-bzip2

Pure Rust implementation of BZip2 compression/decompression algorithm.

## Overview

BZip2 is a high-compression algorithm based on the Burrows-Wheeler Transform (BWT) and Huffman coding, offering better compression ratios than DEFLATE at the cost of speed.

## Features

- **Pure Rust** - No C dependencies or unsafe FFI
- **Parallel compression** - Multi-threaded block compression with Rayon
- **Compression levels 1-9** - Adjustable block sizes (100KB-900KB)
- **Streaming API** - Process data in chunks
- **One-shot API** - Convenient functions for simple cases

## Quick Start

```rust
use oxiarc_bzip2::{compress, decompress, CompressionLevel};

// Compress data
let original = b"Hello, World! ".repeat(100);
let compressed = compress(&original, CompressionLevel::new(9))?;

// Decompress data
let decompressed = decompress(&compressed)?;
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

## Part of OxiArc

This crate is part of the [OxiArc](https://github.com/cool-japan/oxiarc) project - a Pure Rust archive/compression library ecosystem.

## License

MIT OR Apache-2.0
