
# oxiarc-deflate [Stable]

Pure Rust implementation of the DEFLATE compression algorithm (RFC 1951).

![Version](https://img.shields.io/badge/version-0.3.0-blue)
![License](https://img.shields.io/badge/license-Apache--2.0-green)
![Status](https://img.shields.io/badge/status-Stable-brightgreen)

**Version 0.3.0** (2026-05-17) — 205 tests passing.

**What's new in 0.3.x (latest)**:
- **Parallel GZIP compression** (`parallel` feature): pigz-style multi-member GZIP via `gzip_compress_parallel()` and `ParallelGzipEncoder` builder.
- **LZ77 match heuristics tuning**: `Lz77Params` / `Lz77Preset` structs and `Deflater::with_lz77_params()` / `Deflater::with_lz77_preset()` builder methods for fine-grained speed/ratio trade-offs.
- **DeflatePool memory pool**: `DeflatePool` for thread-safe window/hash buffer reuse with `Deflater::with_pool()` and `PoolStats` tracking.
- **OptimalParser**: Zopfli-style graph-based optimal DEFLATE parsing strategy. Enable via `Deflater::with_optimal_parsing(level)`.

**What's new in 0.2.8**: Added async streaming support for raw DEFLATE with `RawDeflateWriter` and `RawInflateReader` (requires `async-io` feature).

**What's new in 0.2.6**: Added streaming support with `GzipStreamEncoder`/`GzipStreamDecoder` and `ZlibStreamEncoder`/`ZlibStreamDecoder` with flush modes for fine-grained control over compressed output.

## Overview

DEFLATE is the compression algorithm used in:
- ZIP archives
- GZIP compressed files
- PNG images
- HTTP compression
- Many other formats

This crate provides both compression and decompression with no external dependencies.


## Features

- **Pure Rust** - No C bindings or unsafe code
- **Full RFC 1951 compliance** - All block types supported
- **Compression levels 0-9** - From stored to maximum compression
- **Streaming API** - Process data in chunks
- **One-shot API** - Convenient functions for simple cases
- **Async I/O** - `async_deflate` module with Tokio-based async streaming (enable `async-io` feature)
- **GZIP support** - `gzip` module for RFC 1952 GZIP format encoding/decoding
- **Parallel GZIP compression** - pigz-style multi-member GZIP using multiple threads (enable `parallel` feature)
- **LZ77 heuristics tuning** - `Lz77Params` / `Lz77Preset` for speed/ratio trade-off control
- **Memory pool** - `DeflatePool` for reusing window/hash buffers across compression calls

All features are implemented and tested. API is stable.

### Cargo Features

| Feature | Default | Description |
|---------|---------|-------------|
| `default` | yes | DEFLATE compression/decompression, LZ77, Huffman, OptimalParser, LZ77 heuristics, DeflatePool |
| `async-io` | no | Async streaming I/O via Tokio (enables `async_deflate` module) |
| `parallel` | no | Multi-threaded GZIP compression via `gzip_compress_parallel` and `ParallelGzipEncoder` |

## Usage

Add to your `Cargo.toml`:

```toml
[dependencies]
oxiarc-deflate = "0.3.0"
```

With async I/O support:

```toml
[dependencies]
oxiarc-deflate = { version = "0.3.0", features = ["async-io"] }
```

With parallel GZIP compression:

```toml
[dependencies]
oxiarc-deflate = { version = "0.3.0", features = ["parallel"] }
```

## Quick Start

```rust
use oxiarc_deflate::{deflate, inflate};

// Compress data
let original = b"Hello, World! Hello, World! Hello, World!";
let compressed = deflate(original, 6)?;  // Level 6 (default)

// Decompress data
let decompressed = inflate(&compressed)?;
assert_eq!(&decompressed, original);
```

## Compression Levels

| Level | Description | Use Case |
|-------|-------------|----------|
| 0 | Stored (no compression) | Already compressed data |
| 1-3 | Fast compression | Real-time streaming |
| 4-6 | Balanced (default: 6) | General purpose |
| 7-9 | Best compression | Archival, storage |

## API

### High-Level Functions

```rust
// Compress with specified level
let compressed = deflate(data, level)?;

// Decompress
let decompressed = inflate(&compressed)?;
```

### Streaming API

```rust
use oxiarc_deflate::{Deflater, Inflater};

// Streaming compression
let mut deflater = Deflater::new(6);
let compressed = deflater.compress_all(data)?;

// Streaming decompression
let mut inflater = Inflater::new();
loop {
    let (consumed, produced, status) = inflater.decompress(input, output)?;
    // Handle status...
}
```

### LZ77 Encoder

```rust
use oxiarc_deflate::{Lz77Encoder, Lz77Token};

let mut encoder = Lz77Encoder::new(6);
for token in encoder.encode(data) {
    match token {
        Lz77Token::Literal(byte) => { /* literal byte */ }
        Lz77Token::Match { length, distance } => { /* back-reference */ }
    }
}
```

### Huffman Trees

```rust
use oxiarc_deflate::{HuffmanTree, HuffmanBuilder};

// Build tree from code lengths
let lengths = [3, 3, 3, 3, 3, 2, 4, 4];
let tree = HuffmanTree::from_lengths(&lengths)?;

// Decode symbols
let symbol = tree.decode(&mut bit_reader)?;
```

## Algorithm Details

### DEFLATE Structure

```
+------------------+
| Block Header     | (3 bits: BFINAL + BTYPE)
+------------------+
| Block Data       | (varies by type)
+------------------+
| ... more blocks  |
+------------------+
```

### Block Types

| BTYPE | Name | Description |
|-------|------|-------------|
| 00 | Stored | Uncompressed data (up to 65535 bytes) |
| 01 | Fixed | Fixed Huffman codes (RFC 1951 Table) |
| 10 | Dynamic | Custom Huffman codes in header |
| 11 | Reserved | Invalid |

### LZ77 Parameters

- **Window size**: 32768 bytes (32KB)
- **Match length**: 3-258 bytes
- **Match distance**: 1-32768 bytes
- **Minimum match**: 3 bytes

### Huffman Alphabets

**Literal/Length (286 symbols)**:
- 0-255: Literal bytes
- 256: End of block
- 257-285: Length codes (3-258)

**Distance (30 symbols)**:
- 0-29: Distance codes (1-32768)

### Fixed Huffman Code Lengths

| Range | Code Length |
|-------|-------------|
| 0-143 | 8 bits |
| 144-255 | 9 bits |
| 256-279 | 7 bits |
| 280-287 | 8 bits |

## Modules

| Module | Description |
|--------|-------------|
| `deflate` | Compression (encoder) |
| `inflate` | Decompression (decoder) |
| `huffman` | Huffman tree operations |
| `lz77` | LZ77 dictionary encoder; `Lz77Params`, `Lz77Preset` |
| `tables` | Fixed Huffman tables, length/distance extra bits |
| `gzip` | GZIP format (RFC 1952) encoding, decoding, and parallel compression |
| `pool` | `DeflatePool` and `PoolStats` for window/hash buffer reuse |
| `async_deflate` | Async streaming compression/decompression (requires `async-io` feature) |

### GZIP API

```rust
use oxiarc_deflate::gzip::{gzip_encode, gzip_decode};

// Encode to GZIP format
let compressed = gzip_encode(data, 6)?;

// Decode GZIP data
let decompressed = gzip_decode(&compressed)?;
```

### Async DEFLATE (requires `async-io` feature)

```rust
use oxiarc_deflate::async_deflate::{AsyncDeflater, AsyncInflater};
use tokio::io::BufReader;

// Async compression
let mut deflater = AsyncDeflater::new(6);
let compressed = deflater.compress_all(reader).await?;

// Async decompression
let mut inflater = AsyncInflater::new();
let decompressed = inflater.decompress_all(reader).await?;
```

### Parallel GZIP (requires `parallel` feature)

```rust
use oxiarc_deflate::gzip::{gzip_compress_parallel, ParallelGzipEncoder};

// One-shot parallel GZIP (pigz-style multi-member output)
let compressed = gzip_compress_parallel(data, 6, 1 << 17)?; // level 6, 128 KB chunks

// Builder API
let compressed = ParallelGzipEncoder::new()
    .level(6)
    .chunk_size(1 << 17)   // 128 KB per chunk
    .num_threads(4)
    .encode(data)?;
```

### LZ77 Heuristics Tuning

```rust
use oxiarc_deflate::{Deflater, Lz77Params, Lz77Preset};

// Use a preset
let compressed = Deflater::new(9)
    .with_lz77_preset(Lz77Preset::Ultra)
    .compress_all(data)?;

// Fine-grained control
let params = Lz77Params {
    nice_length: 128,
    max_chain: 256,
    good_length: 32,
};
let compressed = Deflater::new(9)
    .with_lz77_params(params)
    .compress_all(data)?;
```

Available presets:

| Preset | Description |
|--------|-------------|
| `Fast` | Minimal chain searching, fastest throughput |
| `Default` | Balanced speed and ratio (equivalent to level 6) |
| `Best` | Longer chain searching, best standard ratio |
| `Ultra` | Maximum chain searching, slowest but smallest output |

### DeflatePool (memory pool)

```rust
use oxiarc_deflate::{Deflater, DeflatePool};

// Create a shared pool (e.g., once per application / thread pool)
let pool = DeflatePool::new();

// Reuse window/hash buffers across calls — reduces allocations
let compressed = Deflater::new(6)
    .with_pool(&pool)
    .compress_all(data)?;

// Inspect pool statistics
let stats = pool.stats();
println!("hits={} allocs={}", stats.hits, stats.allocations);
```

## Performance

Compression ratios on typical data (Calgary Corpus):

| File | Original | Compressed | Ratio |
|------|----------|------------|-------|
| book1 | 768771 | ~300000 | ~61% |
| paper1 | 53161 | ~18000 | ~66% |
| progc | 39611 | ~13000 | ~67% |

## References

- [RFC 1951 - DEFLATE Compressed Data Format Specification](https://www.rfc-editor.org/rfc/rfc1951)
- [An Explanation of the Deflate Algorithm](https://zlib.net/feldspar.html)

## License

Apache-2.0
