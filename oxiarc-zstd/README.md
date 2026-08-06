
# oxiarc-zstd [Stable]

Pure Rust implementation of Zstandard (zstd) compression algorithm.

[![Crates.io](https://img.shields.io/crates/v/oxiarc-zstd.svg)](https://crates.io/crates/oxiarc-zstd)
![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)
![Status](https://img.shields.io/badge/status-Stable-brightgreen)

**Version: 0.4.2 (2026-08-06) | 208 tests passing**

## Overview

Zstandard is a modern compression algorithm developed by Facebook (Meta), offering excellent compression ratios with fast decompression speeds. It's designed to replace older algorithms like DEFLATE and BZip2 in many applications. Version 0.3.6 hardens the frame decoder against malformed/hostile headers (bounded, `try_reserve`-based output allocation instead of trusting the untrusted `Frame_Content_Size` field outright) and — most importantly — makes the FSE/Huffman entropy layer bit-exact with RFC 8878: the backward bitstream is now read/written with the reference `BIT_*` semantics, so real `zstd`-produced frames decode byte-identically and every oxiarc-produced frame is accepted by the reference `zstd` CLI (verified continuously by the `zstd-oracle` differential test suite).


## Features

- **Pure Rust** - No C dependencies or unsafe FFI
- **Reference interoperability, both directions** - frames produced by the reference `zstd` CLI (levels 1-19, `--ultra -22`, `--long`, `--no-check`, `--no-content-size`, raw-content dictionaries, multi-frame streams) decode byte-identically, and every frame this encoder emits is accepted and correctly decoded by `zstd -d`; enforced by embedded reference-frame fixtures (always on) plus a live CLI differential suite (`zstd-oracle` feature)
- **Full RFC 8878 entropy decoding** - FSE-compressed sequence tables, 1- and 4-stream Huffman literals, repeat offsets, treeless literals, repeat table modes
- **Huffman literals on the encode path** - literal sections are Huffman-compressed when that wins (self-verified with Raw/RLE fallback); sequences use the RFC 8878 predefined/RLE FSE tables, so the ratio on some inputs trails the reference encoder (custom block-optimal sequence tables are not emitted yet)
- **Parallel compression** - Multi-threaded block compression with Rayon (`parallel` feature)
- **Dictionary support** - Raw-content dictionaries, interoperable with `zstd -D` in both directions
- **Checksum support** - XXH64 checksums for data integrity
- **Streaming API** - Incremental encoder/decoder for large data
- **Progress reporting** - `with_progress(Arc<dyn ProgressSink>)` builder on encoders and stream decoder
- **Cancellation** - `with_cancel(CancellationToken)` builder for cooperative cancellation
- **Hardened frame decoding** - untrusted header fields (e.g. `Frame_Content_Size`) are bounds-checked and reserved with `Vec::try_reserve` rather than trusted outright; every FSE/Huffman table index is validated, so malformed or truncated input returns a clean error instead of panicking or over-allocating
- **Reference-decoder-safe framing** - one-shot output above the internal window cap, dictionary frames, and frames without a stored content size get an explicit, bounded `Window_Descriptor`, so they stay decodable by reference decoders with a default `windowLogMax`

All features are implemented and tested. API is stable. `BlockType`/`LiteralsBlockType` are `#[non_exhaustive]` ahead of the crate's 1.0 release, so `match` expressions over them need a wildcard arm.

## Quick Start

```rust
use oxiarc_zstd::{compress_with_level, decompress};

// Compress data
let original = b"Hello, Zstandard! ".repeat(100);
let compressed = compress_with_level(&original, 3)?; // Level 3

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
use oxiarc_zstd::ZstdEncoder;

// Use all available CPU cores (requires the `parallel` feature)
let mut encoder = ZstdEncoder::new();
encoder.set_level(3);
let compressed = encoder.compress_parallel(&data)?;
```

`oxiarc_zstd::compress_parallel(data)` is also available as a free function for the default level.

## API

### One-Shot Functions

```rust
use oxiarc_zstd::{compress_with_level, decompress};

let compressed = compress_with_level(data, level)?;
let decompressed = decompress(&compressed)?;
```

### Streaming Compression

```rust
use oxiarc_zstd::ZstdEncoder;

let mut encoder = ZstdEncoder::new();
encoder.set_level(3);
encoder.set_checksum(true);
let compressed = encoder.compress(data)?;
```

### Streaming Decompression

```rust
use oxiarc_zstd::ZstdDecoder;

let mut decoder = ZstdDecoder::new();
let decompressed = decoder.decode_frame(&compressed)?;
```

### Dictionary Compression

Training a dictionary from representative samples improves the ratio for
small, similarly-structured inputs (e.g. JSON log lines) that are too short
to build good entropy tables on their own:

```rust
use oxiarc_zstd::{ZstdEncoder, decompress_with_dict, train_dictionary};

let samples: Vec<&[u8]> = vec![b"sample one", b"sample two", b"sample three"];
let dict = train_dictionary(&samples, 4096)?;

let mut encoder = ZstdEncoder::new();
encoder.set_level(19);
encoder.set_dictionary(dict.data());
let compressed = encoder.compress(payload)?;

let decompressed = decompress_with_dict(&compressed, dict.data())?;
assert_eq!(decompressed, payload);
```

See `examples/dictionary_compress.rs` for a complete, runnable version
(`cargo run -p oxiarc-zstd --example dictionary_compress`).

## Progress Reporting and Cancellation

`ZstdEncoder`, `ZstdStreamEncoder`, and `ZstdStreamDecoder` all expose builder methods for observability and cooperative cancellation:

```rust
use std::sync::Arc;
use oxiarc_core::{CancellationToken, ProgressSink};
use oxiarc_zstd::ZstdEncoder;

// Progress reporting
let sink: Arc<dyn ProgressSink> = Arc::new(MyProgressHandler);
let mut encoder = ZstdEncoder::new();
encoder.set_level(3);
let encoder = encoder.with_progress(sink);

// Cooperative cancellation
let token = CancellationToken::new();
let encoder = encoder.with_cancel(token.clone());

// Cancel from another thread
token.cancel();
```

The `with_progress` and `with_cancel` builders can be chained together.

## Features (Cargo)

| Feature | Default | Description |
|---------|---------|-------------|
| `parallel` | no | Multi-threaded block compression via Rayon |
| `zstd-oracle` | no | Live differential tests against the reference `zstd` CLI (`cargo test -p oxiarc-zstd --features zstd-oracle`); tests self-skip when `zstd` is not on PATH |

```toml
[dependencies]
# Default (no parallel)
oxiarc-zstd = "0.4.2"

# With parallel compression
oxiarc-zstd = { version = "0.4.2", features = ["parallel"] }
```

## Algorithm

Zstandard uses a sophisticated multi-stage approach:
1. **LZ77 matching** - Find repeated sequences (levels 1-22; deeper search at higher levels)
2. **Huffman literals** - The encoder emits Huffman-compressed literal sections when they beat Raw/RLE (each section is self-verified before use); the decoder supports the full 1- and 4-stream formats including FSE-compressed weight tables
3. **Finite State Entropy (FSE)** - Sequences (literal/match lengths, offsets) are entropy-coded with the RFC 8878 predefined tables (or RLE tables for constant symbol categories); the decoder additionally handles custom `FSE_Compressed` tables and repeat modes
4. **Block structure** - Independent blocks for parallelization

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
