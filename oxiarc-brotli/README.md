
# oxiarc-brotli [Stable]

Pure Rust Brotli compression/decompression implementation (RFC 7932), part of the OxiArc ecosystem.

[![Crates.io](https://img.shields.io/crates/v/oxiarc-brotli.svg)](https://crates.io/crates/oxiarc-brotli)
![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)
![Status](https://img.shields.io/badge/status-Stable-brightgreen)

**Version: 0.3.0 (2026-05-17) | 150 tests passing**

## Features

- **Pure Rust** — No C dependencies or unsafe FFI
- **Quality levels 0–11** — Quality 0 is fastest; quality 11 is best compression
- **LZ77 with context-dependent Huffman coding** — Standard Brotli algorithm (RFC 7932)
- **Static dictionary** — RFC 7932 Appendix A, 120+ common words/phrases for improved web-content compression
- **Streaming API** — `BrotliCompressor<W: Write>` and `BrotliDecompressor<R: Read>` for incremental processing
- **One-shot API** — Convenient `compress` / `decompress` functions for simple cases
- **Configurable window size** — 16–24 bits (default: 22 = 4 MB)

All features are implemented and tested. API is stable.
- **Interop integration tests** — 19 integration tests covering all quality levels 0–11 against Brotli/Snappy interoperability scenarios (new in 0.3.0)

## Quick Start

Add to your `Cargo.toml`:

```toml
[dependencies]
oxiarc-brotli = "0.3.0"
```

### One-shot compression / decompression

```rust
use oxiarc_brotli::{compress, decompress};

// Compress at quality 6 (balanced default)
let data = b"Hello, Brotli! This is a test of pure-Rust RFC 7932 compression.";
let compressed = compress(data, 6)?;
println!("Compressed {} → {} bytes", data.len(), compressed.len());

// Decompress
let decompressed = decompress(&compressed)?;
assert_eq!(decompressed, data);
```

### Configuring compression parameters

```rust
use oxiarc_brotli::{compress_with_params, BrotliParams};

let params = BrotliParams {
    quality: 11,   // best compression
    lgwin: 24,     // 16 MB window
    lgblock: 0,    // auto block size
};
let compressed = compress_with_params(b"Hello, world!", params)?;
```

### Streaming compression

```rust
use std::io::Write;
use oxiarc_brotli::{BrotliCompressor, BrotliParams};

let mut output = Vec::new();
let mut compressor = BrotliCompressor::new(&mut output, BrotliParams::default());
compressor.write_all(b"chunk one")?;
compressor.write_all(b"chunk two")?;
let _output = compressor.finish()?;
```

### Streaming decompression

```rust
use std::io::Read;
use oxiarc_brotli::BrotliDecompressor;

let compressed: Vec<u8> = /* ... */;
let mut decompressor = BrotliDecompressor::new(&compressed[..]);
let mut output = Vec::new();
decompressor.read_to_end(&mut output)?;
```

## API Overview

| Item | Kind | Description |
|------|------|-------------|
| `compress(data, quality)` | function | One-shot compression; quality 0–11 |
| `compress_with_params(data, params)` | function | One-shot compression with full `BrotliParams` control |
| `decompress(data)` | function | One-shot decompression |
| `BrotliParams` | struct | Compression parameters: `quality`, `lgwin`, `lgblock` |
| `BrotliParams::default()` | method | quality=6, lgwin=22, lgblock=0 |
| `BrotliParams::validate()` | method | Checks that all parameters are in range |
| `BrotliParams::window_size()` | method | Returns window size in bytes (2^lgwin) |
| `BrotliCompressor<W>` | struct | Streaming compressor implementing `Write` |
| `BrotliCompressor::new(writer, params)` | method | Create a new streaming compressor |
| `BrotliCompressor::finish()` | method | Flush and finalise the compressed stream |
| `BrotliDecompressor<R>` | struct | Streaming decompressor implementing `Read` |
| `BrotliDecompressor::new(reader)` | method | Create a new streaming decompressor |
| `BrotliError` | enum | Error type for all Brotli operations |
| `BrotliResult<T>` | type alias | `Result<T, BrotliError>` |

## Feature Flags

| Feature | Default | Description |
|---------|---------|-------------|
| `parallel` | no | Rayon-based parallel compression for throughput-sensitive workloads |

All other functionality — one-shot API, streaming API, Huffman coding, LZ77 engine, static dictionary — is enabled by default with no feature flags required.

```toml
[dependencies]
# Default (no optional features)
oxiarc-brotli = "0.3.0"

# With parallel compression support
oxiarc-brotli = { version = "0.3.0", features = ["parallel"] }
```

## Algorithm

Brotli (RFC 7932) combines three techniques:

1. **LZ77** — Backward-reference matching over a sliding window (16–24 bit, configurable). Backward references and literal sequences are encoded as insert-and-copy commands.
2. **Context-dependent Huffman coding** — Up to 256 prefix code trees selected per block type; context modelling uses the previous two bytes to pick the tree, improving compression for structured data (HTML, CSS, JS).
3. **Static dictionary** — A 122,784-entry table of common English words and suffixes (RFC 7932 Appendix A) provides free matches without storing them in the compressed stream.

Quality levels map to LZ77 search depth and the number of candidate matches considered:

| Quality | Speed | Typical ratio |
|---------|-------|---------------|
| 0 | Fastest | ~70% of gzip |
| 1–4 | Fast | Comparable to gzip |
| 5–9 | Balanced | Better than gzip |
| 10–11 | Slow | Best (web-asset target) |

## Part of OxiArc

This crate is part of the [OxiArc](https://github.com/cool-japan/oxiarc) project — a Pure Rust archive and compression library ecosystem.

## Documentation

Full API documentation: <https://docs.rs/oxiarc-brotli>

## License

Apache-2.0
