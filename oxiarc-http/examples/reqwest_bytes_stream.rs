//! Recipe: **reqwest with no `gzip`/`brotli`/`zstd` features**, decoding an
//! async body through the push [`Decoder`] as chunks arrive.
//!
//! ```text
//! cargo run -p oxiarc-http --example reqwest_bytes_stream
//! ```
//!
//! `reqwest` is not a dependency of this crate (see
//! `examples/ureq3_manual_gzip.rs` for why a dev-dependency would be worse
//! than useless here), so what this example *runs* is the same push loop
//! driven by a simulated chunk stream. The loop body is byte-for-byte the
//! one you would write against `Response::chunk()`.
//!
//! # The Cargo.toml
//!
//! ```toml
//! [dependencies]
//! # reqwest 0.13's default feature set does NOT include gzip; just never
//! # add it. `rustls-no-provider` keeps the TLS stack Pure Rust.
//! reqwest = { version = "0.13", default-features = false, features = [
//!     "rustls-no-provider", "json",
//! ] }
//! oxiarc-http = { version = "0.4", features = ["brotli", "zstd"] }
//! ```
//!
//! # The request/response code
//!
//! ```ignore
//! use oxiarc_http::{AcceptEncoding, DecodeLimits, Decoder};
//!
//! let response = client
//!     .get(url)
//!     .header("accept-encoding", AcceptEncoding::default().to_header_value_or_empty())
//!     .send()
//!     .await?;
//!
//! // Read the header before the body is consumed.
//! let encoding = response
//!     .headers()
//!     .get(reqwest::header::CONTENT_ENCODING)
//!     .and_then(|v| v.to_str().ok())
//!     .unwrap_or("identity")
//!     .to_owned();
//!
//! let mut decoder = Decoder::from_header(&encoding, &DecodeLimits::default())?;
//! let mut out = Vec::new();
//! let mut response = response;
//! // `chunk()` avoids a `futures` dependency; `bytes_stream()` is identical.
//! while let Some(chunk) = response.chunk().await? {
//!     // Bounded: this errors *before* a decompression bomb lands, not
//!     // after. `out` never grows past `DecodeLimits::max_output`.
//!     decoder.feed_into(&chunk, &mut out)?;
//! }
//! // REQUIRED. Skipping it skips gzip's CRC-32 and ISIZE, zlib's Adler-32
//! // and zstd's XXH64 — i.e. silently accepts a corrupted body.
//! decoder.finish_into(&mut out)?;
//! ```
//!
//! An `AsyncRead`-shaped source (a `tokio::net::TcpStream`, a hyper body
//! adapted through `StreamReader`) is better served by
//! `AsyncDecodedBody`, behind the `async-io` feature.

use oxiarc_http::{AcceptEncoding, DecodeLimits, Decoder};

/// Stand-in for `reqwest::Response::chunk()`: hands back the next slice of
/// wire bytes, or `None` at end of body.
struct ChunkStream<'a> {
    wire: &'a [u8],
    pos: usize,
    step: usize,
}

impl<'a> ChunkStream<'a> {
    fn next_chunk(&mut self) -> Option<&'a [u8]> {
        if self.pos >= self.wire.len() {
            return None;
        }
        let end = (self.pos + self.step).min(self.wire.len());
        let chunk = &self.wire[self.pos..end];
        self.pos = end;
        Some(chunk)
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!(
        "Accept-Encoding: {}",
        AcceptEncoding::default().to_header_value_or_empty()
    );

    let plain: String = (0..2000)
        .map(|i| format!("{{\"id\":{i},\"name\":\"row {i}\"}},"))
        .collect();
    let encoding = "gzip";
    let wire = oxiarc_deflate::gzip_compress(plain.as_bytes(), 6)?;

    let mut decoder = Decoder::from_header(encoding, &DecodeLimits::default())?;
    let mut out = Vec::new();
    let mut stream = ChunkStream {
        wire: &wire,
        pos: 0,
        step: 1500, // roughly one TCP segment
    };
    let mut chunks = 0usize;
    while let Some(chunk) = stream.next_chunk() {
        decoder.feed_into(chunk, &mut out)?;
        chunks += 1;
    }
    decoder.finish_into(&mut out)?;

    assert_eq!(out, plain.as_bytes());
    println!(
        "decoded {} bytes from {} wire bytes in {chunks} chunks; checksums verified",
        out.len(),
        wire.len()
    );

    // The same loop, but the server sent a bomb and the client had a budget.
    let bomb_plain = vec![0u8; 32 * 1024 * 1024];
    let bomb = oxiarc_deflate::gzip_compress(&bomb_plain, 9)?;
    let limits = DecodeLimits::default().with_max_output(1024 * 1024);
    let mut decoder = Decoder::from_header("gzip", &limits)?;
    let mut out = Vec::new();
    let mut stream = ChunkStream {
        wire: &bomb,
        pos: 0,
        step: 1500,
    };
    let mut error = None;
    while let Some(chunk) = stream.next_chunk() {
        if let Err(e) = decoder.feed_into(chunk, &mut out) {
            error = Some(e);
            break;
        }
    }
    match error {
        Some(e) => println!(
            "bomb refused after materialising {} bytes (cap {}): {e}",
            out.len(),
            limits.max_output
        ),
        None => println!("unexpected: the bomb was not refused"),
    }

    Ok(())
}
