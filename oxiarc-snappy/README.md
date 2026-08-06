
# oxiarc-snappy [Stable]

Pure Rust Snappy compression library, part of the OxiArc ecosystem.

![Version](https://img.shields.io/badge/version-0.4.2-blue)
![Tests](https://img.shields.io/badge/tests-114%20passing-brightgreen)
![License](https://img.shields.io/badge/license-Apache--2.0-green)
![Status](https://img.shields.io/badge/status-Stable-brightgreen)

**Version: 0.4.2 (2026-08-06) | 140 tests passing**

## Features

- **Pure Rust** — No C dependencies or unsafe FFI
- **Snappy block format** — Fast in-memory compress/decompress
- **Snappy framing format** — Streaming API with CRC32C checksums per chunk
- **Streaming API** — `FrameEncoder<W: Write>` and `FrameDecoder<R: Read>` for incremental processing
- **SSE 4.2 CRC32C hardware acceleration** — x86_64 builds use `_mm_crc32_u64` intrinsics via runtime dispatch (`OnceLock`), falling back to pure-Rust software CRC32C automatically
- **Parallel frame compression** — `compress_parallel` splits input into chunks and compresses them concurrently via Rayon (requires `parallel` feature flag)
- **Google Snappy interop** — 16 integration tests validate wire-format compatibility against Google Snappy golden vectors

All features are implemented and tested. API is stable.

## Quick Start

Add to your `Cargo.toml`:

```toml
[dependencies]
oxiarc-snappy = "0.4.2"
```

### Block Format

```rust
use oxiarc_snappy::{compress, decompress};

let data = b"Hello, World! Hello, World!";
let compressed = compress(data);
let decompressed = decompress(&compressed)?;
assert_eq!(decompressed, data);
```

### Framing Format (Streaming)

```rust
use oxiarc_snappy::{FrameEncoder, FrameDecoder};
use std::io::{Write, Read};

// Compress with streaming encoder
let mut compressed = Vec::new();
{
    let mut encoder = FrameEncoder::new(&mut compressed);
    encoder.write_all(b"Hello, streaming Snappy!")?;
    encoder.finish()?;
}

// Decompress with streaming decoder
let mut decoder = FrameDecoder::new(&compressed[..]);
let mut output = Vec::new();
decoder.read_to_end(&mut output)?;
assert_eq!(output, b"Hello, streaming Snappy!");
```

## API Overview

| Item | Kind | Description |
|------|------|-------------|
| `compress(data: &[u8]) -> Vec<u8>` | function | Snappy block compression |
| `decompress(data: &[u8]) -> Result<Vec<u8>, SnappyError>` | function | Snappy block decompression |
| `max_compress_len(input_len: usize) -> usize` | function | Upper bound on compressed size |
| `decompress_len(compressed: &[u8]) -> Result<usize, SnappyError>` | function | Decode uncompressed length from header |
| `crc32c(data: &[u8]) -> u32` | function | Castagnoli CRC32C checksum |
| `mask_checksum(crc: u32) -> u32` | function | Apply Snappy masking to CRC32C |
| `unmask_checksum(masked: u32) -> u32` | function | Remove Snappy masking from CRC32C |
| `masked_crc32c(data: &[u8]) -> u32` | function | Compute masked CRC32C in one step |
| `FrameEncoder<W>` | struct | Streaming framing encoder (implements `Write`) |
| `FrameDecoder<R>` | struct | Streaming framing decoder (implements `Read`) |
| `SnappyError` | enum | Error type for decompression and framing failures |

## Feature Flags

| Feature | Default | Description |
|---------|---------|-------------|
| `parallel` | no | Multi-threaded frame compression via Rayon (`compress_parallel`) |
| `async-io` | no | `AsyncSnappyCompressor`/`AsyncSnappyDecompressor` (`oxiarc_core::async_io` traits) for `tokio`-based async I/O; reads the input fully before compressing/decompressing synchronously (not bounded-memory streaming) |

All other functionality — block format, framing format, CRC32C (with SSE 4.2 hardware acceleration on x86_64), and the `SnappyPool` buffer pool (`SnappyPool`, `FrameEncoder::with_pool`, `FrameDecoder::with_pool`, `compress_frame_pooled`) — is enabled by default.

```toml
[dependencies]
# Default (no parallel)
oxiarc-snappy = "0.4.2"

# With parallel compression
oxiarc-snappy = { version = "0.4.2", features = ["parallel"] }
```

## CRC32C

The Snappy framing format requires a masked CRC32C checksum for each chunk. `oxiarc-snappy` includes a pure-Rust CRC32C implementation (Castagnoli polynomial, slicing-by-4 optimised).

### Hardware Acceleration (SSE 4.2 on x86_64)

On x86_64 hosts, the CRC32C path is accelerated at runtime when SSE 4.2 is available:

- Checks `is_x86_feature_detected!("sse4.2")` once at startup via `OnceLock`.
- Fast path uses `_mm_crc32_u64` (8 bytes/cycle), with `_mm_crc32_u8` for trailing 1–7 bytes.
- The output is bitwise-identical to the scalar path — no difference in correctness.
- On non-x86_64 platforms (e.g. aarch64/macOS) and on x86_64 without SSE 4.2, the scalar fallback is used transparently.

## What's new in 0.3.6

- **`SnappyError` is now `#[non_exhaustive]`** (pre-1.0 API-stability freeze); its `Display`/`Error`/`From<io::Error>` impls are now generated via `thiserror` instead of hand-written (messages are unchanged). Downstream `match` expressions on `SnappyError` must include a wildcard arm.
- New `proptest`-based round-trip regression suite (`tests/proptest_roundtrip.rs`): decompression never panics on arbitrary input, and compress→decompress round-trips.
- New `pooled_streaming` example demonstrating `SnappyPool`-backed `FrameEncoder`/`FrameDecoder` and `compress_frame_pooled` across many repeated operations, including `SnappyPool::stats()` hit/allocation counters.
- Previously `ignore`-fenced doctests (including the `async-io` examples) now compile and run as part of `cargo test`.
- Expanded rustdoc on `compress()` documenting why it deliberately returns `Vec<u8>` rather than `Result<Vec<u8>, SnappyError>` (block compression has no fallible steps).

## What's new in 0.3.1

16 interop integration tests against Google Snappy wire-format golden vectors:

- Empty input roundtrip
- Single-byte roundtrip
- 64 KiB boundary roundtrip
- 64 KiB + 1 byte roundtrip
- `max_compress_len` invariant verification
- Arbitrary-data roundtrip
- Crafted-stream decode
- Truncated-varint rejection
- Oversized-varint rejection
- And 7 additional wire-format compatibility cases

## Part of OxiArc

This crate is part of the [OxiArc](https://github.com/cool-japan/oxiarc) project — a Pure Rust archive and compression library ecosystem.

## Documentation

Full API documentation: <https://docs.rs/oxiarc-snappy>

## License

Apache-2.0
