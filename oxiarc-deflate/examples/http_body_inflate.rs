//! Decoding an HTTP response body with [`InflateReader`].
//!
//! An HTTP client hands you a body one network chunk at a time and tells
//! you how it was encoded (`Content-Encoding: gzip`, `deflate`, `x-gzip`).
//! [`InflateReader`] is exactly that shape: a `Read` over a `Read`,
//! parameterised by the framing, with a bounded memory footprint no matter
//! how large the response is.
//!
//! This example uses a `ChunkedSource` that hands out one small chunk per
//! `read` — including a `WouldBlock` in the middle, as a non-blocking
//! socket would — instead of a real connection, so it runs offline.
//!
//! Run with:
//! ```sh
//! cargo run -p oxiarc-deflate --example http_body_inflate
//! ```

use oxiarc_deflate::{InflateReader, InflateWrapper, gzip_compress};
use std::io::{self, Read};

/// A stand-in for a network body: hands out one chunk per `read`, and
/// reports `WouldBlock` once in the middle to prove the reader propagates
/// it rather than mistaking it for end-of-stream.
struct ChunkedSource {
    chunks: Vec<Vec<u8>>,
    next: usize,
    stall_at: usize,
    stalled: bool,
}

impl ChunkedSource {
    fn new(body: &[u8], chunk_size: usize, stall_at: usize) -> Self {
        Self {
            chunks: body.chunks(chunk_size).map(<[u8]>::to_vec).collect(),
            next: 0,
            stall_at,
            stalled: false,
        }
    }
}

impl Read for ChunkedSource {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.next == self.stall_at && !self.stalled {
            self.stalled = true;
            return Err(io::Error::new(io::ErrorKind::WouldBlock, "not ready yet"));
        }
        let Some(chunk) = self.chunks.get(self.next) else {
            return Ok(0); // end of body
        };
        let n = buf.len().min(chunk.len());
        buf[..n].copy_from_slice(&chunk[..n]);
        if n == chunk.len() {
            self.next += 1;
        } else {
            self.chunks[self.next].drain(..n);
        }
        Ok(n)
    }
}

/// Map a `Content-Encoding` token onto the framing to decode it with.
fn framing_for(content_encoding: &str) -> Option<InflateWrapper> {
    match content_encoding.trim().to_ascii_lowercase().as_str() {
        "gzip" | "x-gzip" => Some(InflateWrapper::Gzip),
        // RFC 9110 says `deflate` is zlib-framed, but deployed servers send
        // raw DEFLATE too, so sniff the first two bytes.
        "deflate" => Some(InflateWrapper::Auto),
        "identity" => None,
        other => panic!("unsupported Content-Encoding: {other}"),
    }
}

fn main() {
    // A response body a server would actually gzip: repetitive JSON
    // scaffolding around values that do not compress away, so the wire form
    // is several TCP segments long.
    let mut body = Vec::new();
    let mut state = 0x2545_f491_4f6c_dd1du64;
    for id in 0..2_000u32 {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        body.extend_from_slice(
            format!(
                "{{\"id\":{id},\"token\":\"{:016x}\",\"ok\":true}},\n",
                state
            )
            .as_bytes(),
        );
    }
    let wire = gzip_compress(&body, 6).expect("gzip_compress");
    println!(
        "server sent {} bytes for a {} byte body ({:.1}x)",
        wire.len(),
        body.len(),
        body.len() as f64 / wire.len() as f64
    );

    let Some(framing) = framing_for("gzip") else {
        println!("identity encoding: nothing to decode");
        return;
    };

    // 1400-byte chunks, i.e. roughly one TCP segment each, with a
    // WouldBlock before the third.
    let source = ChunkedSource::new(&wire, 1400, 2);

    // Bound what a hostile response can cost us: at most 8 MiB of output,
    // and no more than 200x expansion once past the first 64 KiB.
    let mut reader = InflateReader::new(source, framing)
        .with_max_output(8 * 1024 * 1024)
        .with_ratio_guard(200.0, 64 * 1024);

    let mut decoded = Vec::new();
    let mut buf = [0u8; 8192];
    let mut stalls = 0usize;
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => decoded.extend_from_slice(&buf[..n]),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                // A real client would return to its event loop here and
                // poll again when the socket is readable. The reader keeps
                // every byte it has already staged.
                stalls += 1;
                continue;
            }
            Err(error) => panic!("body decode failed: {error}"),
        }
    }

    assert_eq!(decoded, body, "decoded body must match");
    println!(
        "decoded {} bytes after {} WouldBlock stall(s)",
        decoded.len(),
        stalls
    );
    println!(
        "compressed bytes consumed: {}, members: {}",
        reader.total_in(),
        reader.members_decoded()
    );
    if let Some(header) = reader.gzip_header() {
        println!("gzip OS byte: {}, mtime: {}", header.os, header.mtime);
    }

    // Peak memory is fixed: 64 KiB of compressed staging, 64 KiB of decoded
    // staging and the 32 KiB LZ77 history — for a body of any size.
    println!("peak decoder memory: 64 KiB in + 64 KiB out + 32 KiB history");

    // A truncated body is an error, not a short read: a client must not
    // hand a half-decoded JSON document to its caller.
    let cut = &wire[..wire.len() / 2];
    let mut truncated = InflateReader::new(cut, framing);
    let mut sink = Vec::new();
    match truncated.read_to_end(&mut sink) {
        Ok(_) => panic!("a truncated body must not decode cleanly"),
        Err(error) => println!("truncated body correctly rejected: {}", error.kind()),
    }
}
