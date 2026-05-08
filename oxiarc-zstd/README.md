
# oxiarc-zstd [Stable]

Pure Rust implementation of Zstandard (zstd) compression algorithm.

[![Crates.io](https://img.shields.io/crates/v/oxiarc-zstd.svg)](https://crates.io/crates/oxiarc-zstd)
![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)
![Status](https://img.shields.io/badge/status-Stable-brightgreen)

**Version: 0.2.8 (2026-05-08) | 176 tests passing**

## Overview

Zstandard is a modern compression algorithm developed by Facebook (Meta), offering excellent compression ratios with fast decompression speeds. It's designed to replace older algorithms like DEFLATE and BZip2 in many applications. Version 0.2.6 includes improvements to the frame, streaming, and core library modules.


## Features

- **Pure Rust** - No C dependencies or unsafe FFI
- **Excellent compression ratios** - Better than DEFLATE, competitive with BZip2
- **Fast decompression** - Faster than BZip2, competitive with DEFLATE
- **Parallel compression** - Multi-threaded block compression with Rayon (`parallel` feature)
- **Dictionary support** - Pre-trained dictionaries for better compression
- **Checksum support** - XXH64 checksums for data integrity
- **Streaming API** - Incremental encoder/decoder for large data
- **Progress reporting** - `with_progress(Arc<dyn ProgressSink>)` builder on encoders and stream decoder
- **Cancellation** - `with_cancel(CancellationToken)` builder for cooperative cancellation

All features are implemented and tested. API is stable.

## Quick Start

```rust
use oxiarc_zstd::{compress, decompress};

// Compress data
let original = b"Hello, Zstandard! ".repeat(100);
let compressed = compress(&original, 3)?; // Level 3 (default)

// Decompress data
let decompressed = decompress(&compressed)?;
assert_eq!(decompressed, original);
```

## Compression Levels

| Level | Speed | Ratio | Use Case |
|-------|-------|-------|----------|
| 1-3 | Fast | Good | Real-time compression |
| 4-9 | Medium | Better | General purpose (default: 3) |
| 10-19 | Slow | Best | Archival, storage |
| 20-22 | Very slow | Maximum | Ultra compression |

## Parallel Compression

```rust
use oxiarc_zstd::compress_parallel;

// Use all available CPU cores (requires `parallel` feature)
let compressed = compress_parallel(&data, 3)?;
```

## API

### One-Shot Functions

```rust
use oxiarc_zstd::{compress, decompress};

let compressed = compress(data, level)?;
let decompressed = decompress(&compressed)?;
```

### Streaming Compression

```rust
use oxiarc_zstd::Encoder;

let mut encoder = Encoder::new(3); // Level 3
encoder.set_checksum(true);
let compressed = encoder.compress(data)?;
```

### Streaming Decompression

```rust
use oxiarc_zstd::Decoder;

let mut decoder = Decoder::new();
let decompressed = decoder.decompress(&compressed)?;
```

## Progress Reporting and Cancellation

`ZstdEncoder`, `ZstdStreamEncoder`, and `ZstdStreamDecoder` all expose builder methods for observability and cooperative cancellation:

```rust
use std::sync::Arc;
use oxiarc_zstd::{ZstdEncoder, ProgressSink, CancellationToken};

// Progress reporting
let sink: Arc<dyn ProgressSink> = Arc::new(MyProgressHandler);
let encoder = ZstdEncoder::new(3).with_progress(sink);

// Cooperative cancellation
let token = CancellationToken::new();
let encoder = ZstdEncoder::new(3).with_cancel(token.clone());

// Cancel from another thread
token.cancel();
```

The `with_progress` and `with_cancel` builders can be chained together.

## Features (Cargo)

| Feature | Default | Description |
|---------|---------|-------------|
| `parallel` | no | Multi-threaded block compression via Rayon |

```toml
[dependencies]
# Default (no parallel)
oxiarc-zstd = "0.2.8"

# With parallel compression
oxiarc-zstd = { version = "0.2.8", features = ["parallel"] }
```

## Algorithm

Zstandard uses a sophisticated multi-stage approach:
1. **LZ77 matching** - Find repeated sequences
2. **Finite State Entropy (FSE)** - Advanced entropy coding
3. **Huffman coding** - For literals
4. **Sequence encoding** - Efficient match/literal/offset representation
5. **Block structure** - Independent blocks for parallelization

### Frame Format

```
+------------------+
| Magic Number     | 4 bytes: 0x28 0xB5 0x2F 0xFD
+------------------+
| Frame Header     | Window size, dictionary ID, etc.
+------------------+
| Data Blocks      | Compressed or raw blocks
+------------------+
| Checksum (opt)   | XXH64 checksum
+------------------+
```

## Performance

Typical compression comparison:

| Algorithm | Ratio | Compress Speed | Decompress Speed |
|-----------|-------|----------------|------------------|
| LZ4 | 2.1x | Very Fast | Very Fast |
| Zstandard | 2.8x | Fast | Fast |
| DEFLATE | 2.7x | Medium | Medium |
| BZip2 | 3.3x | Slow | Slow |

## Use Cases

- **Web assets** - Better compression than gzip
- **Database storage** - Fast decompression for queries
- **Network protocols** - HTTP/2, HTTP/3
- **File systems** - Transparent compression (Btrfs, ZFS)
- **Container images** - Docker, OCI images

## Part of OxiArc

This crate is part of the [OxiArc](https://github.com/cool-japan/oxiarc) project - a Pure Rust archive/compression library ecosystem.

## References

- [Zstandard RFC 8878](https://datatracker.ietf.org/doc/html/rfc8878)
- [Zstandard Homepage](https://facebook.github.io/zstd/)

## License

Apache-2.0
