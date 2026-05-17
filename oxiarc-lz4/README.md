
# oxiarc-lz4 [Stable]

Pure Rust implementation of LZ4 compression algorithm with LZ4-HC (High Compression).

[![Crates.io](https://img.shields.io/crates/v/oxiarc-lz4.svg)](https://crates.io/crates/oxiarc-lz4)
![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)
![Status](https://img.shields.io/badge/status-Stable-brightgreen)

**Version: 0.3.0 (2026-05-17) | 138 tests passing**

## Overview

LZ4 is a lossless compression algorithm focused on compression and decompression speed, making it ideal for real-time applications. It provides an excellent balance between speed and compression ratio. Version 0.3.0 introduces true bounded-memory streaming (no full-input buffering in `Lz4Compressor`), a state-machine block parser in `Lz4Decompressor`, and a `with_memory_budget(usize)` builder on both encoder and decoder, along with continued improvements to the dictionary (`dict`) and high-compression (`hc`) modules.


## Features

- **Pure Rust** - No C dependencies or unsafe FFI
- **Extremely fast** - Optimized for speed-critical applications
- **LZ4-HC** - High Compression mode for better ratios at the cost of speed
- **Frame format support** - Compatible with LZ4 frame format
- **Block format support** - Low-level block API available
- **Content size tracking** - Original size metadata in frames
- **Parallel compression** - Optional multi-threaded compression via Rayon (`parallel` feature)
- **Acceleration parameter** - Tunable speed/ratio tradeoff for compression
- **Dictionary support** - Improved dictionary compression
- **Progress reporting** - `with_progress(Arc<dyn ProgressSink>)` builder on compressor/decompressor types
- **Cancellation support** - `with_cancel(CancellationToken)` builder on compressor/decompressor types
- **True bounded-memory streaming** - `Lz4Compressor` emits complete blocks on the fly with no full-input buffering
- **State-machine block parser** - `Lz4Decompressor` processes one block at a time via an internal state machine
- **Memory budget builder** - `with_memory_budget(usize)` on both encoder and decoder to cap working-set size
- **Block-layer prefix dictionary** - `compress_block_with_dict` / `decompress_block_dict` free functions and `Lz4DictBlockEncoder` / `Lz4DictBlockDecoder` builders for prefix-dictionary block compression (dictionary truncated to last 64 KiB per LZ4 spec)

All features are implemented and tested. API is stable.

## Quick Start

```rust
use oxiarc_lz4::{compress, decompress};

// Compress data
let original = b"Hello, LZ4! ".repeat(100);
let compressed = compress(&original)?;

// Decompress data
let decompressed = decompress(&compressed)?;
assert_eq!(decompressed, original);
```

## API

### Frame Format (High-Level)

```rust
use oxiarc_lz4::{Lz4Writer, Lz4Reader};
use std::io::Cursor;

// Compression
let mut output = Vec::new();
let mut writer = Lz4Writer::new(&mut output);
writer.write_compressed(&data)?;

// Decompression
let mut reader = Lz4Reader::new(Cursor::new(compressed))?;
let decompressed = reader.decompress()?;
```

### Block Format (Low-Level)

```rust
use oxiarc_lz4::block::{compress_block, decompress_block};

let compressed = compress_block(&data);
let decompressed = decompress_block(&compressed, original_size)?;
```

### Block-Layer Prefix Dictionary

Both encoder and decoder must use the same dictionary. The dictionary is automatically truncated to the last 64 KiB (LZ4 spec limit).

```rust
use oxiarc_lz4::block::{
    compress_block_with_dict, decompress_block_dict,
    Lz4DictBlockEncoder, Lz4DictBlockDecoder,
};

let dict = b"common prefix data used to seed the dictionary";
let input = b"common prefix data plus some new payload bytes";

// Free-function API
let compressed = compress_block_with_dict(input, dict, 1 /* accel */);
let decompressed = decompress_block_dict(&compressed, dict, input.len())?;
assert_eq!(&decompressed, input);

// Builder API
let compressed = Lz4DictBlockEncoder::new(dict)
    .acceleration(1)
    .compress(input);

let decompressed = Lz4DictBlockDecoder::new(dict)
    .decompress(&compressed, input.len())?;
assert_eq!(&decompressed, input);
```

### Parallel Compression

```rust
use oxiarc_lz4::compress_parallel;

// Use all available CPU cores (requires `parallel` feature)
let compressed = compress_parallel(&data)?;
```

## Progress and Cancellation (0.3.0)

`Lz4Compressor`, `Lz4Decompressor`, `Lz4DictFrameEncoder`, and `Lz4DictFrameDecoder` all expose two new builder methods:

```rust
use oxiarc_lz4::{Lz4Compressor, ProgressSink, CancellationToken};
use std::sync::Arc;

// Attach a progress sink (receives bytes-processed callbacks)
let compressor = Lz4Compressor::new()
    .with_progress(Arc::new(my_progress_sink));

// Attach a cancellation token (cooperative cancellation)
let compressor = Lz4Compressor::new()
    .with_cancel(cancellation_token);

// Both can be combined
let compressor = Lz4Compressor::new()
    .with_progress(Arc::new(my_progress_sink))
    .with_cancel(cancellation_token);
```

The same pattern applies to `Lz4Decompressor`, `Lz4DictFrameEncoder`, and `Lz4DictFrameDecoder`.

## Features (Cargo)

| Feature | Default | Description |
|---------|---------|-------------|
| `parallel` | no | Multi-threaded block compression via Rayon |

```toml
[dependencies]
# Default (no parallel)
oxiarc-lz4 = "0.3.0"

# With parallel compression
oxiarc-lz4 = { version = "0.3.0", features = ["parallel"] }
```

## Use Cases

- **Real-time data streaming** - Network protocols, game assets
- **Log compression** - Fast compression for high-volume logs
- **Cache compression** - Reduce memory usage with minimal overhead
- **Backup systems** - Quick compression for frequent backups

## Algorithm

LZ4 uses simple but effective techniques:
1. **LZ77-style matching** - Find repeated sequences
2. **Token encoding** - Compact representation of literals and matches
3. **Fast hash table** - Quick pattern matching
4. **No entropy coding** - Raw tokens for maximum speed

## Part of OxiArc

This crate is part of the [OxiArc](https://github.com/cool-japan/oxiarc) project - a Pure Rust archive/compression library ecosystem.

## License

Apache-2.0
