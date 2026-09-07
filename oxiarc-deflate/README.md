
# oxiarc-deflate [Stable]

Pure Rust implementation of the DEFLATE compression algorithm (RFC 1951).

![Version](https://img.shields.io/badge/version-0.4.2-blue)
![License](https://img.shields.io/badge/license-Apache--2.0-green)
![Status](https://img.shields.io/badge/status-Stable-brightgreen)

**Version 0.4.2** (2026-09-07) — 422 tests passing (381 via `cargo nextest run --all-features` + 41 doctests).

**What's new in 0.4.2**: **Resumable inflate, and every decode path re-based on it.** New `InflateStream` (raw DEFLATE) and `WrappedInflate` (gzip/zlib/raw/auto framing) are push decoders: an arbitrary byte split of the same stream yields byte-identical output, because a half-parsed Huffman header, a partially consumed byte and a half-copied match all survive across calls. New `InflateReader<R: Read>` and `AsyncInflateReader<R: AsyncRead>` (feature `async-io`) drive them from any source, with a mandatory 64 KiB output staging buffer, `Interrupted` retry, `WouldBlock` propagated (never turned into a bogus end-of-stream), and truncation reported as an `io::Error` rather than a short read.

`GzipStreamDecoder` and `ZlibStreamDecoder` no longer call `read_to_end`: they serve the first byte without reading the whole stream, and peak memory drops from `O(compressed + decompressed)` to a fixed 64 KiB in + 64 KiB out + 32 KiB history. `ZlibStreamDecoder::with_max_output` is now enforced *during* decoding — inside a single DEFLATE block, so a one-block bomb is stopped at the limit instead of after full expansion — and `decompressed_size()` now means "produced so far". `Decompressor for Inflater` is genuinely incremental with a **sticky fault latch**: a second call after a mid-stream EOF returns the error instead of `Ok((0, n, Done))` with silently truncated output. `RawInflateReader` (RFC 4978) delivers bytes as they decode instead of materialising a whole sync-flush unit, and the async `AsyncDecompressor` path is a bounded pump instead of buffering both the compressed and the decompressed stream. `gzip_decompress` now verifies the `FHCRC` header checksum when present (previously skipped). Decode throughput went **up**: `inflate()` is 12-30 % faster than before on text/random/repetitive/zeros. New `examples/http_body_inflate.rs` shows an HTTP body decoded from a chunked, occasionally-`WouldBlock` source.

**What's new in 0.4.0**: DEFLATE/zlib decoder performance rewrite — no wire-format change, no public API removed. New `inflate_into(src, dst) -> Result<usize>` decompresses a raw DEFLATE stream directly into a caller-supplied buffer with no intermediate `Vec` and no output-size guessing (`BufferTooSmall` if the stream would overflow `dst`, never silently truncated; `InvalidDistance` if a back-reference reaches before the start of `dst` — use `Inflater::with_dictionary` when history before `dst` is needed instead). `zlib::zlib_decompress_into` is the zlib-wrapper equivalent — validates the header, decodes via `inflate_into`, and verifies the trailing Adler-32. New `Inflater::with_output_capacity(size_hint)` pre-sizes the output buffer from a size hint (clamped to the new `MAX_OUTPUT_CAPACITY_HINT` = 64 MiB, since the hint is untrusted); GZIP decoding now seeds this automatically from the trailing ISIZE field. Internally (no API change): `HuffmanTree` now decodes through a two-level root+sub-table (root widened from a 9-bit to a 10-bit table, zlib/libdeflate style); the LZ77 history is now the output buffer itself (`InflateWindow`, via `Vec::extend_from_within`) rather than a separate ring buffer that wrote every decoded byte twice; `Adler32::update` now folds 32-byte groups through a closed-form reduction instead of one add-pair per byte so the compiler can auto-vectorize it. These decoders build on `oxiarc-core`'s new buffered `BitReader`/`BitCache`. New differential test suite `tests/inflate_differential.rs` proves the buffered fast path, the exact-mode path, and `inflate_into`/`zlib_decompress_into` all agree byte-for-byte, including hostile/truncated/corrupted input, plus a new `fuzz_inflate_into` fuzz target.

**What's new in 0.3.6**: New `gzip_streaming` and `parallel_gzip` runnable examples; `#[must_use]` added to the LZ77-heuristics builder setters (`with_nice_length`, `with_min_match_length`, `with_max_chain`, `with_good_length`, `with_lz77_params`) and to `ParallelGzipEncoder`'s builder setters (`level`, `chunk_size`, `num_threads`), so a discarded builder return value now warns; new `proptest`-based round-trip test suite (`tests/proptest_roundtrip.rs`); a decoder-only regression test for a hand-built fixed-Huffman length-258 back-reference closes a coverage gap. `oxiarc-core::FlushMode` (used by `Deflater`) is now `#[non_exhaustive]` as part of a pre-1.0 API freeze — the internal flush-mode dispatch already carries a forward-compatible wildcard arm.

**What's new in 0.3.0–0.3.2**:
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
- **Resumable push decoding** - `InflateStream`/`WrappedInflate` accept an arbitrary byte split of the compressed stream
- **Bounded-memory readers** - `InflateReader`/`AsyncInflateReader` decode gzip/zlib/raw from any `Read`/`AsyncRead` in constant memory
- **Decompression-bomb limits** - `with_max_output` / `with_ratio_guard`, enforced inside a block, not merely between blocks
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
| `default` | yes | DEFLATE compression/decompression, LZ77, Huffman, OptimalParser, LZ77 heuristics, DeflatePool, the resumable `InflateStream`/`WrappedInflate` core and `InflateReader` |
| `async-io` | no | Async streaming I/O via Tokio (enables the `async_deflate`, `async_reader` and `raw_stream` modules: `AsyncInflateReader`, `RawDeflateWriter`/`RawInflateReader`) |
| `parallel` | no | Multi-threaded GZIP compression via `gzip_compress_parallel` and `ParallelGzipEncoder` |

## Usage

Add to your `Cargo.toml`:

```toml
[dependencies]
oxiarc-deflate = "0.4.2"
```

With async I/O support:

```toml
[dependencies]
oxiarc-deflate = { version = "0.4.2", features = ["async-io"] }
```

With parallel GZIP compression:

```toml
[dependencies]
oxiarc-deflate = { version = "0.4.2", features = ["parallel"] }
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

## API Overview

| Item | What it is |
|------|------------|
| `deflate(data, level)` / `inflate(data)` | One-shot raw DEFLATE (RFC 1951) |
| `inflate_into(src, dst)` | Decode into a caller-supplied buffer; `BufferTooSmall`, never truncation |
| `Deflater` / `Inflater` | Encoder / decoder objects (`Compressor` / `Decompressor` traits) |
| `InflateStream` | Resumable raw-DEFLATE push decoder: `inflate(input, output, flush)` |
| `WrappedInflate` | The same, plus gzip/zlib/auto framing, multi-member and trailing-byte policy |
| `InflateReader<R: Read>` | Bounded-memory `Read` adapter over `WrappedInflate` |
| `AsyncInflateReader<R: AsyncRead>` | The async twin (`async-io`) |
| `GzipStreamDecoder` / `ZlibStreamDecoder` | Lenient legacy `Read` decoders (now incremental) |
| `GzipStreamEncoder` / `ZlibStreamEncoder` | `Write` encoders with `sync_flush` / `full_flush` / `partial_flush` |
| `gzip_compress` / `gzip_decompress` | One-shot GZIP (RFC 1952), multi-member on decode |
| `zlib_compress` / `zlib_decompress` | One-shot zlib (RFC 1950); exact-slice input |
| `RawDeflateWriter` / `RawInflateReader` | RFC 4978 IMAP `COMPRESS=DEFLATE` (`async-io`) |
| `Lz77Encoder`, `HuffmanTree`, `OptimalParser`, `DeflatePool` | Building blocks |

## Streaming Decode

### `InflateReader` — a `Read` over a `Read`

```rust
use std::io::Read;
use oxiarc_deflate::{InflateReader, InflateWrapper};

// gzip / zlib / raw / auto-sniff are all the same type.
let mut reader = InflateReader::new(response_body, InflateWrapper::Gzip)
    .with_max_output(8 * 1024 * 1024)      // cap a decompression bomb
    .with_ratio_guard(200.0, 64 * 1024);   // ... and its expansion ratio

let mut plain = Vec::new();
reader.read_to_end(&mut plain)?;
println!("{} compressed bytes -> {} bytes", reader.total_in(), reader.total_out());
```

Peak memory is a fixed 64 KiB of compressed staging + 64 KiB of decoded
staging + the 32 KiB LZ77 history, whatever the size of the stream. The
inner reader's `Interrupted` is retried, its `WouldBlock` is propagated
unchanged, and a source that stops mid-member is an `io::Error` rather than
a short read. See `examples/http_body_inflate.rs`.

### `InflateStream` / `WrappedInflate` — push decoding

When the bytes arrive as chunks you do not control (a socket, a PNG `IDAT`
chain, a TIFF strip), drive the core directly and pass the flush mode that
says whether more input can still arrive:

```rust
use oxiarc_core::traits::FlushMode;
use oxiarc_deflate::{InflateStatus, InflateStream};

let mut stream = InflateStream::new();
let mut out = [0u8; 4096];
let progress = stream.inflate(chunk, &mut out, FlushMode::None)?;
// progress.consumed / progress.produced / progress.status
assert_ne!(progress.status, InflateStatus::StreamEnd);
```

| Consumer | `flush` |
|----------|---------|
| Whole compressed slice in hand (TIFF strip, APNG frame) | `Finish` |
| Chunked source (HTTP body, PNG `IDAT` chain) | `None` until EOF, then `Finish` |
| RFC 4978 peer (may send more at any time) | `None` always |
| `Decompressor::decompress` (whole-remaining-input contract) | `Finish` |

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
use oxiarc_core::traits::{Compressor, Decompressor};
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

`compress_all`/`decompress`/`decompress_all` are default/trait methods from
`oxiarc_core::traits::{Compressor, Decompressor}` — that trait must be in
scope to call them, as shown above.

### LZ77 Encoder

```rust
use oxiarc_deflate::{Lz77Encoder, Lz77Token};

let mut encoder = Lz77Encoder::with_level(6);
for token in encoder.compress(data) {
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
let tree = HuffmanTree::from_code_lengths(&lengths)?;

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
| `gzip` | GZIP format (RFC 1952) encoding and decoding |
| `parallel` | Multi-threaded GZIP/DEFLATE compression (requires `parallel` feature): `gzip_compress_parallel`, `compress_deflate_parallel`, `ParallelGzipEncoder` |
| `pool` | `DeflatePool` and `PoolStats` for window/hash buffer reuse |
| `async_deflate` | Async streaming compression/decompression (requires `async-io` feature) |
| `stream` | `InflateStream`, `InflateProgress`, `InflateStatus` — the resumable raw-DEFLATE core |
| `wrapper` | `WrappedInflate`, `InflateWrapper`, `TrailingPolicy`, `GzipHeaderInfo` |
| `reader` | `InflateReader` — the bounded-memory `Read` adapter |
| `async_reader` | `AsyncInflateReader` (requires `async-io` feature) |
| `streaming` | `GzipStreamEncoder`/`Decoder`, `ZlibStreamEncoder`/`Decoder` |
| `raw_stream` | RFC 4978 `RawDeflateWriter`/`RawInflateReader` (requires `async-io` feature) |

### GZIP API

```rust
use oxiarc_deflate::gzip::{gzip_compress, gzip_decompress};

// Encode to GZIP format
let compressed = gzip_compress(data, 6)?;

// Decode GZIP data
let decompressed = gzip_decompress(&compressed)?;
```

### Zero-Copy Decode (`inflate_into`)

Decompress directly into a caller-supplied buffer — no intermediate `Vec`
and no output-size guessing. Returns the number of bytes written; a stream
that would overflow `dst` is rejected with `BufferTooSmall` rather than
truncated silently.

```rust
use oxiarc_deflate::{deflate, inflate_into};

let original = b"Hello, World! Hello, World!";
let compressed = deflate(original, 6)?;

let mut out = vec![0u8; original.len()];
let n = inflate_into(&compressed, &mut out)?;
assert_eq!(&out[..n], original);
```

`zlib::zlib_decompress_into` is the zlib-wrapped equivalent, additionally
verifying the trailing Adler-32 checksum.

### Async DEFLATE (requires `async-io` feature)

The `async_deflate` module implements `oxiarc_core`'s `AsyncCompressor`/
`AsyncDecompressor` traits directly on the existing `Deflater`/`Inflater`
types (there is no separate `AsyncDeflater`/`AsyncInflater` type) — wrap
either in the corresponding `oxiarc_core::async_io` adapter to drive it from
an `AsyncRead`/`AsyncWrite` source:

```rust
use oxiarc_core::async_io::{
    AsyncCompressor, AsyncCompressorWrapper, AsyncDecompressor, AsyncDecompressorWrapper,
};
use oxiarc_deflate::{Deflater, Inflater};

// Async compression — Deflater implements AsyncCompressor directly.
let mut compressor = AsyncCompressorWrapper::new(Deflater::new(6));
let mut compressed = Vec::new();
compressor.compress_async(&mut reader, &mut compressed).await?;

// Async decompression — Inflater implements AsyncDecompressor directly.
let mut decompressor = AsyncDecompressorWrapper::new(Inflater::new());
let mut decompressed = Vec::new();
decompressor.decompress_async(&mut reader, &mut decompressed).await?;
```

### Parallel GZIP (requires `parallel` feature)

```rust
use oxiarc_deflate::{gzip_compress_parallel, ParallelGzipEncoder};

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
    .compress_to_vec(data)?;

// Fine-grained control
let params = Lz77Params {
    nice_length: 128,
    max_chain: 256,
    good_length: 32,
};
let compressed = Deflater::new(9)
    .with_lz77_params(params)
    .compress_to_vec(data)?;
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
    .compress_to_vec(data)?;

// Inspect pool statistics
let stats = pool.stats();
println!("window_hits={} window_allocations={}", stats.window_hits, stats.window_allocations);
```

## Performance

Compression ratios on typical data (Calgary Corpus):

| File | Original | Compressed | Ratio |
|------|----------|------------|-------|
| book1 | 768771 | ~300000 | ~61% |
| paper1 | 53161 | ~18000 | ~66% |
| progc | 39611 | ~13000 | ~67% |

## Test Coverage

422 tests (381 via `cargo nextest run -p oxiarc-deflate --all-features` + 41
doctests), zero clippy warnings with `--all-features --all-targets` and with
`--no-default-features`.

| Suite | Covers |
|-------|--------|
| `tests/inflate_stream.rs` | Split invariance (byte-at-a-time input, 1-byte output, split at every offset, proptest schedules), all 16 gzip header-flag combinations, concatenated zlib with 1-3 byte accumulator residue, truncation at every offset with a bounded call count, `max_output` inside a single 812 KB -> 123 MiB block, CAB-style reset/`set_dictionary` cycles |
| `tests/inflate_reader.rs` | `Interrupted` retry, `WouldBlock` propagation, truncation as `io::Error`, the zlib 1-5 byte trailing-fragment rule (with a negative control), raw padding-bit vs trailing-byte framing, 1/3/7/64 KiB read patterns, the legacy-vs-strict split, `Decompressor` sticky-fault two-call regression |
| `tests/inflate_differential.rs` | Seven decode paths compared byte-for-byte over the whole corpus at four levels: `inflate`, exact-mode `BitReader`, `inflate_into`, `InflateStream` at three granularities, `inflate_to_vec`, `WrappedInflate`, `InflateReader` |
| `tests/wrapper_regressions.rs` | The DEFLATE-01..05 wrapper defects, with CPython-produced gzip/zlib fixtures |
| `tests/compliance.rs`, `tests/edge_cases.rs`, `tests/proptest_roundtrip.rs` | RFC 1951 block types, spec-inflater cross-checks, round-trip properties |
| `tests/zlib_oracle.rs` (`zlib-oracle` feature) | Live differential tests against CPython `zlib`/`gzip` and the system `gzip` CLI, in both directions and through both `Read` adapters. Self-skips when the tools are absent |

## Performance Notes

Interleaved A/B, best-of-40 rounds, 200 KB payloads at level 6 (negative =
faster than the previous implementation):

| Corpus | `inflate()` | `inflate_into()` |
|--------|-------------|------------------|
| text | -12 % | +8 % |
| random | -30 % | -0.4 % |
| repetitive | -12 % | -38 % |
| zeros | -27 % | ~0 % |

`GzipStreamDecoder` at a 64 KiB caller buffer is within +-3 % of the
one-shot `gzip_decompress`, while using bounded memory instead of holding
both the compressed and the decompressed stream.

## References

- [RFC 1951 - DEFLATE Compressed Data Format Specification](https://www.rfc-editor.org/rfc/rfc1951)
- [An Explanation of the Deflate Algorithm](https://zlib.net/feldspar.html)

## License

Apache-2.0
