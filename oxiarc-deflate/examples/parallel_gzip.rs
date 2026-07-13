//! Multi-member (pigz-style) parallel GZIP compression using
//! [`ParallelGzipEncoder`] / [`compress_gzip_parallel`].
//!
//! Splits input into independently-compressed chunks (encoded across a
//! rayon thread pool) and concatenates the resulting GZIP members into a
//! single valid multi-member stream, which any standard GZIP decoder
//! (including this crate's own [`oxiarc_deflate::gzip_decompress`]) reads
//! back as one continuous byte stream.
//!
//! Requires the `parallel` feature:
//! ```sh
//! cargo run -p oxiarc-deflate --example parallel_gzip --features parallel
//! ```

#[cfg(feature = "parallel")]
fn main() {
    use oxiarc_deflate::{
        GzipStreamDecoder, ParallelGzipEncoder, compress_gzip_parallel, gzip_decompress,
    };
    use std::io::Read;

    // Build input large enough to span several parallel chunks.
    let input: Vec<u8> = (0..8)
        .flat_map(|i| {
            format!("chunk-{i}: the quick brown fox jumps over the lazy dog\n").into_bytes()
        })
        .collect::<Vec<u8>>()
        .repeat(200);
    println!("Input: {} bytes", input.len());

    // ---- One-shot convenience function ---------------------------------
    let compressed = compress_gzip_parallel(&input, 6).expect("compress_gzip_parallel");
    println!(
        "compress_gzip_parallel: {} bytes -> {} bytes",
        input.len(),
        compressed.len()
    );
    let decompressed = gzip_decompress(&compressed).expect("gzip_decompress");
    assert_eq!(decompressed, input);

    // ---- Builder API with explicit chunk size / thread count ----------
    // `ParallelGzipEncoder` emits a true pigz-style *multi-member* GZIP
    // stream (one independent member per chunk), so it must be read back
    // with `GzipStreamDecoder` (which concatenates members) rather than the
    // single-member `gzip_decompress`.
    let encoder = ParallelGzipEncoder::new()
        .level(6)
        .chunk_size(4096)
        .num_threads(2);
    let compressed_builder = encoder.encode(&input).expect("ParallelGzipEncoder::encode");
    println!(
        "ParallelGzipEncoder (chunk_size=4096, threads=2): {} bytes -> {} bytes",
        input.len(),
        compressed_builder.len()
    );
    let mut decoder = GzipStreamDecoder::new(compressed_builder.as_slice());
    let mut decompressed_builder = Vec::new();
    decoder
        .read_to_end(&mut decompressed_builder)
        .expect("GzipStreamDecoder read_to_end");
    assert_eq!(decompressed_builder, input);

    println!("Round-trip verified for both parallel GZIP encoding paths.");
}

#[cfg(not(feature = "parallel"))]
fn main() {
    eprintln!(
        "This example requires the `parallel` feature. Run with:\n  \
         cargo run -p oxiarc-deflate --example parallel_gzip --features parallel"
    );
}
