# oxiarc-lz4

Pure Rust implementation of LZ4 compression algorithm with LZ4-HC (High Compression).

[![Crates.io](https://img.shields.io/crates/v/oxiarc-lz4.svg)](https://crates.io/crates/oxiarc-lz4)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)

**Version: 0.2.3 (2026-03-11) | Tests: 99 passing**

## Overview

LZ4 is a lossless compression algorithm focused on compression and decompression speed, making it ideal for real-time applications. It provides an excellent balance between speed and compression ratio. Version 0.2.3 includes improvements to the dictionary (`dict`) and high-compression (`hc`) modules.

## Features

- **Pure Rust** - No C dependencies or unsafe FFI
- **Extremely fast** - Optimized for speed-critical applications
- **LZ4-HC** - High Compression mode for better ratios at the cost of speed
- **Frame format support** - Compatible with LZ4 frame format
- **Block format support** - Low-level block API available
- **Content size tracking** - Original size metadata in frames
- **Parallel compression** - Optional multi-threaded compression via Rayon (`parallel` feature)
- **Dictionary support** - Improved dictionary compression in 0.2.3

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

### Parallel Compression

```rust
use oxiarc_lz4::compress_parallel;

// Use all available CPU cores (requires `parallel` feature)
let compressed = compress_parallel(&data)?;
```

## Features (Cargo)

| Feature | Default | Description |
|---------|---------|-------------|
| `parallel` | no | Multi-threaded block compression via Rayon |

```toml
[dependencies]
# Default (no parallel)
oxiarc-lz4 = "0.2.3"

# With parallel compression
oxiarc-lz4 = { version = "0.2.3", features = ["parallel"] }
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
