//! Custom-configured LZMA2 chunked encoding with [`Lzma2ChunkedEncoder`].
//!
//! The free functions [`encode_lzma2_chunked`]/[`decode_lzma2_chunked`] use a
//! fixed default chunk size large enough that typical example-sized inputs
//! fit in a single chunk. [`Lzma2ChunkedEncoder`] (built from an explicit
//! [`Lzma2Config`]) lets a caller force a much smaller
//! uncompressed-chunk-size boundary so that a modest input is actually split
//! across *multiple* LZMA2 chunks, each with its own state/dictionary reset
//! flags — the code path exercised by real multi-chunk XZ blocks.
//!
//! Note: this demo intentionally uses highly repetitive input. Splitting
//! more varied/high-entropy data across many small chunks currently hits a
//! known decode edge case in this pre-1.0 crate's multi-chunk state
//! management (tracked separately; out of scope for this example), so
//! sticking to compressible repeated-byte payloads (as the crate's own
//! chunked-encoder test suite does) keeps this example reliable.
//!
//! Run with:
//! ```sh
//! cargo run -p oxiarc-lzma --example lzma2_chunked
//! ```

use oxiarc_lzma::{Lzma2ChunkedEncoder, Lzma2Config, LzmaLevel, decode_lzma2_chunked};

fn main() {
    let data = vec![b'M'; 200_000];
    println!("Input: {} highly-compressible bytes", data.len());

    for chunk_size in [4 * 1024usize, 16 * 1024, 64 * 1024] {
        let config = Lzma2Config::with_level(LzmaLevel::DEFAULT).chunk_size(chunk_size);
        let mut encoder = Lzma2ChunkedEncoder::with_config(config);

        let encoded = encoder.encode(&data).expect("encoder.encode");
        let expected_chunks = data.len().div_ceil(chunk_size);

        // A generous dictionary size is always safe for decoding (it only
        // affects the scratch buffer allocated up front).
        let decoded = decode_lzma2_chunked(&encoded, 1 << 20).expect("decode_lzma2_chunked");

        assert_eq!(decoded, data);
        println!(
            "chunk_size={chunk_size:>6} (~{expected_chunks} chunks): {} bytes -> {} bytes",
            data.len(),
            encoded.len()
        );
    }

    println!("Round-trip verified for all chunk sizes.");
}
