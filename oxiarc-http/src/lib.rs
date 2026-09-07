//! HTTP content-coding support for OxiArc (RFC 9110 "Content-Encoding" /
//! "Accept-Encoding").
//!
//! Part of the [OxiArc](https://github.com/cool-japan/oxiarc) Pure Rust
//! archive/compression ecosystem. This crate is the headers/negotiation
//! layer: parsing and rendering the two headers, RFC 9110 §12.5.3
//! server-side negotiation, decompression-bomb limits, the shared error
//! type, and server-side response encoding. It has **no** dependency on the
//! `http` crate — every header value is a plain `&str` in, an owned `String`
//! or `Vec<u8>` out — so it works unmodified from `ureq`, `reqwest`,
//! `oxihttp`, or any hand-rolled client or server.
//!
//! # What is (and is not) in this crate yet
//!
//! This is the **headers/negotiation/encode** layer. The matching
//! **decoder** — a `Decoder` that actually undoes `gzip`/`deflate`/`br`/`zstd`
//! on a response body, `Read`/`BufRead`/async adapters, and a `decode_body`
//! one-shot helper — is a separate, later wave, gated on the resumable push
//! decoders landing in `oxiarc-deflate`/`oxiarc-brotli`/`oxiarc-zstd`. Adding
//! it means adding new files (a `decode` module, most likely `decode/mod.rs`
//! plus one file per coding) plus a handful of `pub use` lines in this file
//! — nothing here needs to change shape to make room for it. Until then:
//!
//! - [`ContentCoding`], [`parse_content_encoding`], [`QValue`],
//!   [`AcceptEncoding`], [`negotiate`] and [`DecodeLimits`] are all usable
//!   today on the client side (build the request header, parse the response
//!   header) and the server side (negotiate, then [`encode_body`]).
//! - [`encode_body`] and [`Encoder`] are the full server-side story: given a
//!   coding, they produce the compressed bytes (or a streaming `Write`
//!   wrapper that does).
//! - There is deliberately no `Decoder` type, `decode_body` function, or
//!   `Read`/`BufRead`/async response-body wrapper in this version.
//!
//! [`negotiate`] itself is written for the response direction (a server
//! choosing a `Content-Encoding` against a client's `Accept-Encoding`); a
//! server that wants to *decode* an encoded request body — the mirror
//! case, with no negotiation involved, since the client already committed
//! to a coding — will do so through the same future `Decoder` machinery
//! mentioned above, once it exists.
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
//! of the following to a decoder (once one exists) even if they carry a
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
//! # Cargo features
//!
//! | Feature | Default | Adds |
//! |---|---|---|
//! | `gzip` | on | [`ContentCoding::Gzip`] via `oxiarc-deflate` |
//! | `deflate` | on | [`ContentCoding::Deflate`] via `oxiarc-deflate` |
//! | `brotli` | off | [`ContentCoding::Brotli`] via `oxiarc-brotli` |
//! | `zstd` | off | [`ContentCoding::Zstd`] and, with a dictionary, [`ContentCoding::Dcz`], via `oxiarc-zstd` |
//! | `compress` | off | Plumbing only — see [`ContentCoding::Compress`] |
//! | `async-io` | off | Plumbing for the future async decoder wave |
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

#![warn(missing_docs)]
#![warn(clippy::all)]
#![forbid(unsafe_code)]

mod accept;
mod coding;
mod encode;
mod error;
mod header;
mod limits;
mod negotiate;

pub use accept::AcceptEncoding;
pub use coding::ContentCoding;
pub use encode::{EncodeOptions, Encoder, encode_body};
pub use error::{HttpCodingError, LimitKind, UnsupportedReason};
pub use header::{
    AcceptEntry, QValue, parse_accept_encoding, parse_content_encoding, parse_content_encoding_all,
};
pub use limits::DecodeLimits;
pub use negotiate::{NotAcceptable, negotiate};

pub use error::Result;
