//! Streaming GZIP compression/decompression with [`GzipStreamEncoder`] and
//! [`GzipStreamDecoder`].
//!
//! Unlike [`gzip_compress`]/[`gzip_decompress`] (which operate on whole
//! in-memory buffers), the streaming encoder/decoder implement
//! `std::io::Write`/`std::io::Read` so data can be fed and consumed
//! incrementally, with `sync_flush` available to force a flush point
//! mid-stream (useful for network protocols).
//!
//! Run with:
//! ```sh
//! cargo run -p oxiarc-deflate --example gzip_streaming
//! ```

use oxiarc_deflate::{GzipStreamDecoder, GzipStreamEncoder};
use std::io::{Read, Write};

fn main() {
    let chunks: Vec<Vec<u8>> = vec![
        b"Hello, ".to_vec(),
        b"streaming ".to_vec(),
        b"GZIP ".to_vec(),
        b"world!\n".repeat(50),
    ];
    let full_input: Vec<u8> = chunks.iter().flatten().copied().collect();

    // ---- Stream-compress in chunks, forcing a flush after the second one ----
    let mut compressed = Vec::new();
    {
        let mut encoder = GzipStreamEncoder::new(&mut compressed, 6);
        for (i, chunk) in chunks.iter().enumerate() {
            encoder.write_all(chunk).expect("write_all chunk");
            if i == 1 {
                encoder.sync_flush().expect("sync_flush");
                println!(
                    "After flush point: {} bytes buffered internally",
                    encoder.buffered_bytes()
                );
            }
        }
        encoder.finish().expect("finish encoder");
    }
    println!(
        "Compressed {} input bytes into {} GZIP bytes across {} write() calls",
        full_input.len(),
        compressed.len(),
        chunks.len()
    );

    // ---- Stream-decompress and verify ----------------------------------------
    let mut decoder = GzipStreamDecoder::new(compressed.as_slice());
    let mut output = Vec::new();
    decoder.read_to_end(&mut output).expect("read_to_end");

    assert_eq!(output, full_input);
    println!("Round-trip verified: {} bytes match.", output.len());
}
