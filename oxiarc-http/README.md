# oxiarc-http [Partial: headers/negotiation/encode done, decoder pending]

HTTP content-coding (RFC 9110 `Content-Encoding` / `Accept-Encoding`: gzip, deflate, br, zstd, dcz) for OxiArc — Pure Rust, no `flate2`, no `http` crate dependency.

[![Crates.io](https://img.shields.io/crates/v/oxiarc-http.svg)](https://crates.io/crates/oxiarc-http)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)
![Status](https://img.shields.io/badge/status-Partial-yellow)

**Version: 0.4.2 (unreleased) | 90 tests passing (nextest, all features) + 6 doctests**

## Overview

`oxiarc-http` closes the last common route by which `flate2` (and its C
dependency chain) enters a `~/work` project: `ureq`'s default `gzip` feature.
It parses and renders `Content-Encoding`/`Accept-Encoding`, implements RFC
9110 §12.5.3 server-side negotiation exactly, bounds decompression-bomb
exposure, and encodes response bodies through the existing `oxiarc-deflate`
/ `oxiarc-brotli` / `oxiarc-zstd` codecs — all with a plain-`&str` API, no
dependency on the `http` crate, so it drops into `ureq`, `reqwest`,
`oxihttp`, or any hand-rolled client or server unmodified.

**What's in this version:** the headers/negotiation/encode layer —
[`ContentCoding`], header parsing, [`QValue`], the [`AcceptEncoding`]
builder, [`negotiate`], [`DecodeLimits`], the crate's error type, and
server-side [`encode_body`] / [`Encoder`]. **Not yet in this version:** a
response-body `Decoder` (the client-side decode path). That lands in a
later wave, gated on resumable push decoders landing in
`oxiarc-deflate`/`oxiarc-brotli`/`oxiarc-zstd`; see `TODO.md`. The module
boundary is deliberately drawn so that wave only adds new files plus a
handful of `pub use` lines in `lib.rs` — nothing documented below changes
shape to make room for it.

## Features

- **Pure Rust, no `http` crate** — every header value is `&str` in, owned
  data out; works from any HTTP client or server without pulling in a
  particular `http`-crate version.
- **`ContentCoding`** — `Identity`, `Compress`, `Deflate`, `Gzip`, `Brotli`,
  `Zstd`, `Dcb`, `Dcz`, `Unknown(String)`; case-insensitive parsing with the
  `x-gzip`/`x-compress` aliases; declared least-to-most-preferred so the
  derived `Ord` is a ready-made client-side preference order.
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
  server-side response encoding for gzip, deflate, brotli, zstd, and (with
  a shared dictionary) `dcz` (RFC 9842 Compression Dictionary Transport).
- **`DecodeLimits`** — `max_output` (the load-bearing bomb control, default
  64 MiB), `max_ratio` (defense-in-depth, documented with the measurements
  behind the default), `max_codings`.
- **Bomb-safe by construction** — every limit is documented with the exact
  measured legitimate-vs-hostile ratios that justify its default, not a
  guessed number.
- **Cargo feature matrix** — `default = ["gzip", "deflate"]`; `brotli`,
  `zstd`, `compress` (plumbing only — see `ContentCoding::Compress`),
  `async-io` (plumbing for the future decoder) all opt-in. Every
  combination, including `--no-default-features`, builds and is
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

See the crate-level rustdoc (`cargo doc -p oxiarc-http --all-features --open`)
for the full API, the `Transfer-Encoding`-out-of-scope statement, and the
`HEAD`/204/304/`Range` empty-body notes.

## Testing

```bash
cargo nextest run -p oxiarc-http --all-features
cargo test --doc -p oxiarc-http --all-features
cargo clippy -p oxiarc-http --all-features --all-targets -- -D warnings
cargo build -p oxiarc-http --no-default-features
```

Every test lives as a unit test co-located with the code it exercises
(`#[cfg(test)] mod tests` in each `src/*.rs` file) and exclusively calls
`pub` items — verified by inspection, not merely assumed — so it doubles as
an integration test of the public API without a separate `tests/`
directory duplicating the same coverage.

Part of the [OxiArc](https://github.com/cool-japan/oxiarc) Pure Rust
archive/compression ecosystem.
