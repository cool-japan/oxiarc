//! HTTP content-coding support for OxiArc (RFC 9110 "Content-Encoding" /
//! "Accept-Encoding").
//!
//! Part of the [OxiArc](https://github.com/cool-japan/oxiarc) Pure Rust
//! archive/compression ecosystem. This crate is the whole content-coding
//! layer: parsing and rendering the two headers, RFC 9110 §12.5.3
//! server-side negotiation, streaming **decoding** of response bodies under
//! decompression-bomb limits, server-side response encoding, and the shared
//! error type. It has **no** dependency on the
//! `http` crate — every header value is a plain `&str` in, an owned `String`
//! or `Vec<u8>` out — so it works unmodified from `ureq`, `reqwest`,
//! `oxihttp`, or any hand-rolled client or server.
//!
//! # What is in this crate
//!
//! | Direction | Entry points |
//! |---|---|
//! | Client: build the request header | [`AcceptEncoding`], [`QValue`] |
#![cfg_attr(
    feature = "async-io",
    doc = "| Client: decode the response body | [`decode_body`], [`Decoder`], [`DecodedBody`], [`AsyncDecodedBody`] |"
)]
#![cfg_attr(
    not(feature = "async-io"),
    doc = "| Client: decode the response body | [`decode_body`], [`Decoder`], [`DecodedBody`], `AsyncDecodedBody` (needs `async-io`) |"
)]
//! | Server: choose a coding | [`negotiate`], [`parse_accept_encoding`] |
//! | Server: encode the response body | [`encode_body`], [`Encoder`], [`negotiate_and_encode`] |
//! | Both: parse `Content-Encoding` | [`parse_content_encoding`], [`ContentCoding`] |
//! | Both: bound the work | [`DecodeLimits`], [`TrailingData`] |
//!
//! [`negotiate`] is written for the response direction (a server choosing a
//! `Content-Encoding` against a client's `Accept-Encoding`). A server that
//! wants to *decode* an encoded **request** body is the mirror case with no
//! negotiation involved — the client already committed to a coding — so it
//! uses the same [`Decoder`] machinery directly, against the request's own
//! `Content-Encoding`.
//!
//! # Decoding is genuinely incremental
//!
//! Every coding is driven through a resumable push decoder
//! (`oxiarc_deflate::WrappedInflate`, `oxiarc_brotli::BrotliStream`,
//! `oxiarc_zstd::ZstdStream`; `compress` bridges `oxiarc_lzw::z::ZReader`'s
//! pull shape onto the same push seam — see `decode/compress.rs`'s module
//! docs), never through a `read_to_end`. Concretely, measured in
//! `tests/allocations.rs`:
//!
//! * streaming a 16 MiB gzip body through [`DecodedBody`] with 4 KiB reads
//!   peaks at **~174 KiB** of live allocation — two 64 KiB staging buffers,
//!   the 32 KiB DEFLATE window and its tables;
//! * streaming a 128 MiB `compress` (`.Z`) body that expands **4992:1**,
//!   with no output budget at all
//!   ([`DecodeLimits::unlimited`](crate::DecodeLimits::unlimited)), peaks at
//!   **~873 KiB** — the 192 KiB `.Z` code table and one metered decode fill
//!   on top of the same staging buffers;
//! * [`Decoder::feed_into`] performs **zero allocations** per call once warm;
//! * a decompression bomb is refused having materialised the budget, not the
//!   bomb.
//!
//! The bytes are identical however the wire data is split: feeding one byte
//! at a time, in 4 KiB chunks, or all at once produces the same output
//! (`tests/chunking.rs`, including a proptest over arbitrary split points).
//!
//! # `finish` is not optional
//!
//! [`Decoder::finish`] / [`Decoder::close`] is what verifies gzip's CRC-32
//! and `ISIZE`, zlib's Adler-32, zstd's XXH64 and every other codec's
//! truncation check. Skipping it silently accepts a corrupted or truncated
//! body — for every coding but one: `compress` (`.Z`) has no checksum and
//! no end-of-information code at all, so a truncated `.Z` body decodes to a
//! plausible, valid-looking short prefix whatever `finish` does or does not
//! check (see [`ContentCoding::Compress`]'s own doc comment).
#![cfg_attr(
    feature = "async-io",
    doc = "[`DecodedBody`] and [`AsyncDecodedBody`] call it for you at EOF and"
)]
#![cfg_attr(
    not(feature = "async-io"),
    doc = "[`DecodedBody`] and `AsyncDecodedBody` call it for you at EOF and"
)]
//! surface a failure as an `io::Error` from the final read, so a truncated
//! response can never look like a short one.
//!
//! # `Transfer-Encoding` is out of scope
//!
//! This crate handles `Content-Encoding` and `Accept-Encoding` only.
//! `Transfer-Encoding` (chunked framing) is an HTTP/1.1 wire-transport
//! concern owned by whatever client or server library you use — it is
//! resolved before any bytes reach this crate, and re-implementing chunked
//! transfer coding here would duplicate logic that must already exist in
//! every HTTP implementation this crate is meant to plug into. RFC 9110 also
//! permits a `Transfer-Encoding: gzip`-shaped value in principle; that,
//! too, is the transport's problem to unwrap before `Content-Encoding`
//! (and this crate) ever come into play.
//!
//! # Responses with no body: HEAD, 204, 304, and `Range`
//!
//! A `Content-Encoding` header describes how a body *would be* encoded —
//! it says nothing about whether a body is present at all. Do not feed any
//! of the following to a [`Decoder`] (or [`decode_body`]) even if they carry a
//! `Content-Encoding` header:
//!
//! - A response to a `HEAD` request: it has no body by definition, encoded
//!   or otherwise.
//! - `204 No Content` and `304 Not Modified`: defined by HTTP to never carry
//!   a body, even though header fields that would normally describe one
//!   (including `Content-Encoding`) may still be present.
//! - A response obtained with a `Range` request: the body is a byte range
//!   of the *encoded* representation, not a complete, independently
//!   decodable stream — there is nothing a decoder could correctly do with
//!   it in isolation.
//!
//! Feeding an encoded, genuinely empty body from one of the first two cases
//! to a decoder is not the same as feeding it a truncated stream, and a
//! correct caller distinguishes the two before decoding is ever attempted.
//!
//! # Decompression bombs
//!
//! [`DecodeLimits::max_output`] is the load-bearing control; every other
//! limit is defense-in-depth. See that type's docs for the measurements
//! behind the defaults.
//!
//! The cap is enforced **inside** a compressed block, not between blocks.
//! That distinction is the whole design: one fixed-Huffman DEFLATE block of
//! 812 KB expands to 123 MiB (length-258, distance-1 back-references at 13
//! bits each — a factor of 158.8), so a decoder that checks its budget at
//! block boundaries checks it exactly once, after the damage.
//! `tests/limits.rs` regenerates that stream from a committed generator and
//! asserts a 1 MiB cap stops it having produced 1 MiB.
//!
//! `max_ratio` cannot do this job: measured legitimate traffic reaches 411x
//! and the classic bomb is 1029x, a gap of 2.5x that any attacker can pad
//! their way under. Lower `max_output`, not `max_ratio`.
//!
//! # Security notes
//!
//! * **An unknown coding is refused, never passed through.** A client that
//!   silently hands compressed bytes to a JSON parser is the failure mode
//!   this crate exists to remove.
//! * **Trailing data is rejected by default** ([`TrailingData::Reject`]):
//!   bytes after a complete stream are the shape a response-splitting attack
//!   takes. [`TrailingData::AllowZeros`] is available for peers that pad
//!   gzip with `0x00`.
//! * **Every allocation is bounded** by [`DecodeLimits`] or by a fixed
//!   staging buffer. A declared zstd window above 8 MiB (the largest an HTTP
//!   `zstd` decoder must support) is refused *before* the ring is allocated.
//! * **Chained codings are bounded per stage**, because an intermediate
//!   stage of `gzip, gzip` can be a bomb even when the final body is small.
//!   [`DecodeLimits::max_codings`] bounds the number of stages.
//! * A `finish` error means the response is corrupt — and bytes already
//!   handed back are not trustworthy, because a checksum necessarily covers
//!   content that has already been streamed out. Buffer until `finish`
//!   succeeds if you must not act on unverified data.
//!
//! # Integration recipes
//!
//! Runnable, in `examples/`:
//!
//! | Example | Client |
//! |---|---|
//! | `ureq3_manual_gzip` | `ureq` 3 with `default-features = false` |
//! | `reqwest_bytes_stream` | `reqwest` with no compression features, push decoding |
//! | `oxihttp_client` | replacing `oxihttp`'s hand-rolled coding code |
//!
//! Neither `ureq` nor `reqwest` is a dependency of this crate, not even a
//! dev-dependency: `cargo deny check bans` walks dev-dependencies, and
//! `Cargo.lock` records a crate's optional dependencies whether or not their
//! feature is on — so adding `ureq` would write `flate2` into this
//! workspace's lockfile, which is exactly what the crate removes. Each
//! example carries the real wiring in its module docs and runs the same
//! calls against a canned response.
//!
//! # Cargo features
//!
//! | Feature | Default | Adds |
//! |---|---|---|
//! | `gzip` | on | [`ContentCoding::Gzip`] via `oxiarc-deflate` |
//! | `deflate` | on | [`ContentCoding::Deflate`] via `oxiarc-deflate` |
//! | `brotli` | off | [`ContentCoding::Brotli`] and, with a dictionary, [`ContentCoding::Dcb`], via `oxiarc-brotli` |
//! | `zstd` | off | [`ContentCoding::Zstd`] and, with a dictionary, [`ContentCoding::Dcz`], via `oxiarc-zstd` |
//! | `compress` | off | [`ContentCoding::Compress`] (legacy UNIX `.Z`) via `oxiarc-lzw` |
#![cfg_attr(
    feature = "async-io",
    doc = "| `async-io` | off | [`AsyncDecodedBody`], the `tokio::io::AsyncRead` adapter |"
)]
#![cfg_attr(
    not(feature = "async-io"),
    doc = "| `async-io` | off | `AsyncDecodedBody`, the `tokio::io::AsyncRead` adapter |"
)]
//! | `http-oracle` | off | Differential tests against python3 / the `brotli` and `zstd` CLIs |
//!
//! `gzip` and `deflate` are independent switches over the same
//! `oxiarc-deflate` dependency: enabling one does not enable the other.
//!
//! # Example
//!
//! Build a request `Accept-Encoding` header, negotiate a response coding
//! server-side, and encode a body with it:
//!
//! ```
//! use oxiarc_http::{AcceptEncoding, ContentCoding, EncodeOptions, encode_body, negotiate};
//!
//! // Client side: advertise every coding this build can decode.
//! let accept = AcceptEncoding::all_supported();
//! let header_value = accept.to_header_value(); // None => send no header at all
//!
//! // Server side: negotiate against what it received, and only what this
//! // build can actually produce. Never hardcode the list — filter by
//! // `is_encodable` so it tracks this crate's own compiled-in features,
//! // the same footgun `AcceptEncoding::add` guards against on the client side.
//! let available: Vec<ContentCoding> = [
//!     ContentCoding::Zstd,
//!     ContentCoding::Brotli,
//!     ContentCoding::Gzip,
//!     ContentCoding::Deflate,
//! ]
//! .into_iter()
//! .filter(ContentCoding::is_encodable)
//! .collect();
//! let chosen = negotiate(header_value.as_deref(), &available)
//!     .expect("identity is always acceptable here, so this never fails");
//!
//! let body = b"hello, world! hello, world! hello, world!";
//! match chosen {
//!     Some(coding) => {
//!         let compressed = encode_body(&coding, body, EncodeOptions::default())
//!             .expect("`available` only ever contains codings this build can encode");
//!         assert!(compressed.len() < body.len());
//!         // ... set Content-Encoding: coding.as_str(), Content-Length, Vary: Accept-Encoding
//!     }
//!     None => {
//!         // ... send `body` as-is, with no Content-Encoding header.
//!     }
//! }
//! ```
//!
//! And the client side of the same exchange — decode a response body, with
//! bomb limits on and checksums verified:
//!
//! ```
//! use oxiarc_http::{DecodeLimits, DecodedBody};
//!
//! # let wire = oxiarc_deflate::gzip_compress(b"hello, world", 6).expect("compress");
//! # let content_encoding = "gzip";
//! // `wire` is the raw response body; `content_encoding` its header value
//! // ("identity" when the header is absent).
//! let mut body = DecodedBody::new(&wire[..], content_encoding, &DecodeLimits::default())?;
//! let text = body.read_to_string()?;
//! assert_eq!(text, "hello, world");
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

