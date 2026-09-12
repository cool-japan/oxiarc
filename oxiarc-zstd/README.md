
# oxiarc-zstd [Stable]

Pure Rust implementation of Zstandard (zstd) compression algorithm.

[![Crates.io](https://img.shields.io/crates/v/oxiarc-zstd.svg)](https://crates.io/crates/oxiarc-zstd)
![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)
![Status](https://img.shields.io/badge/status-Stable-brightgreen)

**Version: 0.4.3 (2026-09-08) | 356 tests passing (341 unit/integration + 15 doctests, `--all-features`)**

## Overview

Zstandard is a modern compression algorithm developed by Facebook (Meta), offering excellent compression ratios with fast decompression speeds. It's designed to replace older algorithms like DEFLATE and BZip2 in many applications. Version 0.3.6 hardened the frame decoder against malformed/hostile headers (bounded, `try_reserve`-based output allocation instead of trusting the untrusted `Frame_Content_Size` field outright) and made the FSE/Huffman entropy layer bit-exact with RFC 8878: the backward bitstream is read/written with the reference `BIT_*` semantics, so real `zstd`-produced frames decode byte-identically and every oxiarc-produced frame is accepted by the reference `zstd` CLI (verified continuously by the `zstd-oracle` differential test suite).

**New in 0.4.2: bounded, truly incremental decoding.** [`ZstdStream`] is a resumable push decoder — feed it any number of compressed bytes, take back any number of decompressed bytes, one at a time if you like. It keeps a real sliding-window ring (never "the output `Vec` is the window"), enforces an output budget *before* decoding wherever the format declares a size, refuses frames that declare an oversized window before allocating one, and grows the window lazily to `min(declared Window_Size, max(Block_Maximum_Decompressed_Size, bytes actually produced))` — the block-maximum floor (at most 128 KiB) is inherent, since one whole block has to fit before it is drained. `ZstdStreamDecoder<R>` and the new async adapters are thin shells over it, so neither reads the whole compressed input nor materialises the whole output.


## Features

- **Pure Rust** - No C dependencies or unsafe FFI
- **Reference interoperability, both directions** - frames produced by the reference `zstd` CLI (levels 1-19, `--ultra -22`, `--long`, `--no-check`, `--no-content-size`, raw-content dictionaries, multi-frame streams) decode byte-identically, and every frame this encoder emits is accepted and correctly decoded by `zstd -d`; enforced by embedded reference-frame fixtures (always on) plus a live CLI differential suite (`zstd-oracle` feature)
- **Full RFC 8878 entropy decoding** - FSE-compressed sequence tables, 1- and 4-stream Huffman literals, repeat offsets, treeless literals, repeat table modes
- **Huffman literals on the encode path** - literal sections are Huffman-compressed when that wins (self-verified with Raw/RLE fallback)
- **Custom block-optimal FSE sequence tables** - `FSE_Compressed_Mode` is emitted when it beats RLE and the predefined tables on total bit cost (reference-faithful `FSE_normalizeCount` / `FSE_writeNCount` ports)
- **Parallel compression** - Multi-threaded block compression with Rayon (`parallel` feature)
- **Dictionary support** - Raw-content dictionaries, interoperable with `zstd -D` in both directions. Formatted dictionaries (RFC 8878 §5, `Magic_Number` `0xEC30A437`, what `zstd --train` writes) are **rejected with a named error** rather than mistaken for content, and a frame that names a `Dictionary_ID` is refused by `ZstdStream` when no dictionary is supplied — see [Dictionary Compression](#dictionary-compression)
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

### What every decoding path refuses

The one-shot decoders and `ZstdStream` enforce the same **format** rules, from
shared code, so neither can drift from the other:

| input | `decompress` / `decompress_multi_frame` | `ZstdStream` |
|---|---|---|
| frame naming a `Dictionary_ID`, no dictionary supplied | error | error |
| block regenerating more than `min(Window_Size, 128 KiB)` (further bounded by a declared `Frame_Content_Size`) | error | error |
| garbage *before* any frame (short magic, unknown magic, truncated skippable frame) | error | error |
| a *complete* skippable frame in front of a Zstandard frame | metadata, walked past | metadata, walked past |
| bytes that start no frame *after* at least one complete frame | tolerated, stream ends | tolerated, stream ends |
| empty input (multi-frame) | empty output | empty output |
| declared `Window_Size` of 11 MB | accepted | refused above `with_max_window` (8 MiB default) |

The block ceiling is the RFC's, not the reference decoder's: RFC 8878
§3.1.1.2.3 caps a block at `min(Window_Size, 128 KiB)`, while `zstd -d` lets a
frame past that cap when it also declares a larger `Frame_Content_Size`. No
encoder emits such a frame — a `--zstd=wlog=10` frame declares a 1 KiB window
next to a content size hundreds of times larger and still keeps its blocks
within 1 KiB — so this crate keeps the RFC rule and is, in that one corner,
stricter than `zstd -d`. Both are pinned by
`tests/legacy_verify.rs::oracle`.

The last row is the one deliberate split, and it is about memory, not
conformance: `ZstdStream` keeps a real window ring, so an over-large
declaration is refused before it is allocated; the one-shot decoders keep no
ring at all — their output `Vec` *is* the window — so a declaration costs them
nothing. The bounded helpers `decompress_with_limit` /
`decompress_multi_frame_with_limit` bound the *output*, and refuse a
declaration only above the reference decoder's own 128 MiB ceiling (raised
further by a larger limit). Tying that ceiling to the output limit is not
possible: a payload piped through `zstd -3` declares a 2 MiB window whatever
its length, and `zstd --long=24 -6` declares 16 MiB, neither carrying a
`Frame_Content_Size` — so `Window_Size > limit` would reject ordinary
reference frames. Use `with_max_window` when the declaration itself is what
you need to bound.

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

**Raw content dictionaries only.** RFC 8878 §5 defines two dictionary shapes:
a *raw content* dictionary, whose bytes are used directly as the LZ77 history
prefix, and a *formatted* dictionary, which starts with `Magic_Number`
`0xEC30A437` and additionally carries a `Dictionary_ID`, one Huffman literals
table, three FSE tables and three initial repeat offsets. This crate implements
the raw content shape — `train_dictionary` produces one, and `zstd -D` accepts
and produces frames against one in both directions.

A formatted dictionary (what `zstd --train` writes) is **refused by name**
(`Unsupported compression method: formatted Zstandard dictionary …`) by
`ZstdStream::with_dictionary`, `ZstdStreamDecoder::with_dictionary`, the async
adapters, `decompress_with_dict` and `decompress_multi_frame_with_dict`. It is
deliberately not treated as content: frames built against such a dictionary
reference its *entropy tables* through `Repeat_Mode`, so seeding the window
with the dictionary's header and tables would hand back silently wrong bytes
for exactly the frames that need it. Convert with
`zstd --train ... --dictID=0` plus a raw extraction, or train with
[`train_dictionary`], which emits raw content.

Correspondingly, `ZstdStream` (and everything built on it) refuses a frame
whose header names a non-zero `Dictionary_ID` when no dictionary is configured,
instead of decoding it to wrong bytes; the reference decoder refuses too.

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
oxiarc-zstd = "0.4.3"

# With parallel compression
oxiarc-zstd = { version = "0.4.3", features = ["parallel"] }
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

### Decode throughput vs the reference decoder

Measured by `examples/decode_throughput.rs`: the same `.zst` frames decoded
three ways here and by `zstd -b -d` (libzstd's own in-memory benchmark), one
round of each arm in turn so machine load moves them together. Run it with

```text
cargo run --release -p oxiarc-zstd --example decode_throughput
```

| shape (level) | `decompress_into` | `decompress` | `ZstdStream` 64 KiB | `zstd -b -d` | best ratio |
|---|---|---|---|---|---|
| RGB8 TIFF strip 288 KiB (1) | 926 | 947 | 913 | 1316 | **0.72x** |
| RGB8 TIFF strip 288 KiB (3) | 921 | 969 | 925 | 1632 | 0.59x |
| RGB8 TIFF strip 288 KiB (9) | 975 | 1047 | 964 | 1706 | 0.61x |
| RGB8 TIFF strip 288 KiB (19) | 193 | 213 | 196 | 359 | 0.59x |
| RGB8 TIFF strip 1 MiB (1) | 945 | 957 | 926 | 1259 | **0.76x** |
| RGB8 TIFF strip 1 MiB (3) | 561 | 607 | 560 | 948 | 0.64x |
| RGB8 TIFF strip 1 MiB (9) | 624 | 684 | 621 | 1079 | 0.63x |
| RGB8 TIFF strip 1 MiB (19) | 171 | 186 | 171 | 278 | 0.67x |
| text corpus 50 MB (3) | 702 | 622 | 698 | 1531 | 0.46x |
| text corpus 50 MB (19) | 1236 | 1122 | 1235 | 2368 | 0.52x |
| incompressible 8 MiB (3) | 9507 | 6900 | 8190 | 11815 | **0.80x** |
| highly repetitive 8 MiB (3) | 10220 | 6899 | 9082 | 2846 | **3.59x** |

MB/s of *output* (1 MB = 1e6 B), Apple Silicon, `zstd` 1.5.7, best of two
7-round interleaved runs taken back to back inside one window on a machine
shared with other builds at load averages 27-37 on 8 cores. The reference
column is the *higher* of the two readings taken in that same window, which
favours libzstd. "best ratio" is the fastest of our three arms over it.

**Read the ratios, not the absolute numbers**, and read the *best-of* column on
a shared machine: contention can only slow a round down, so the fastest round is
the least contaminated. The example prints medians and best-of side by side, plus
the load average, precisely so a reader can tell the two apart.

**How stable is this?** Across three full runs of the matrix at load averages
between 14 and 64, every row above held at or over 0.5x except the 50 MB text
corpus at level 3, which ranged 0.43x-0.58x — that shape is the most
memory-bound of the set and its *reference* reading alone varied by a factor of
1.5 between runs. It is the one row this crate does not claim clears 0.5x.

**Re-confirmed 2026-09-08.** The verifier that first pinned this row asked for a
quieter re-measurement of the two `text50m` rows specifically. Two further
**15-round** interleaved runs (load average 18→84 and 80→21 on 8 cores)
reproduce the straddle exactly rather than resolving it: level 3 lands at 0.54x
and 0.43x, level 19 at 0.53x and 0.50x. The rest of the matrix held — the TIFF
strip shapes at 0.53x-0.81x, incompressible at 0.80x-0.90x and highly
repetitive at 3.8x. **No figure in the table above needed changing**; this
machine simply never got quiet enough during that window to do better than
confirm the published range.

Where the remaining gap is: on literal-dense frames (a photographic TIFF strip
compresses to ~1.1x, so nearly every output byte is a Huffman-coded literal) the
decode is one four-way-interleaved symbol loop and little else, and libzstd's
double-symbol (`HUF_DECOMPRESS_X2`) tables decode two literals per lookup where
this crate decodes one. On `Raw`-block and highly repetitive data — where the
work is copying, not decoding — this decoder is at or ahead of the reference.

### Incremental decode vs one-shot

The same interleaved measurement, read as `ZstdStream` (64 KiB chunks) over the
legacy one-shot `decompress`. The audit gate is *incremental >= 85 % of
one-shot*:

| shape (level) | one-shot | `ZstdStream` | vs one-shot |
|---|---|---|---|
| RGB8 TIFF strip 288 KiB (3) | 969 | 925 | 0.95x |
| RGB8 TIFF strip 1 MiB (9) | 684 | 621 | 0.91x |
| text corpus 50 MB (3) | 622 | 698 | **1.12x** |
| text corpus 50 MB (19) | 1122 | 1235 | **1.10x** |
| incompressible 8 MiB (3) | 6900 | 8190 | **1.19x** |
| highly repetitive 8 MiB (3) | 6899 | 9082 | **1.32x** |

Every shape clears the gate. Where the push decoder is *slower* the gap is the
one extra copy that bounded memory costs: the one-shot decoder writes each byte
into a growing `Vec` and hands the `Vec` over, while a windowed decoder writes
it into the ring and then copies it out to the caller. Where it is faster, the
one-shot path is paying for the owned `Vec` it returns — a fresh allocation and
its first-touch page faults on every call, which `decompress_into` avoids
entirely (see the table above, where `decompress_into` is 38 % ahead of
`decompress` on incompressible data).

Steady-state allocations after warm-up are **zero** over `Raw`/`RLE` blocks, and
for compressed blocks the allocation count is a function of the frame (a few
entropy tables per block) and provably independent of how the caller chunks the
input — pinned by `tests/alloc_budget.rs` with a counting global allocator,
which also pins that no attacker-controlled length field (a literals section's
20-bit `Regenerated_Size`, a sequences section's `Number_of_Sequences`) can size
a buffer beyond what one block can hold.

`tests/mutation_differential.rs` closes the gap between "agrees with the
one-shot decoder on valid frames" and "does not panic on invalid ones": it
mutates real frames byte by byte and requires that **whenever the one-shot
decoder still succeeds, the incremental decoder produces exactly the same
bytes**, under chunk schedules down to one byte in and one byte out.

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
