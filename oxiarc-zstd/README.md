
# oxiarc-zstd [Stable]

Pure Rust implementation of Zstandard (zstd) compression algorithm.

[![Crates.io](https://img.shields.io/crates/v/oxiarc-zstd.svg)](https://crates.io/crates/oxiarc-zstd)
![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)
![Status](https://img.shields.io/badge/status-Stable-brightgreen)

**Version: 0.4.2 (2026-09-07) | 286 tests passing (274 unit/integration + 12 doctests)**

## Overview

Zstandard is a modern compression algorithm developed by Facebook (Meta), offering excellent compression ratios with fast decompression speeds. It's designed to replace older algorithms like DEFLATE and BZip2 in many applications. Version 0.3.6 hardened the frame decoder against malformed/hostile headers (bounded, `try_reserve`-based output allocation instead of trusting the untrusted `Frame_Content_Size` field outright) and made the FSE/Huffman entropy layer bit-exact with RFC 8878: the backward bitstream is read/written with the reference `BIT_*` semantics, so real `zstd`-produced frames decode byte-identically and every oxiarc-produced frame is accepted by the reference `zstd` CLI (verified continuously by the `zstd-oracle` differential test suite).

**New in 0.4.2: bounded, truly incremental decoding.** [`ZstdStream`] is a resumable push decoder — feed it any number of compressed bytes, take back any number of decompressed bytes, one at a time if you like. It keeps a real sliding-window ring (never "the output `Vec` is the window"), enforces an output budget *before* decoding wherever the format declares a size, refuses frames that declare an oversized window before allocating one, and grows the window lazily to the bytes actually produced. `ZstdStreamDecoder<R>` and the new async adapters are thin shells over it, so neither reads the whole compressed input nor materialises the whole output.


## Features

- **Pure Rust** - No C dependencies or unsafe FFI
- **Reference interoperability, both directions** - frames produced by the reference `zstd` CLI (levels 1-19, `--ultra -22`, `--long`, `--no-check`, `--no-content-size`, raw-content dictionaries, multi-frame streams) decode byte-identically, and every frame this encoder emits is accepted and correctly decoded by `zstd -d`; enforced by embedded reference-frame fixtures (always on) plus a live CLI differential suite (`zstd-oracle` feature)
- **Full RFC 8878 entropy decoding** - FSE-compressed sequence tables, 1- and 4-stream Huffman literals, repeat offsets, treeless literals, repeat table modes
- **Huffman literals on the encode path** - literal sections are Huffman-compressed when that wins (self-verified with Raw/RLE fallback)
- **Custom block-optimal FSE sequence tables** - `FSE_Compressed_Mode` is emitted when it beats RLE and the predefined tables on total bit cost (reference-faithful `FSE_normalizeCount` / `FSE_writeNCount` ports)
- **Parallel compression** - Multi-threaded block compression with Rayon (`parallel` feature)
- **Dictionary support** - Raw-content dictionaries, interoperable with `zstd -D` in both directions
- **Checksum support** - XXH64 checksums for data integrity
- **Bounded incremental decoding** - `ZstdStream` push decoder (`decode(input, output, flush)`, `finish()`, `reset()`), resumable at every input-dry point, with a real sliding-window ring, a sticky fault latch, `with_max_output` / `with_max_window` / `with_multi_frame` / `with_dictionary`, and `decompress_into` / `decompress_with_limit` / `decompress_multi_frame_with_limit` as bomb-safe one-shot helpers
- **Streaming API** - `ZstdStreamEncoder<W>` (`Write`) and `ZstdStreamDecoder<R>` (`Read`, truly incremental: it serves the first byte without reading the whole input)
- **Async I/O** (`async-io` feature) - `AsyncZstdReader<R>` (`tokio::io::AsyncRead`) and `AsyncZstdDecompressor`, both bounded and built on the same push decoder
- **Incremental XXH64** - `XxHash64::{new, with_seed, update, finish, finish_checksum}` for frame checksums computed without retaining the output
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

`ZstdStreamDecoder<R>` is a `Read` shell over the bounded push decoder: it holds
at most `window + one 128 KiB block + two 64 KiB staging buffers`, whatever the
size of the stream.

```rust
use std::io::Read;
use oxiarc_zstd::ZstdStreamDecoder;

let mut decoder = ZstdStreamDecoder::new(&compressed[..])
    .with_max_output(64 * 1024 * 1024);   // reject bombs
let mut out = Vec::new();
decoder.read_to_end(&mut out)?;
```

### Bounded push decoding (`ZstdStream`)

Use this when you own the I/O loop — an HTTP body, a socket, a TIFF strip, or
anything where the compressed bytes arrive in pieces.

```rust
use oxiarc_core::traits::FlushMode;
use oxiarc_zstd::{ZstdStatus, ZstdStream};

let mut stream = ZstdStream::new()
    .with_max_output(16 * 1024 * 1024)   // hard output cap
    .with_max_window(8 * 1024 * 1024)    // refuse oversized declared windows
    .with_multi_frame(true);             // decode concatenated frames

let mut out = Vec::new();
let mut scratch = [0u8; 64 * 1024];
let mut pos = 0;
loop {
    let end = (pos + 4096).min(compressed.len());
    let flush = if end == compressed.len() { FlushMode::Finish } else { FlushMode::None };
    let p = stream.decode(&compressed[pos..end], &mut scratch, flush)?;
    pos += p.consumed;
    out.extend_from_slice(&scratch[..p.produced]);
    if p.status == ZstdStatus::StreamEnd { break; }
}
stream.finish()?;   // verifies the frame checksum and rejects truncation
```

`decode` returns `ZstdProgress { consumed, produced, status }`, where `status` is
`NeedInput`, `NeedOutput` or `StreamEnd`. Any error is latched: every later call
returns it until `reset()`.

### Bomb-safe one-shot decoding

```rust
use oxiarc_zstd::{decompress_into, decompress_multi_frame_with_limit, decompress_with_limit};

// Into a caller-owned buffer; the buffer length *is* the budget.
let mut dst = vec![0u8; expected_len];
let n = decompress_into(&compressed, &mut dst)?;

// Or with an explicit cap, enforced while decoding.
let out = decompress_with_limit(&compressed, 64 * 1024 * 1024)?;
let all = decompress_multi_frame_with_limit(&compressed, 64 * 1024 * 1024)?;
```

### Low-level frame decoding

```rust
use oxiarc_zstd::ZstdDecoder;

let mut decoder = ZstdDecoder::new();
let decompressed = decoder.decode_frame(&compressed)?;
```

`ZstdDecoder::decode_frame` needs the whole frame in memory and has no output
cap; prefer the bounded APIs above for untrusted input.

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
| `async-io` | no | `AsyncZstdReader<R>` (`tokio::io::AsyncRead`) and `AsyncZstdDecompressor` (`oxiarc_core::async_io::AsyncDecompressor`), both bounded |
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
3. **Finite State Entropy (FSE)** - Sequences (literal/match lengths, offsets) are entropy-coded with whichever of RLE, the RFC 8878 predefined tables and a custom block-optimal `FSE_Compressed` table costs fewest bits including the table description; the decoder handles all four modes plus repeat modes
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

### Incremental decode vs one-shot

1 MiB payloads, Apple Silicon, level 3. Measured as an **interleaved A/B**
(alternating rounds, best of 40 each), which is what the target ratio needs: on a
loaded machine the absolute numbers move a long way but the ratio does not.

| Path | structured 1 MiB | vs one-shot | random 1 MiB | vs one-shot |
|------|-----------------|-------------|--------------|-------------|
| one-shot `decompress` (baseline) | 471 MiB/s | 1.00x | 4009 MiB/s | 1.00x |
| `ZstdStream`, 64 KiB chunks | 434 MiB/s | **0.92x** | 8433 MiB/s | **2.10x** |
| `decompress_into` | 432 MiB/s | 0.92x | 7333 MiB/s | 1.83x |
| `decompress_with_limit` | 430 MiB/s | 0.91x | 5633 MiB/s | 1.41x |
| `ZstdStreamDecoder::read_to_end` | 426 MiB/s | 0.90x | 4823 MiB/s | 1.20x |

The target is *incremental >= 85 % of one-shot*, and every path clears it. On
entropy-coded data the ~8 % gap is the one extra copy that bounded memory costs:
the one-shot decoder pushes each byte into a growing `Vec` once and hands the
`Vec` over, while a windowed decoder writes each byte into the ring and then
copies it out to the caller. On data that is mostly `Raw` blocks the push decoder
is 2.1x *faster*, because it writes into a pre-sized ring instead of reallocating
a `Vec` and executes matches with chunked `copy_within` runs rather than a
per-byte modulo loop.

Steady-state allocations after warm-up are **zero** over `Raw`/`RLE` blocks, and
for compressed blocks the allocation count is a function of the frame (a few
entropy tables per block) and provably independent of how the caller chunks the
input — pinned by `tests/alloc_budget.rs` with a counting global allocator.

Reproduce the full matrix, including the starved 1-byte-in / 1-byte-out
schedules, with `cargo bench -p oxiarc-zstd --bench stream_bench` (criterion's
absolute numbers are only meaningful on an otherwise idle machine).

### Typical compression comparison

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