#![warn(missing_docs)]
#![warn(clippy::all)]
#![forbid(unsafe_code)]

mod accept;
mod coding;
mod decode;
mod encode;
mod error;
mod header;
mod limits;
mod negotiate;
mod read;

#[cfg(feature = "async-io")]
mod async_read;

pub use accept::AcceptEncoding;
pub use coding::ContentCoding;
pub use decode::{
    DecodeStatus, Decoder, Progress, TrailingData, decode_body, decode_body_from_header,
};
pub use encode::{EncodeOptions, Encoder, NegotiateEncodeError, encode_body, negotiate_and_encode};
pub use error::{HttpCodingError, LimitKind, UnsupportedReason};
pub use header::{
    AcceptEntry, QValue, parse_accept_encoding, parse_content_encoding, parse_content_encoding_all,
};
pub use limits::DecodeLimits;
pub use negotiate::{NotAcceptable, negotiate};
pub use read::DecodedBody;

#[cfg(feature = "async-io")]
pub use async_read::AsyncDecodedBody;

/// Re-exported from `oxiarc-core` so callers of [`Decoder::decode`] do not
/// need to name that crate.
///
/// Only two values matter to an HTTP body: [`FlushMode::Finish`] means "this
/// is the last wire data that will ever arrive", and everything else means
/// "more may follow".
pub use oxiarc_core::traits::FlushMode;

pub use error::Result;
