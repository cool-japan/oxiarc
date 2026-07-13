
# oxiarc-brotli [Stable]

Pure Rust Brotli compression/decompression implementation (RFC 7932), part of the OxiArc ecosystem.

[![Crates.io](https://img.shields.io/crates/v/oxiarc-brotli.svg)](https://crates.io/crates/oxiarc-brotli)
![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)
![Status](https://img.shields.io/badge/status-Stable-brightgreen)

**Version: 0.4.0 (2026-07-13) | 219 tests passing | Reference-interop verified (both directions)**

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
- **Streaming API** — `BrotliCompressor<W: Write>` and `BrotliDecompressor<R: Read>`
  adapters (fully buffered in memory; see the `streaming` module docs)
- **One-shot API** — Convenient `compress` / `decompress` functions
- **Configurable window** — `lgwin` 10–24; window size is `(1 << lgwin) - 16` bytes (RFC 9.1)

## Quick Start

Add to your `Cargo.toml`:

```toml
[dependencies]
oxiarc-brotli = "0.4.0"
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
| `BrotliParams::window_size()` | method | Returns window size in bytes: `(1 << lgwin) - 16` (RFC 7932 §9.1) |
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
| `async-io` | no | `BrotliAsyncCompressor`/`BrotliAsyncDecompressor` (`oxiarc_core::async_io` traits) for `tokio`-based async I/O; reads the input fully before compressing/decompressing synchronously (not bounded-memory streaming) |
| `brotli-oracle` | no | Differential oracle tests against the reference `brotli` CLI in both directions (tests self-skip when the binary is absent) |

All other functionality — one-shot API, streaming API, Huffman coding, LZ77 engine, static dictionary — is enabled by default with no feature flags required.

```toml
[dependencies]
# Default (no optional features)
oxiarc-brotli = "0.4.0"

# With parallel compression support
oxiarc-brotli = { version = "0.4.0", features = ["parallel"] }
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
