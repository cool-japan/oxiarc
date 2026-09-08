
# oxiarc-deflate [Stable]

Pure Rust implementation of the DEFLATE compression algorithm (RFC 1951).

![Version](https://img.shields.io/badge/version-0.4.2-blue)
![License](https://img.shields.io/badge/license-Apache--2.0-green)
![Status](https://img.shields.io/badge/status-Stable-brightgreen)

**Version 0.4.2** (2026-09-07) — 422 tests passing (381 via `cargo nextest run --all-features` + 41 doctests).

**What's new in 0.4.2**: **Resumable inflate, and every decode path re-based on it.** New `InflateStream` (raw DEFLATE) and `WrappedInflate` (gzip/zlib/raw/auto framing) are push decoders: an arbitrary byte split of the same stream yields byte-identical output, because a half-parsed Huffman header, a partially consumed byte and a half-copied match all survive across calls. New `InflateReader<R: Read>` and `AsyncInflateReader<R: AsyncRead>` (feature `async-io`) drive them from any source, with a mandatory 64 KiB output staging buffer, `Interrupted` retry, `WouldBlock` propagated (never turned into a bogus end-of-stream), and truncation reported as an `io::Error` rather than a short read.

`GzipStreamDecoder` and `ZlibStreamDecoder` no longer call `read_to_end`: they serve the first byte without reading the whole stream, and peak memory drops from `O(compressed + decompressed)` to a fixed 64 KiB in + 64 KiB out + 32 KiB history. `ZlibStreamDecoder::with_max_output` is now enforced *during* decoding — inside a single DEFLATE block, so a one-block bomb is stopped at the limit instead of after full expansion — and `decompressed_size()` now means "produced so far". `Decompressor for Inflater` is genuinely incremental with a **sticky fault latch**: a second call after a mid-stream EOF returns the error instead of `Ok((0, n, Done))` with silently truncated output. `RawInflateReader` (RFC 4978) delivers bytes as they decode instead of materialising a whole sync-flush unit, and the async `AsyncDecompressor` path is a bounded pump instead of buffering both the compressed and the decompressed stream. `gzip_decompress` now verifies the `FHCRC` header checksum when present (previously skipped). Decode throughput went **up**: `inflate()` is 12-30 % faster than before on text/random/repetitive/zeros. New `examples/http_body_inflate.rs` shows an HTTP body decoded from a chunked, occasionally-`WouldBlock` source.

**Also new in 0.4.2**: **the encoder is a faithful port of zlib's `deflate.c`/`trees.c`.** `Deflater` now uses a persistent 32 KiB window with slid (not rebuilt) hash chains, zlib's per-level `configuration_table`, `deflate_fast` at levels 1-3 and `deflate_slow` lazy matching at 4-9 (with the `TOO_FAR` rule), blocks cut at 16 383 symbols, and a per-block stored/fixed/dynamic choice made on real bit costs. The result is **byte-identical to CPython's `zlib.compress(data, level)` at every level 1-9** on every corpus tested (source text, HTML, log lines, binary records, runs, random, and four PNG-filtered scanline fixtures) — verified as a gate, not as a claim (`tests/zlib_encoder_oracle.rs`). Before this, output was up to **179 % larger** than zlib's on the same bytes.

All encoder state now persists across calls, so **the call size no longer changes anything**: feeding a 2 MiB stream as 1 KiB calls costs the same as one 2 MiB call (32.3 ms vs 30.8 ms) and produces the *same bytes*. Previously a small-call stream cost 13x more, because every call restarted the match finder and emitted its own block and Huffman tree. Level-6 throughput is now 0.77x-1.40x of CPython `zlib` (same miniz-class algorithm) depending on the corpus, against 0.10x-0.40x before.

`Deflater::with_optimal_parsing(level)` (the graph-based DP parser) is now **never larger than the default ladder** at the same level — it was up to 2 % *larger* on noisy image rows, because its candidate set ignored zlib's `TOO_FAR` rule and bought rare long-distance codes for 3-byte matches. Measured gains over level 9 on a 96 KiB slice of each corpus: -1.3 % to -9.9 % (0 % on random). It is also call-size invariant now: it waits for a whole parse span instead of running one as soon as 262 bytes are buffered, which used to make a byte-at-a-time caller run a 259-position candidate collection per emitted byte (measured on 200 KB of log lines at level 9: 4.17 s and 20 277 bytes in 1-byte calls, against 0.31 s and 19 920 bytes in one call; now 0.36 s and 19 920 bytes at every call size). New `Deflater::with_strategy` exposes zlib's `Z_FILTERED` / `Z_HUFFMAN_ONLY` / `Z_RLE` / `Z_FIXED`, byte-identical to CPython under each.

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

The level selects a row of zlib's `configuration_table`, so a level here means
exactly what it means in zlib (and therefore in CPython's `zlib` module, gzip,
PNG and every other zlib consumer):

| Level | Inner loop | `good_length` | `max_lazy` | `nice_length` | `max_chain` | Use case |
|-------|-----------|--------------:|-----------:|--------------:|------------:|----------|
| 0 | stored only | - | - | - | - | Already compressed data |
| 1 | `deflate_fast` (greedy) | 4 | 4 | 8 | 4 | Real-time streaming |
| 2 | `deflate_fast` | 4 | 5 | 16 | 8 | |
| 3 | `deflate_fast` | 4 | 6 | 32 | 32 | |
| 4 | `deflate_slow` (lazy) | 4 | 4 | 16 | 16 | |
| 5 | `deflate_slow` | 8 | 16 | 32 | 32 | |
| 6 | `deflate_slow` | 8 | 16 | 128 | 128 | General purpose (default) |
| 7 | `deflate_slow` | 8 | 32 | 128 | 256 | |
| 8 | `deflate_slow` | 32 | 128 | 258 | 1024 | Archival |
| 9 | `deflate_slow` | 32 | 258 | 258 | 4096 | Maximum |

`Lz77Params::for_level(n)` returns the row, and `Deflater::with_lz77_params`
replaces it if you want a level's search effort with one knob changed.

Note that compressed size is **not** monotone in the level — that is true of
zlib too, and this encoder reproduces its bytes exactly. Measured examples:
the `runs` corpus goes 370, 371, 380, 380 bytes over levels 4-7, and a
photo-like PNG fixture goes 159 783 -> 159 784 from level 2 to level 3. What
holds everywhere is that level 9 is never beaten by another level.

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
| `optimal` | `OptimalParser`, the graph-based (Zopfli-style) token parser |
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

Reproduce the *encoder* numbers below with `cargo run --release --example
zlib_ab` and the *decoder* numbers with `cargo run --release --example
inflate_ab` (both criterion-free; the reference columns need `python3`).

### Compressed size

The encoder emits the **same bytes** as `python3 -c "zlib.compress(data, n)"`
for every level 1-9 on every corpus in `tests/common/corpus.rs`, so the size
delta is `+0.00 %` everywhere. This is asserted as byte-identity, not as a
size band, by `tests/zlib_encoder_oracle.rs` behind the `zlib-oracle` feature.

### Optimal parser (opt-in, `Deflater::with_optimal_parsing`)

96 KiB slice of each corpus; negative is smaller than the default ladder at the
same level:

| Corpus | vs level 6 | vs level 9 |
|--------|-----------:|-----------:|
| source text | -3.6 % | -3.0 % |
| HTML | -4.0 % | -3.8 % |
| log lines | -14.3 % | -9.9 % |
| binary records | -2.0 % | -1.8 % |
| runs | -17.5 % | -5.5 % |
| random | 0.0 % | 0.0 % |
| PNG rows, gradient + noise | -2.4 % | -2.4 % |
| PNG rows, photo-like | -1.3 % | -1.3 % |
| PNG rows, text-like | -3.6 % | -4.3 % |

It costs encode time, never ratio: each span is parsed twice (shortest path and
a zlib-equivalent lazy parse over the same candidates) and the cheaper one wins
on the *real* block cost, tree description included.

### Encode throughput

Interleaved A/B against CPython `zlib` (the same miniz-class algorithm), best
of 3, timed inside python with `time.perf_counter()` so process spawn never
enters the number. macOS aarch64, release:

| Level | oxiarc / python |
|-------|-----------------|
| 1 | 0.76x - 1.99x |
| 6 | 0.77x - 1.40x |
| 9 | 0.83x - 1.42x |

The low end is incompressible data (where both encoders are memcpy-bound) and
the high end is run-heavy data.

### Per-call cost

2 MiB of source text at level 6, fed in calls of the given size. The cost is
proportional to the input, not to the number of calls, and the output is
byte-identical at every call size:

| Call size | Calls | Time | Output |
|-----------|------:|-----:|-------:|
| 1 KiB | 2048 | 32.3 ms | 476 492 B |
| 4 KiB | 512 | 32.3 ms | 476 492 B |
| 32 KiB | 64 | 31.0 ms | 476 492 B |
| 1 MiB | 2 | 30.8 ms | 476 492 B |

(Best of three `cargo run --release --example zlib_ab` runs; single-run wall
times vary by ~20 %, so the load-bearing column is the output size, which is
identical at every call size. The absolute byte count drifts between commits
because the "source text" corpus is built from this workspace's own `.rs`
files — see `tests/common/corpus.rs`.)

The opt-in optimal parser holds to the same rule. 200 KB of log lines at
level 9, `Deflater::with_optimal_parsing(9)`:

| Call size | Time | Output |
|-----------|-----:|-------:|
| 1 B | 357 ms | 19 920 B |
| 16 B | 475 ms | 19 920 B |
| 258 B | 317 ms | 19 920 B |
| 4 KiB | 328 ms | 19 920 B |
| 1 MiB | 330 ms | 19 920 B |

Level 0 is the documented exception to byte-identity across call sizes: stored
blocks are cut from whatever is buffered when a call arrives (zlib's
`deflate_stored` cuts on `avail_out` for the same reason), so small calls can
carry more 5-byte block headers. The decoded bytes are of course identical, no
block ever exceeds the format's 65 535-byte maximum, and one-shot output stays
the tightest layout — all asserted by
`tests/encoder_behaviour.rs::level_0_streaming_stays_stored_and_bounded_at_every_call_size`.

## Test Coverage

484 tests (432 via `cargo nextest run -p oxiarc-deflate --all-features` + 52
doctests; 364 with `--no-default-features`), zero clippy warnings with
`--all-features --all-targets` and with `--no-default-features`.

| Suite | Covers |
|-------|--------|
| `tests/inflate_stream.rs` | Split invariance (byte-at-a-time input, 1-byte output, split at every offset, proptest schedules), all 16 gzip header-flag combinations, concatenated zlib with 1-3 byte accumulator residue, truncation at every offset with a bounded call count, `max_output` inside a single 812 KB -> 123 MiB block, CAB-style reset/`set_dictionary` cycles |
| `tests/inflate_reader.rs` | `Interrupted` retry, `WouldBlock` propagation, truncation as `io::Error`, the zlib 1-5 byte trailing-fragment rule (with a negative control), raw padding-bit vs trailing-byte framing, 1/3/7/64 KiB read patterns, the legacy-vs-strict split, `Decompressor` sticky-fault two-call regression |
| `tests/inflate_differential.rs` | Seven decode paths compared byte-for-byte over the whole corpus at four levels: `inflate`, exact-mode `BitReader`, `inflate_into`, `InflateStream` at three granularities, `inflate_to_vec`, `WrappedInflate`, `InflateReader` |
| `tests/wrapper_regressions.rs` | The DEFLATE-01..05 wrapper defects, with CPython-produced gzip/zlib fixtures |
| `tests/compliance.rs`, `tests/edge_cases.rs`, `tests/proptest_roundtrip.rs` | RFC 1951 block types, spec-inflater cross-checks, round-trip properties |
| `tests/zlib_oracle.rs` (`zlib-oracle` feature) | Live differential tests against CPython `zlib`/`gzip` and the system `gzip` CLI, in both directions and through both `Read` adapters. Self-skips when the tools are absent |
| `tests/zlib_encoder_oracle.rs` (`zlib-oracle` feature) | Encoder byte-identity with `zlib.compress` at every level 1-9 over ten corpora, plus raw DEFLATE, level-0 stored blocks, tiny/degenerate inputs, window-spanning input, multi-call streams, preset dictionaries and `Z_SYNC_FLUSH` streams |
| `tests/encoder_behaviour.rs` | The mechanisms identity is made of, with no external tool: the `configuration_table`, lazy-vs-greedy decisions on crafted inputs, `max_lazy` suppression, the strictly-longer rule, `TOO_FAR`, run shortcuts, block-type selection (via an independent block walker in `tests/common/blocks.rs`), call-size invariance (default ladder and optimal parser), the level-0 stored-block bound, cross-call matching, the level ladder, and the optimal parser over every corpus |
| `tests/encoder_adversarial.rs` | Every encoder boundary size (`MIN_MATCH`, `MAX_MATCH`, `MIN_LOOKAHEAD`, `SYM_END`, `W_SIZE`, `MAX_STORED`, `2 * W_SIZE`, the window-slide trigger) crossed with hostile shapes (zero head/tail at the window edge, periods 1/2/3/257/258/259, a single far 3-byte repeat) crossed with every level, strategy, flush mode, dictionary size and the optimal parser; plus `reset()` returning the encoder to a pristine state |
| `tests/allocation_steady_state.rs` | A counting global allocator over a many-dynamic-block stream: `InflateStream::inflate` must allocate **zero** times in the steady state, at four (chunk, output-buffer) shapes including 1-byte chunks and a 1-byte output buffer, on both a uniform and a shape-shifting fixture |

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
