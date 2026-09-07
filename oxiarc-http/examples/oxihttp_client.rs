//! Recipe: **`oxihttp`** — replacing its hand-rolled content-coding code
//! with `oxiarc-http`, on both the client and the server side.
//!
//! ```text
//! cargo run -p oxiarc-http --example oxihttp_client
//! ```
//!
//! `oxihttp` lives outside this repository, so this example runs the two
//! replacement bodies against canned data rather than over a socket. The
//! code shapes are the ones to paste.
//!
//! # Client side
//!
//! Delete `bounded_inflate`, `bounded_deflate_decompress`,
//! `gzip_deflate_payload_start`, `bounded_gzip_decompress` and
//! `decompression_feature_missing_error`, then:
//!
//! ```ignore
//! pub async fn body_bytes(self) -> Result<Bytes, OxiHttpError> {
//!     // Copy every field needed BEFORE `into_body()` partially moves
//!     // `self`, or `self.decompress` below is a use-after-move.
//!     let cap = self.max_body_bytes;
//!     let decompress = self.decompress;
//!     let encoding = self
//!         .inner
//!         .headers()
//!         .get(http::header::CONTENT_ENCODING)
//!         .and_then(|v| v.to_str().ok())
//!         .unwrap_or("identity")
//!         .to_owned();
//!
//!     // Keep: transport-specific.
//!     let raw = collect_body_limited(self.inner.into_body(), cap).await?;
//!     if !decompress {
//!         return Ok(raw);
//!     }
//!
//!     let limits = oxiarc_http::DecodeLimits::default().with_max_output(cap as u64);
//!     let out = oxiarc_http::decode_body_from_header(&encoding, &raw, &limits)
//!         .map_err(|e| OxiHttpError::Body(e.to_string()))?;
//!     Ok(Bytes::from(out))
//! }
//! ```
//!
//! That also **fixes** `oxihttp`'s multi-member gzip rejection, and adds
//! `br`, `zstd` and `x-gzip`, none of which it handles today.
//!
//! # Server side
//!
//! Replace `compression.rs`'s hand-rolled `negotiate` with
//! [`oxiarc_http::negotiate`] — it inverts client q-value precedence and
//! mishandles `*` — and the `gzip_compress`/`zlib_compress` calls with
//! [`oxiarc_http::negotiate_and_encode`]. Keep `oxihttp`'s
//! `Compression::apply` wrapper: status-code and `Vary` policy is framework
//! glue and belongs there.

use oxiarc_http::{
    ContentCoding, DecodeLimits, EncodeOptions, decode_body_from_header, negotiate_and_encode,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // ── Server side ───────────────────────────────────────────────────────
    let available: Vec<ContentCoding> = [
        ContentCoding::Zstd,
        ContentCoding::Brotli,
        ContentCoding::Gzip,
        ContentCoding::Deflate,
    ]
    .into_iter()
    .filter(ContentCoding::is_encodable)
    .collect();

    let body = "{\"rows\":[".to_string()
        + &(0..500)
            .map(|i| format!("{{\"id\":{i}}},"))
            .collect::<String>()
        + "]}";

    // What the client asked for. `oxihttp` used to answer `gzip` here, which
    // ignores the client's stated preference: deflate's q=0.9 beats gzip's
    // q=0.8.
    let accept = "deflate;q=0.9, gzip;q=0.8";
    let response = negotiate_and_encode(
        Some(accept),
        &available,
        body.as_bytes(),
        EncodeOptions::default(),
    )?;

    let (content_encoding, wire) = match response {
        Some((coding, bytes)) => {
            println!(
                "Accept-Encoding: {accept} -> Content-Encoding: {coding} \
                 ({} bytes from {})",
                bytes.len(),
                body.len()
            );
            println!("Vary: Accept-Encoding          <- do not forget this one");
            (coding.as_str().to_owned(), bytes)
        }
        None => {
            println!("no acceptable coding; sending identity");
            ("identity".to_owned(), body.clone().into_bytes())
        }
    };

    // ── Client side ───────────────────────────────────────────────────────
    let limits = DecodeLimits::default().with_max_output(8 * 1024 * 1024);
    let decoded = decode_body_from_header(&content_encoding, &wire, &limits)?;
    assert_eq!(decoded, body.as_bytes());
    println!("round trip verified: {} bytes", decoded.len());

    // Multi-member gzip: what `oxihttp` rejects today and this accepts.
    let a = oxiarc_deflate::gzip_compress(b"first ", 6)?;
    let b = oxiarc_deflate::gzip_compress(b"second", 6)?;
    let mut multi = a;
    multi.extend_from_slice(&b);
    let decoded = decode_body_from_header("gzip", &multi, &limits)?;
    println!(
        "multi-member gzip decoded to {:?}",
        String::from_utf8_lossy(&decoded)
    );

    Ok(())
}
