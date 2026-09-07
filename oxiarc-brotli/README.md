
# oxiarc-brotli [Stable]

Pure Rust Brotli compression/decompression implementation (RFC 7932), part of the OxiArc ecosystem.

[![Crates.io](https://img.shields.io/crates/v/oxiarc-brotli.svg)](https://crates.io/crates/oxiarc-brotli)
![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)
![Status](https://img.shields.io/badge/status-Stable-brightgreen)

**Version: 0.4.2 (2026-08-06) | 219 tests passing | Reference-interop verified (both directions)**

## Features

- **Pure Rust** — No C dependencies or unsafe FFI
- **Reference-interoperable decoder** — full RFC 7932 decoding: simple/complex
  prefix codes with two-level `O(1)` decode tables, block-type switching,
  literal/distance context maps with the exact Section 7.1 context tables,
  metadata meta-blocks, the complete distance code space, and the byte-exact
  122,784-byte Appendix A static dictionary with all 121 word transforms
  (UTF-8-aware ferment casing included). Differentially validated against the
  reference `brotli` CLI across qualities 0–11 and windows 10–24 —
  byte-identical, with zero tolerance for silent mismatches.
- **Reference-accepted encoder** — RFC-conformant output decodable by
  `brotli -d`, verified across the same grid. Uses one prefix code per
  category per meta-block plus implicit distance-code-0 reuse; incompressible
  data falls back to stored (uncompressed) meta-blocks.
- **Strict decoder** — truncated streams, trailing garbage, non-zero padding,
  incomplete prefix codes, and length overruns are rejected; the decoder
  never returns `Ok` with wrong bytes.
- **Quality levels 0–11** — Quality 0 stores; higher levels increase LZ77 search effort
- **Bounded incremental decoding** — `BrotliStream` is a push decoder: feed it
  whatever compressed bytes and output space you have and it makes as much
  progress as both allow. Peak memory is the stream's declared sliding window,
  not the body size; the output cap is exact (checked per meta-block, before
  the meta-block is decoded) and an over-large declared window is refused
  before it is allocated.
- **Streaming API** — `BrotliCompressor<W: Write>` (incremental) and
  `BrotliDecompressor<R: Read>` / `BrotliAsyncDecompressor` (incremental,
  built on `BrotliStream`)
- **One-shot API** — Convenient `compress` / `decompress` functions
- **Configurable window** — `lgwin` 10–24; window size is `(1 << lgwin) - 16` bytes (RFC 9.1)

## Quick Start

Add to your `Cargo.toml`:

```toml
[dependencies]
oxiarc-brotli = "0.4.2"
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
// Decodes as the source delivers: 64 KiB of compressed data is staged at a
// time and decoded straight into `output`, so nothing waits for EOF.
let mut decompressor = BrotliDecompressor::new(&compressed[..])
    .with_max_output(64 << 20)   // refuse a bomb before it expands
    .with_max_window(4 << 20);   // refuse an over-large declared window
let mut output = Vec::new();
decompressor.read_to_end(&mut output)?;
```

### Incremental decoding (`BrotliStream`)

For an HTTP body, a pipe, or anything else that arrives in pieces — and for
callers that own their own output buffer:

```rust
use oxiarc_brotli::{BrotliStatus, BrotliStream};
use oxiarc_core::traits::FlushMode;

let compressed: Vec<u8> = /* ... */;
let mut stream = BrotliStream::new()
    .with_max_output(64 << 20)
    .with_max_window(4 << 20);

let mut decoded = Vec::new();
let mut out = [0u8; 8192];
let mut fed = 0;
loop {
    let end = (fed + 1400).min(compressed.len());          // one TCP segment
    let flush = if end == compressed.len() { FlushMode::Finish } else { FlushMode::None };
    let progress = stream.decode(&compressed[fed..end], &mut out, flush)?;
    fed += progress.consumed;
    decoded.extend_from_slice(&out[..progress.produced]);
    if progress.status == BrotliStatus::StreamEnd { break; }
}
stream.finish()?;   // a truncated body fails here, never silently succeeds
```

`decode` reports which side to grow: `NeedInput` wants more compressed bytes,
`NeedOutput` wants more room. Chunking is not observable — one byte at a time
into a one-byte slice yields exactly the bytes one call with everything would.

#### Memory and the window

`BrotliStream` keeps a real LZ77 ring, allocated lazily and grown on demand up
to the stream's declared `1 << WBITS`. `with_max_window` (default 16 MiB, which
admits every RFC 7932 window) refuses a larger declaration *while reading the
stream header*, before anything is allocated — `Content-Encoding: br` in
practice uses `lgwin <= 22` (4 MiB).

`with_max_output` bounds the total decoded size. Because every meta-block
declares its exact `MLEN`, the check is an exact projection made *before* the
offending meta-block is decoded: a bomb is refused with none of its expansion
produced, and without the rest of the body being read.

## API Overview

| Item | Kind | Description |
|------|------|-------------|
| `compress(data, quality)` | function | One-shot compression; quality 0–11 |
| `compress_with_params(data, params)` | function | One-shot compression with full `BrotliParams` control |
| `decompress(data)` | function | One-shot decompression |
| `BrotliParams` | struct | Compression parameters: `quality`, `lgwin`, `lgblock` |
| `BrotliParams::default()` | method | quality=6, lgwin=22, lgblock=0 |
| `BrotliParams::validate()` | method | Checks that all parameters are in range |
| `BrotliParams::window_size()` | method | Returns window size in bytes: `(1 << lgwin) - 16` (RFC 7932 §9.1) |
| `BrotliCompressor<W>` | struct | Streaming compressor implementing `Write` |
| `BrotliCompressor::new(writer, params)` | method | Create a new streaming compressor |
| `BrotliCompressor::finish()` | method | Flush and finalise the compressed stream |
| `BrotliDecompressor<R>` | struct | Incremental decompressor implementing `Read`, built on `BrotliStream` |
| `BrotliDecompressor::new(reader)` | method | Create a new streaming decompressor |
| `BrotliDecompressor::with_max_output(n)` | method | Cap total output; enforced before the offending meta-block decodes |
| `BrotliDecompressor::with_max_window(n)` | method | Refuse a declared window larger than `n`, before allocating |
| `BrotliStream` | struct | Bounded push decoder: `decode`/`finish`/`reset` |
| `BrotliStream::decode(input, output, flush)` | method | Make progress from the given input and output space |
| `BrotliStream::finish()` | method | Assert the stream really ended (truncation is an error here) |
| `BrotliStream::reset()` | method | Return to the initial state and clear the fault latch |
| `BrotliStream::with_max_output(n)` | method | Exact per-meta-block output cap |
| `BrotliStream::with_max_window(n)` | method | Declared-window ceiling, checked before allocation |
| `BrotliProgress` | struct | `{ consumed, produced, status }` returned by `decode` |
| `BrotliStatus` | enum | `NeedInput` / `NeedOutput` / `StreamEnd` |
| `DEFAULT_MAX_WINDOW` | const | 16 MiB — the default `with_max_window` ceiling |
| `BrotliError` | enum | Error type for all Brotli operations |
| `BrotliResult<T>` | type alias | `Result<T, BrotliError>` |

## Feature Flags

| Feature | Default | Description |
|---------|---------|-------------|
| `parallel` | no | Rayon-based parallel compression for throughput-sensitive workloads |
| `async-io` | no | `BrotliAsyncCompressor`/`BrotliAsyncDecompressor` (`oxiarc_core::async_io` traits) for `tokio`-based async I/O. The **decompressor is bounded**: it drives `BrotliStream` with a small compressed staging buffer and writes each decoded chunk as it is produced. The compressor still reads its input fully before compressing. |
| `brotli-oracle` | no | Differential oracle tests against the reference `brotli` CLI in both directions (tests self-skip when the binary is absent) |

All other functionality — one-shot API, `BrotliStream`, the `Read`/`Write`
adapters, Huffman coding, LZ77 engine and the static dictionary — is enabled by
default with no feature flags required.

```toml
[dependencies]
# Default (no optional features)
oxiarc-brotli = "0.4.2"

# With parallel compression support
oxiarc-brotli = { version = "0.4.2", features = ["parallel"] }
```

## Algorithm

Brotli (RFC 7932) combines three techniques:

1. **LZ77** — Backward-reference matching over a sliding window (`lgwin` 10–24, configurable). Backward references and literal sequences are encoded as insert-and-copy commands.
2. **Context-dependent Huffman coding** — Up to 256 prefix code trees selected per block type; context modelling uses the previous two bytes to pick the tree, improving compression for structured data (HTML, CSS, JS).
3. **Static dictionary** — A 122,784-byte table of common words in 21 lengths, each usable through 121 transforms (RFC 7932 Appendix A/B), providing matches that never appear in the stream itself. The decoder resolves these; the encoder does not emit them yet.

Quality levels map to LZ77 search depth (quality 0 emits stored
meta-blocks). Compression-ratio expectations, honestly stated: this encoder
emits one prefix code per category per meta-block and does not yet perform
block splitting, context modeling, or dictionary-reference emission, so its
output is larger than the reference encoder's at the same quality — close
(within a few percent) on typical text at q5–9, further behind at q10–11 and
on structured binary data. The decoder, in contrast, handles everything the
reference encoder produces.

## Performance

Decode throughput, best of 12 runs per configuration, Apple Silicon, release
build (`cargo run --release --example decode_profile`). "one-shot" is
`decompress` over the complete slice. Two streaming columns are reported
because they do different amounts of *output-side* work: `decompress` allocates
and grows a `Vec` for the whole body, so the **Vec sink** column is the
apples-to-apples comparison, while the **64 KiB buffer** column is the API's own
shape — a caller that owns a fixed buffer and consumes each chunk, which is what
an HTTP body reader does.

| Payload (lgwin 22, q5) | one-shot | Vec sink | 64 KiB buffer |
|---|---|---|---|
| 1.08 MB repetitive text | 616 µs | 112 µs (**5.5×**) | 94 µs (**6.6×**) |
| 1.05 MB single repeated byte | 565 µs | 137 µs (**4.1×**) | 107 µs (**5.3×**) |
| 2.94 MB hex-dump text (literal-dense) | 16.1 ms | 25.4 ms (0.63×) | 25.4 ms (0.63×) |
| 1.05 MB incompressible (stored meta-blocks) | 14.4 µs | 125 µs (0.12×) | 97 µs (0.15×) |

Two things are worth reading off that table.

**Where the push decoder wins**, it wins by a lot: it resolves matches with
bulk `copy_within` runs and tiles short-distance (periodic) matches, whereas
the one-shot decoder appends backward references one byte at a time. Repetitive
content — which is most real web content — is 5–6× faster.

**Where it loses, it loses to the same trade that makes it bounded.** The
one-shot decoder uses its output `Vec` *as* the sliding window, so it touches
each byte once and keeps the whole body resident. `BrotliStream` maintains a
real ring and hands the caller a copy, so it touches each byte twice and pays
the memory traffic of the declared window. Re-running the same two payloads at
`lgwin = 10` — a 1 KiB, cache-resident ring — isolates that cost exactly:

| Payload (64 KiB buffer) | lgwin 22 (4 MiB ring) | lgwin 10 (1 KiB ring) |
|---|---|---|
| hex-dump text (literal-dense) | 0.63× | 0.80× |
| incompressible (stored meta-blocks) | 0.15× | **0.36×** |

Shrinking the ring recovers a large part of the gap in both rows, which
localises the cost to the window's memory traffic rather than to the decode
loop. The stored-meta-block row is the effect at its extreme: it is essentially
a memcpy benchmark (12 GB/s in absolute terms) in which the one-shot decoder
performs one copy — its output `Vec` *is* the window — and the bounded decoder
performs two, into the ring and out to the caller. That second copy is not a
defect to be optimised away; it is the price of not holding the whole body in
memory. Run `cargo run --release --example decode_profile` to reproduce the
whole table, including the window sweep, and
`cargo bench --bench brotli_bench -- brotli_decode_window` for the criterion
version.

Peak memory is the point of the exercise, and it is asserted in
`tests/memory_limit.rs`: decoding a 64 MiB body through a 64 KiB output slice
allocates under 12 MiB, and streaming 8 MiB through the `Read` adapter with a
fixed 32 KiB buffer allocates under 12 MiB. The previous implementation
allocated the entire compressed input *and* the entire decompressed output
before serving the first byte.

## What's new in 0.4.2

- **`BrotliStream` — bounded, truly incremental decoding.** A push decoder that
  makes progress from whatever input and output space it is given, with peak
  memory proportional to the declared sliding window rather than to the stream.
  Meta-block preludes are parsed atomically with bit-cursor rollback (bounded by
  a 1 MiB header cap); the command loop is resumable at every literal, every
  byte of a backward reference and every byte of a transformed dictionary word.
  Chunking is not observable: one byte in and one byte out yields exactly the
  bytes one call with everything would.
- **`with_max_window`** (default 16 MiB) refuses an over-large declared window
  while reading the stream header, before the ring is allocated. New
  `BrotliError::WindowTooLarge`.
- **`with_max_output`** on `BrotliStream` — the exact per-meta-block projection
  the one-shot `decompress_with_limit` already used, now available to streaming
  callers, and enforced before the body is downloaded.
- **`BrotliDecompressor<R>` and `BrotliAsyncDecompressor` are re-based on
  `BrotliStream`.** Both now produce output before the source reaches EOF.
  Every public item is preserved. `Interrupted` is retried, `WouldBlock`
  propagates with the decoder state intact, and a source that stops mid-stream
  is an error rather than a short read. A source that is empty from its very
  first read still yields an empty body without an error, as before.
- **`BrotliStream::with_shape_recording`** exposes the decoder's per-meta-block
  `MetaBlockShape` sequence, used as a differential oracle against
  `decompress_reporting_shapes`: identical output bytes do not prove a resumable
  header parser read the right fields at the right bit offsets, but an identical
  shape sequence does. The `brotli-oracle` suite runs this against real
  reference-`brotli` streams.
- `BrotliError::Cancelled` now converts to `io::ErrorKind::Other` rather than
  `InvalidData`, matching what the streaming adapters have always surfaced.

## What's new in 0.3.6

- **Full RFC 7932 conformance overhaul — real interoperability with reference brotli.**
  The previous decoder/encoder pair was a self-consistent private dialect that
  round-tripped with itself but failed against the reference implementation.
  Both sides were rewritten against the RFC:
  - *Decoder*: full WBITS tree (10–24), Section 5 insert-and-copy command
    alphabet (704 symbols, implicit distance-code-0), Section 3.5 complex
    prefix codes (exact code-length VLC, Kraft-complete stop, repeat
    accumulation), block-type switching (NBLTYPES ≥ 2), context maps with the
    exact Section 7.1 UTF8/Signed lookup tables, Section 4 distance short
    codes and ring semantics with proper window bounds, metadata meta-blocks,
    the byte-exact Appendix A static dictionary (122,784 bytes, CRC-verified)
    with all 121 transforms, two-level Huffman decode tables (O(1) per symbol;
    fixes a CPU-DoS in the old per-symbol rebuild path), and strict
    trailing-garbage/padding/truncation rejection.
  - *Encoder*: RFC-exact meta-block structure accepted by `brotli -d`
    (verified 426/426 across the quality × window grid), stored-meta-block
    fallback for incompressible data, and an end-to-end self-check that
    guarantees the emitted stream decodes back to the input.
  - *Validation*: embedded reference-produced vectors (always run) plus a
    `brotli-oracle` feature running full differential sweeps against the CLI
    (608/608 reference streams decode byte-identically; zero silent
    mismatches).
- **`BrotliError` is now `#[non_exhaustive]`** (pre-1.0 API-stability freeze); its `Display`/`Error`/`From<io::Error>` impls are now generated via `thiserror` instead of hand-written (messages are unchanged). Downstream `match` expressions on `BrotliError` must include a wildcard arm.
- **`BrotliParams` now derives `PartialEq`/`Eq`** for easier comparison in tests and application code.
- New `proptest`-based round-trip regression suite (`tests/proptest_roundtrip.rs`): decompression never panics on arbitrary input, and compress→decompress round-trips across quality levels.
- New `quality_levels` example comparing compression ratio across quality 0–11 plus a custom `BrotliParams` (window/block-size) configuration.
- Previously `ignore`-fenced doctests (including the `async-io` examples) now compile and run as part of `cargo test`.

## What's new in 0.3.3

**High-entropy / incompressible data now round-trips byte-for-byte across all quality levels (1–11).** Previously, near-uniform or incompressible inputs (random bytes, counters, all-distinct sequences) could fail to decode. Two underlying encoder bugs were fixed:

1. **Incomplete length-limited Huffman codes.** The old `compute_code_lengths` heuristic derived lengths from `ceil(-log2 p)` and patched them with a Kraft fix-up, which could emit an *incomplete* prefix code (Kraft sum below 2^15). The decoder then failed with "invalid Huffman code: no matching code found". This is replaced with the **package-merge algorithm** (Larmore–Hirschberg), which always produces a complete, length-optimal prefix code under the length limit.
2. **Insert lengths above 319 silently truncated.** A single incompressible meta-block is encoded as one insert-and-copy command spanning the whole block, but the encoder only had insert-length categories 0–15 (covering inserts up to 319 bytes) and wrapped the excess into 7 bits, corrupting the stream. The insert-length code table is now a **single source of truth shared by encoder and decoder**, with categories extended to cover inserts up to ~4 MiB.

A new `high_entropy_roundtrip.rs` regression suite exercises quality levels 1–11 over random 4 KiB / 64 KiB buffers, an incompressible counter, all-distinct and all-same-byte inputs, empty input, and mixed content — adding 13 new tests (150 → 163 passing).

## What's new in 0.3.1

19 interop integration tests covering:

- All quality levels 0–11 roundtrips
- Empty input roundtrip
- Single-byte roundtrip
- Binary data roundtrip
- Text data roundtrip
- Large-input roundtrip
- `compress_with_params` variations
- Minimum-window roundtrip (lgwin=16)
- Compression-is-beneficial assertion
- Invalid parameter rejection

## Part of OxiArc

This crate is part of the [OxiArc](https://github.com/cool-japan/oxiarc) project — a Pure Rust archive and compression library ecosystem.

## Documentation

Full API documentation: <https://docs.rs/oxiarc-brotli>

## License

Apache-2.0
