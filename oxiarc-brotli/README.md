
# oxiarc-brotli [Stable]

Pure Rust Brotli compression/decompression implementation (RFC 7932), part of the OxiArc ecosystem.

[![Crates.io](https://img.shields.io/crates/v/oxiarc-brotli.svg)](https://crates.io/crates/oxiarc-brotli)
![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)
![Status](https://img.shields.io/badge/status-Stable-brightgreen)

**Version: 0.4.2 (2026-09-08) | 343 tests + 21 doctests passing | Reference-interop verified (both directions)**

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
- **Shared (custom LZ77) dictionaries** — both directions, and interoperable
  with the reference `brotli --dictionary=FILE` in both: content both peers
  already hold is preloaded as the LZ77 history, so a stream's backward
  distances may reach past everything it has itself produced and past its own
  declared window. `compress_with_dictionary` / `decompress_with_dictionary`,
  `BrotliStream::with_dictionary`, and the same on the `Read` and async
  adapters.
- **`Content-Encoding: dcb` framing (RFC 9842)** — the `dcb` module parses and
  writes the 4-byte magic plus the 32-byte SHA-256 dictionary id that precedes
  a dictionary-compressed Brotli body, and verifies the id against the
  dictionary the caller holds.

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

### Shared dictionaries and `Content-Encoding: dcb`

A *shared* dictionary is content both ends already have — last week's copy of a
page, a common JSON schema, a JavaScript bundle. It is preloaded as the LZ77
history, so the encoder can reference it and the body shrinks to the delta:

```rust
use oxiarc_brotli::{compress_with_dictionary, decompress_with_dictionary, BrotliParams};

let dictionary = std::fs::read("previous-version.html")?;
let page = std::fs::read("current-version.html")?;

let params = BrotliParams { quality: 9, ..BrotliParams::default() };
let body = compress_with_dictionary(&page, &dictionary, &params)?;
assert_eq!(decompress_with_dictionary(&body, &dictionary)?, page);
```

The same dictionary can be attached to the push decoder, so an HTTP body that
arrives in pieces is decoded against it without buffering:

```rust
use oxiarc_brotli::BrotliStream;

let dictionary: Vec<u8> = /* ... */;
let mut stream = BrotliStream::new()
    .with_dictionary(dictionary)      // survives `reset()`
    .with_max_output(64 << 20);
```

RFC 9842 (Compression Dictionary Transport) wraps such a stream in a 36-byte
preamble — `FF 44 43 42` and the SHA-256 of the dictionary — and calls the
result `Content-Encoding: dcb`. The `dcb` module is that framing:

```rust
use oxiarc_brotli::{dcb, BrotliParams};

let dictionary = b"<nav class=\"site\"><a href=\"/\">home</a>".repeat(32);
let page = b"<nav class=\"site\"><a href=\"/\">home</a><main>hi</main>";

let body = dcb::compress(page, &dictionary, &BrotliParams::default())?;
assert_eq!(&body[..4], &dcb::DCB_MAGIC);
assert_eq!(&body[4..36], &dcb::dictionary_id(&dictionary));   // Available-Dictionary
assert_eq!(dcb::decompress(&body, &dictionary)?, page);

// Or strip the header yourself and stream the rest:
let stream_bytes = dcb::verify_header(&body, &dictionary)?;
```

Both directions are checked against the reference CLI: every
`brotli -D dict` stream decodes byte-identically here, and every stream this
encoder produces with a dictionary is accepted by `brotli -d -D dict`
(`--features brotli-oracle`; the tests self-skip when the binary is absent or
too old for `--dictionary`).

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
| `compress_with_dictionary(data, dict, params)` | function | Compress against a shared (custom LZ77) dictionary |
| `decompress_with_dictionary(data, dict)` | function | Decompress a stream whose distances reach into `dict` |
| `decompress_with_dictionary_and_limit(data, dict, n)` | function | The same, with an output budget |
| `BrotliStream::with_dictionary(dict)` | method | Attach a shared dictionary to the push decoder (survives `reset()`) |
| `BrotliStream::dictionary()` | method | The attached dictionary, empty when none |
| `BrotliDecompressor::with_dictionary(dict)` | method | The same on the `Read` adapter |
| `BrotliAsyncDecompressor::with_dictionary(dict)` | method | The same on the async adapter (`async-io`) |
| `shared_dict::MAX_SHARED_DICTIONARY` | const | 16 MiB — largest dictionary any entry point accepts |
| `dcb::DCB_MAGIC` / `dcb::DCB_HEADER_LEN` | const | `FF 44 43 42`; 36 bytes of preamble |
| `dcb::dictionary_id(dict)` | function | The 32-byte SHA-256 that names a dictionary (RFC 9842) |
| `dcb::write_header(dict)` / `dcb::parse_header(body)` | function | Build / split the `dcb` preamble |
| `dcb::verify_header(body, dict)` | function | Check the id and return the Brotli stream that follows |
| `dcb::compress` / `dcb::decompress` / `dcb::decompress_with_limit` | function | Whole-body `dcb` round trip |
| `parse_dcb_header` / `verify_dcb_header` / `write_dcb_header` | function | The same three at the crate root, under names that say what they frame |
| `compress_dcb` / `decompress_dcb` / `decompress_dcb_with_limit` | function | The whole-body round trip at the crate root (`compress`/`decompress` there stay plain Brotli) |
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

Decode throughput, interleaved A/B, Apple Silicon, release build
(`cargo run --release --example decode_profile`). "one-shot" is `decompress`
over the complete slice; the streaming column is the API's own shape — a caller
that owns a fixed 64 KiB buffer and consumes each chunk, which is what an HTTP
body reader does. Every figure below is the **median of five runs of 25
interleaved repetitions**, taken on a machine carrying other work (load average
~35); the harness reports the median of the *paired* per-repetition ratios,
which is what stays put when the machine does not.

| Payload (q5) | window | one-shot | `BrotliStream`, 64 KiB buffer | ratio |
|---|---|---|---|---|
| 1.08 MB repetitive text | lgwin 22 | 606 µs | 62 µs | **9.5×** |
| 1.08 MB repetitive text | lgwin 10 | 609 µs | 23 µs | **24.9×** |
| 1.05 MB single repeated byte | lgwin 22 | 517 µs | 35 µs | **19.7×** |
| 1.05 MB single repeated byte | lgwin 10 | 524 µs | 18 µs | **29.6×** |
| 2.94 MB hex dump, literal-dominated stream | lgwin 10 | 14.29 ms | 12.74 ms | **1.21×** |
| 2.94 MB hex dump, copy-dense stream | lgwin 22 | 15.55 ms | 20.92 ms | 0.76× |
| 1.05 MB incompressible (stored meta-blocks) | lgwin 10 | 14.1 µs | 14.3 µs | 1.01× (floor 1.01×) |
| 1.05 MB incompressible (stored meta-blocks) | lgwin 22 | 13.7 µs | 41.0 µs | 0.33× (floor 0.34×) |

**Where the push decoder wins, it wins by a lot.** It resolves matches with bulk
runs and tiles short-distance (periodic) matches, whereas the one-shot decoder
appends backward references one byte at a time; repetitive content — which is
most real web content — is 9–28× faster, and the advantage grows as the declared
window shrinks.

**The window is not the cost, and that is measured.** An earlier edition of this
section blamed the second write per byte: a bounded decoder puts every byte in
the caller's buffer *and* in its window, where the one-shot decoder's output
`Vec` *is* its window. That story is wrong for everything except stored blocks.
With the ring mirror compiled out entirely (an incorrect build, for timing only)
the copy-dense row moved from 21.9 ms to 22.1 ms — nothing. The command loop
resolves a match's *source* across the boundary between the ring and the bytes
already produced into the caller's slice, and mirrors the whole slice into the
ring with one bulk copy per `decode` call; that copy is streaming memory traffic
the machine absorbs behind the decode work.

**The stored shape is at a measured floor.** Uncompressed meta-blocks are the
one case where the second write really is the whole job, so the harness prints a
`copy floor` row: the same one-shot decode in one column, and in the other a
model that does exactly the irreducible work of a bounded push decoder — copy
every byte into the caller's fixed buffer, and copy the part still reachable
afterwards (the last `1 << WBITS` bytes) into a freshly allocated ring. It is
timed against the same one-shot decode as every row above, so its ratio and the
decoder's are directly comparable. The decoder sits **on** that floor: 0.33×
against a 0.34× floor at lgwin 22, and 1.01× against a 1.01× floor at lgwin 10
— 97-100 % of the achievable figure. A bounded push decoder cannot do better
without holding the whole body in memory, which is the thing it exists not to
do.

**What is still open** is the copy-dense row, and its shape is worth naming
precisely, because the payload's name is misleading. The 2.94 MB hex dump is
literal-dominated only at small windows: at lgwin 10 its stream carries 95,605
copy commands (73 % of the output is literals) and decodes *faster* than the
one-shot decoder, at 1.21×. At lgwin 22 the encoder finds a match nearly
everywhere, and the same payload becomes
**570,440 copy commands of a mean 5.0 bytes at a mean distance of 116,525** —
97 % of the output — of which 65 % must read their source out of the ring,
because the distance reaches back past everything produced in the current call.
(That census was taken on 2026-09-08 with temporary in-tree counters in
`copy_into_pending`, and the function shares below with `sample`, run against
each decoder alone via `PROFILE_PUSH_ONLY` / `PROFILE_ONE_SHOT_ONLY`; neither
instrument is in the shipped code, so `decode_profile` alone will not reproduce
them.) Profiling shows the Huffman work is *identical* in the two decoders
(`decode_symbol` 29 vs 30.6 units of a normalised 100, `decode_distance` 19.1 vs
19.6); the entire difference is the resumable command machinery around it,
spread thinly over half a million 5-byte commands rather than concentrated
anywhere a single fix would reach. Removing the per-copy
`memmove` call (7.9 % of the profile) was tried and reverted: three loop shapes
were measured, LLVM rewrites two of them back into the same call, and the third
— a wrapping-index byte loop that really does remove it — costs exactly what the
call cost.

Run `cargo run --release --example decode_profile` to reproduce the whole table
(`PROFILE_ONLY`, `PROFILE_LGWIN`, `PROFILE_REPS`, and `PROFILE_PUSH_ONLY` /
`PROFILE_ONE_SHOT_ONLY` for profiling one decoder at a time), and
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
