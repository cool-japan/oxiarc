//! Recipe: **ureq 3 with `default-features = false`**, doing content coding
//! through `oxiarc-http` instead of `flate2`.
//!
//! This is the Tier-A route the whole Phase 8 program exists to close: an
//! ecosystem-wide audit found `flate2` in 39 of 93 `~/work` lockfiles, and
//! `ureq`'s default `gzip` feature is one of the two dominant paths in.
//!
//! ```text
//! cargo run -p oxiarc-http --example ureq3_manual_gzip
//! ```
//!
//! # `ureq` is deliberately NOT a dependency of this crate
//!
//! Not even a dev-dependency. `cargo deny check bans` walks dev-dependencies
//! too, and `Cargo.lock` is feature-agnostic: it records a crate's optional
//! dependencies whether or not the feature that gates them is on. Adding
//! `ureq` — whose `gzip` feature pulls `flate2` — would therefore write
//! `flate2` into this workspace's lockfile, which is exactly the thing the
//! program is removing. The wiring below is real, copy-pasteable code; what
//! this example actually *runs* is the same `DecodedBody` call against a
//! canned response body, so the mechanics stay compile-checked and
//! executable without the dependency.
//!
//! # The Cargo.toml
//!
//! ```toml
//! [dependencies]
//! # default-features = false drops ureq's `gzip` (flate2) *and* its default
//! # `rustls` stack (ring: C and assembly). Restore a Pure Rust TLS stack
//! # explicitly — the established COOLJAPAN pattern.
//! ureq = { version = "3.4", default-features = false, features = [
//!     "rustls-no-provider", "rustls-webpki-roots",
//! ] }
//! oxiarc-http = { version = "0.4", features = ["brotli", "zstd"] }
//! ```
//!
//! # The request/response code
//!
//! ```ignore
//! use oxiarc_http::{AcceptEncoding, DecodeLimits, DecodedBody};
//!
//! // With its compression features off, ureq sends NO Accept-Encoding at
//! // all, so advertise what *this* build can decode. `AcceptEncoding` is
//! // feature-aware: it never advertises a coding the decoder would then
//! // refuse.
//! let accept = AcceptEncoding::default();
//! let response = ureq::get(url)
//!     .header("accept-encoding", accept.to_header_value_or_empty())
//!     .call()?;
//!
//! // With decompression compiled out, ureq leaves `Content-Encoding` in
//! // place and hands back RAW bytes. Skipping the next step yields
//! // compressed bytes with no error at all — the trap this recipe exists
//! // to avoid. Read the header BEFORE consuming the response.
//! let encoding = response
//!     .headers()
//!     .get("content-encoding")
//!     .and_then(|v| v.to_str().ok())
//!     .unwrap_or("identity")
//!     .to_owned();
//!
//! // `into_body().into_reader()` yields an owned `BodyReader<'static>`, so
//! // no borrow of `response` stays alive.
//! let mut body = DecodedBody::new(
//!     response.into_body().into_reader(),
//!     &encoding,
//!     &DecodeLimits::default(),   // 64 MiB cap, bomb guard on
//! )?;
//! let text = body.read_to_string()?;
//! ```
//!
//! Memory is bounded at roughly 210 KiB regardless of body size (measured in
//! `tests/allocations.rs`), and a truncated, corrupt or over-budget body is
//! an `io::Error` from the final read — never a silently short string.

use oxiarc_http::{AcceptEncoding, DecodeLimits, DecodedBody};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 1. What a client would put in the request.
    let accept = AcceptEncoding::default();
    println!("Accept-Encoding: {}", accept.to_header_value_or_empty());

    // 2. A canned response: the header the server sent, and the raw wire
    //    bytes ureq would hand back with its compression features off.
    let plain =
        "{\"status\":\"ok\",\"rows\":[1,2,3],\"note\":\"decoded through oxiarc-http\"}".repeat(64);
    let content_encoding = "gzip";
    let wire = oxiarc_deflate::gzip_compress(plain.as_bytes(), 6)?;
    println!(
        "Content-Encoding: {content_encoding} ({} wire bytes for {} decoded)",
        wire.len(),
        plain.len()
    );

    // 3. The only step that differs from a `flate2`-enabled ureq build.
    let mut body = DecodedBody::new(&wire[..], content_encoding, &DecodeLimits::default())?;
    let text = body.read_to_string()?;

    assert_eq!(text, plain);
    println!("decoded {} bytes, checksums verified", text.len());

    // 4. What happens when the server sends something the build cannot
    //    decode: a clean, named error — never a silent pass-through of
    //    compressed bytes to a JSON parser.
    match DecodedBody::new(&wire[..], "shrink-o-matic", &DecodeLimits::default()) {
        Ok(_) => println!("unexpected: an unknown coding was accepted"),
        Err(error) => println!("unknown coding refused as expected: {error}"),
    }

    Ok(())
}
