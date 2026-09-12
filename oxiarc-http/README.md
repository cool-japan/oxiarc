# oxiarc-http

HTTP content-coding (RFC 9110 `Content-Encoding` / `Accept-Encoding`: gzip, deflate, br, zstd, compress, plus RFC 9842 Compression Dictionary Transport's `dcb`/`dcz`) for OxiArc — Pure Rust, no `flate2`, no `http` crate dependency.

[![Crates.io](https://img.shields.io/crates/v/oxiarc-http.svg)](https://crates.io/crates/oxiarc-http)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)
![Status](https://img.shields.io/badge/status-Complete-brightgreen)

**Version: 0.4.2 (2026-09-12) | 310 tests passing (nextest, all features) + 17 doctests**

## Overview

`oxiarc-http` closes the last common route by which `flate2` (and its C
dependency chain) enters a `~/work` project: `ureq`'s default `gzip` feature.
It parses and renders `Content-Encoding`/`Accept-Encoding`, implements RFC
9110 §12.5.3 server-side negotiation exactly, bounds decompression-bomb
exposure, and encodes response bodies through the existing `oxiarc-deflate`
/ `oxiarc-brotli` / `oxiarc-zstd` codecs — all with a plain-`&str` API, no
dependency on the `http` crate, so it drops into `ureq`, `reqwest`,
`oxihttp`, or any hand-rolled client or server unmodified.

Both directions are complete: the client decodes response bodies
incrementally and bounded (`Decoder`, `DecodedBody`, `AsyncDecodedBody`,
`decode_body`), and the server negotiates and encodes them (`negotiate`,
`encode_body`, `Encoder`, `negotiate_and_encode`).

## Features

- **Pure Rust, no `http` crate** — every header value is `&str` in, owned
  data out; works from any HTTP client or server without pulling in a
  particular `http`-crate version.
- **`ContentCoding`** — `Identity`, `Compress`, `Deflate`, `Gzip`, `Brotli`,
  `Zstd`, `Dcb`, `Dcz`, `Unknown(String)`; case-insensitive parsing with the
  `x-gzip`/`x-compress` aliases; declared least-to-most-preferred so the
  derived `Ord` is a ready-made client-side preference order. `is_decodable`/
  `is_encodable` track *real* capability: `Compress` needs its own feature,
  `Dcb`/`Dcz` need their feature (`brotli`/`zstd`) **and** a per-response
  dictionary — see below.
- **RFC-exact header parsing** — `Content-Encoding` application order
  preserved (decode in reverse, RFC 9110 §8.4); `Accept-Encoding` q-values
  as exact `u16` thousandths (`QValue`), never `f32`/`NaN`; RFC 9110
  §5.6.1.2's empty-element tolerance, bounded against a hostile header.
- **`AcceptEncoding` builder** — `all_supported()` reflects exactly what
  this build can decode; `add`/`with_q` silently skip a coding this build
  cannot decode (the one mistake that turns a working client into one that
  receives an undecodable body); `to_header_value() -> Option<String>`
  makes RFC 9110's "empty value and absent header are opposites" trap hard
  to hit by accident.
- **`negotiate`** — RFC 9110 §12.5.3 implemented exactly, including the
  wildcard/identity interaction the RFC's own prose states ambiguously (see
  `negotiate`'s rustdoc) and the `Err(NotAcceptable)` → 415 case; ties break
  by the *caller's* `available` order, not `ContentCoding`'s own `Ord` —
  the actual bug in `oxihttp`'s pre-`oxiarc-http` negotiator, along with
  never honouring the client's q-value over the server's fixed order.
  Pinned against the RFC's full negotiation table as tests.
- **`encode_body` / `Encoder<W: Write>`** — one-shot and streaming
  server-side response encoding for gzip, deflate, brotli, zstd, compress
  and (with a shared dictionary) `dcz`/`dcb` (RFC 9842 Compression
  Dictionary Transport — each body opens with a preamble naming the
  dictionary's SHA-256, verified on the way back in, never silently
  decoded against the wrong one or falling back to plain `br`/`zstd`).
  `EncodeOptions` is built with `new()` +
  `with_level`/`with_brotli_quality`/`with_zstd_level`/
  `with_compress_max_bits`/`with_dictionary`. `Encoder`'s per-coding
  framing is documented rather than assumed uniform: a `flush()` stays
  inside one member for gzip/deflate/brotli, but **closes a Zstandard
  frame** for `zstd`/`dcz` — as does streaming past 128 KiB with no flush
  at all — so a streamed zstd body needs a multi-frame decoder
  (`oxiarc_zstd::decompress_multi_frame`), never the single-frame one,
  which stops after the first frame without erroring. `dcb` has no
  streaming `Encoder` at all (`oxiarc-brotli` has no dictionary-aware
  streaming encoder to wrap) — `encode_body`'s one-shot path is the only
  way to produce one; the rustdoc on that `Encoder::new` arm explains why.
- **`DecodeLimits`** — `max_output` (the load-bearing bomb control, default
  64 MiB), `max_ratio` (defense-in-depth, documented with the measurements
  behind the default), `max_codings`.
- **Bomb-safe by construction** — every limit is documented with the exact
  measured legitimate-vs-hostile ratios that justify its default, not a
  guessed number.
- **`Decoder` / `decode_body` / `DecodedBody` / `AsyncDecodedBody`** — the
  client-side decode path, driven through the resumable push decoders in
  `oxiarc-deflate` / `oxiarc-brotli` / `oxiarc-zstd` / `oxiarc-lzw`, never a
  `read_to_end`. A slice-primary, allocation-free `decode()`; `feed_into`
  for a push body loop (`reqwest::Response::chunk`, a `hyper` frame loop);
  `Read` + `BufRead` and `tokio::io::AsyncRead` adapters. `gzip` is
  multi-member (RFC 1952 §2.2), `deflate` accepts all three spellings
  servers actually send (zlib, raw, and a whole gzip stream mislabelled
  `deflate`), `zstd` is multi-frame, `compress` decodes the legacy UNIX
  `.Z` format (bridged from `oxiarc_lzw::z::ZReader`'s pull shape onto this
  crate's push seam — see `decode/compress.rs`'s module docs for how), and
  `dcz`/`dcb` decode against a caller-supplied dictionary
  (`Decoder::with_dictionary`) after verifying each body's own preamble
  names that same dictionary.
  `.Z` has no end-of-information code: a truncated `compress` body decodes
  to a silently short, valid prefix rather than an error — the one coding
  here where that is the documented, correct behaviour, not a bug (see
  `ContentCoding::Compress`'s doc comment).
- **Truly incremental, measured** — streaming a 16 MiB gzip body peaks at
  **~210 KiB** of live allocation; `feed_into` allocates **nothing** per call
  after warm-up; the decoded bytes are identical however the wire data is
  split (byte-at-a-time, arbitrary proptest split points, 64 KiB chunks).
  Throughput is **0.96–0.98x** of `oxiarc_deflate::gzip_decompress` — i.e.
  making the decode incremental and bounded costs 2–4%.
- **Bomb-safe inside a block, not between blocks** — one fixed-Huffman
  DEFLATE block of 812 KB expands to 123 MiB, so `max_output` is enforced by
  truncating the output slice handed to the codec, which makes
  `oxiarc-deflate`'s own bounded sink enforce the HTTP budget on every
  literal and match copy. The test suite regenerates that exact stream from a
  committed Rust generator and asserts a 1 MiB cap stops it at 1 MiB.
- **`finish()` is mandatory and enforced** — it verifies gzip's CRC-32 and
  `ISIZE`, zlib's Adler-32, zstd's XXH64 and every codec's truncation check.
  `DecodedBody` calls it at EOF, so a truncated response is an `io::Error`
  from the final read, never a silently short body.
- **Cargo feature matrix** — `default = ["gzip", "deflate"]`; `brotli`,
  `zstd`, `compress` (legacy UNIX `.Z`, both directions), `async-io`
  (`AsyncDecodedBody`) and `http-oracle` (differential tests) all opt-in.
  Every combination, including `--no-default-features`, builds and is
  clippy-clean.

## Quick Start

```rust
use oxiarc_http::{AcceptEncoding, ContentCoding, EncodeOptions, encode_body, negotiate};

// Client side: advertise every coding this build can decode.
let accept = AcceptEncoding::all_supported();
let header_value = accept.to_header_value(); // None => send no header at all

// Server side: negotiate against what it received, and only what this
// build can actually produce.
let available: Vec<ContentCoding> = [ContentCoding::Gzip, ContentCoding::Deflate]
    .into_iter()
    .filter(ContentCoding::is_encodable)
    .collect();
let chosen = negotiate(header_value.as_deref(), &available)?;

let body = b"hello, world! hello, world! hello, world!";
if let Some(coding) = chosen {
    let compressed = encode_body(&coding, body, EncodeOptions::default())?;
    // ... set Content-Encoding: coding.as_str(), Content-Length, Vary: Accept-Encoding
}
```

Decoding a response body, bounded and with checksums verified:

```rust
use oxiarc_http::{DecodeLimits, DecodedBody};

// `wire` is the raw response body, `encoding` its Content-Encoding header
// value ("identity" when the header is absent).
let mut body = DecodedBody::new(wire_reader, encoding, &DecodeLimits::default())?;
let text = body.read_to_string()?;   // a truncated body is an Err, never short
```

Or, for a push-shaped transport (`reqwest`, `hyper`):

```rust
use oxiarc_http::{DecodeLimits, Decoder};

let mut decoder = Decoder::from_header(encoding, &DecodeLimits::default())?;
let mut out = Vec::new();
while let Some(chunk) = next_chunk() {
    decoder.feed_into(&chunk, &mut out)?;   // errors *before* a bomb lands
}
decoder.finish_into(&mut out)?;             // REQUIRED: verifies checksums
```

Runnable integration recipes live in `examples/`:
`ureq3_manual_gzip`, `reqwest_bytes_stream`, `oxihttp_client`, plus
`http_fuzz_seeds` (the seed-corpus generator for this crate's two fuzz targets,
`fuzz_http_decode` and `fuzz_http_headers`).

See the crate-level rustdoc (`cargo doc -p oxiarc-http --all-features --open`)
for the full API, the `Transfer-Encoding`-out-of-scope statement, and the
`HEAD`/204/304/`Range` empty-body notes.

## Testing

```bash
cargo nextest run -p oxiarc-http --all-features
cargo test --doc -p oxiarc-http --all-features
cargo clippy -p oxiarc-http --all-features --all-targets -- -D warnings
cargo build -p oxiarc-http --no-default-features
cargo bench -p oxiarc-http --all-features

# Differential tests against CPython's zlib/gzip and the brotli/zstd CLIs.
# Each self-skips (does not fail) when its tool is absent.
cargo nextest run -p oxiarc-http --features http-oracle,gzip,deflate,brotli,zstd
```

The headers/negotiation/encode layer keeps its tests co-located
(`#[cfg(test)] mod tests` in each `src/*.rs`); the decode layer adds an
integration suite in `tests/`, because what it has to prove is external
behaviour:

| File | Proves |
|---|---|
| `roundtrip.rs` | every coding x payload agrees across all four entry points |
| `chunking.rs` | chunk invariance, incl. proptest over arbitrary split points |
| `framing.rs` | gzip members/flags/FHCRC, the `deflate` sniff, trailing policy, chains |
| `limits.rs` | the 812 KB -> 123 MiB single-block bomb, ratio, boundaries, windows |
| `dictionary.rs` | `dcz`/`dcb` against a supplied dictionary; wrong/missing dictionary refused |
| `async_body.rs` | a `Poll::Pending` source is never an empty body |
| `allocations.rs` | peak-memory and zero-allocation gates (counting allocator) |
| `fuzz_seeds.rs` | the invariants `fuzz_http_decode`/`fuzz_http_headers` assert |
| `http_oracle.rs` | reference-encoder differential (feature `http-oracle`): gzip/deflate/brotli/zstd both directions, plus `compress` (`compress`/`uncompress`/`gzip -dc`, including the prefix a *truncated* `.Z` decodes to, checked against `uncompress -c` at every third offset) and the `dcz` preamble (`zstd -D <dict> -d`) — the legs that had no external anchor before |

A co-located test is **not** a substitute for external linkage, though, and
this crate has already been bitten by the difference: code inside the crate
may build a `#[non_exhaustive]` struct with struct-expression syntax, and a
downstream crate may not. The **doctests** are the guard for that class,
because rustdoc compiles each one as its own external crate. They are not a
blanket second copy of the unit tests — the rule is narrower: anything whose
correctness depends on being *constructible or callable from outside* must
carry a doctest as well as a unit test. Every `EncodeOptions` builder does,
for exactly that reason.

Part of the [OxiArc](https://github.com/cool-japan/oxiarc) Pure Rust
archive/compression ecosystem.
