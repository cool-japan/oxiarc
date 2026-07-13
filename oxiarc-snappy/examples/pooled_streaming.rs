//! Pooled Snappy framed streaming with [`SnappyPool`].
//!
//! Repeatedly encoding/decoding small frames allocates scratch buffers on
//! every call. [`SnappyPool`] lets `FrameEncoder`/`FrameDecoder` (and the
//! [`compress_frame_pooled`] convenience function) reuse those buffers
//! across many operations instead of allocating fresh ones each time.
//! [`SnappyPool::stats`] reports how often the pool actually served a
//! cache hit versus falling back to a fresh allocation.
//!
//! Run with:
//! ```sh
//! cargo run -p oxiarc-snappy --example pooled_streaming
//! ```

use oxiarc_snappy::{FrameDecoder, FrameEncoder, SnappyPool, compress_frame_pooled};
use std::io::{Read, Write};

fn main() {
    let pool = SnappyPool::with_cap(8);
    let messages: Vec<Vec<u8>> = (0..20)
        .map(|i| format!("message #{i}: hello from the pooled Snappy stream!").into_bytes())
        .collect();

    // ---- Pooled framed writer/reader, one frame stream per message -------
    for msg in &messages {
        let mut compressed = Vec::new();
        {
            let mut encoder = FrameEncoder::with_pool(&mut compressed, &pool);
            encoder.write_all(msg).expect("write_all");
            encoder.finish().expect("finish");
        }

        let mut decoder = FrameDecoder::with_pool(compressed.as_slice(), &pool);
        let mut output = Vec::new();
        decoder.read_to_end(&mut output).expect("read_to_end");
        assert_eq!(&output, msg);
    }

    let stats_after_streams = pool.stats();
    println!(
        "After {} streamed messages: encoder scratch {} hits / {} allocs, decoder scratch {} hits / {} allocs",
        messages.len(),
        stats_after_streams.encoder_scratch_hits,
        stats_after_streams.encoder_scratch_allocations,
        stats_after_streams.decoder_scratch_hits,
        stats_after_streams.decoder_scratch_allocations,
    );

    // ---- One-shot pooled compression convenience function -----------------
    for msg in &messages {
        let compressed = compress_frame_pooled(msg, &pool).expect("compress_frame_pooled");
        let mut decoder = FrameDecoder::with_pool(compressed.as_slice(), &pool);
        let mut output = Vec::new();
        decoder.read_to_end(&mut output).expect("read_to_end");
        assert_eq!(&output, msg);
    }

    let final_stats = pool.stats();
    println!(
        "After also using compress_frame_pooled: encoder scratch {} hits / {} allocs, decoder scratch {} hits / {} allocs",
        final_stats.encoder_scratch_hits,
        final_stats.encoder_scratch_allocations,
        final_stats.decoder_scratch_hits,
        final_stats.decoder_scratch_allocations,
    );

    assert!(
        final_stats.encoder_scratch_hits > 0,
        "expected the pool to serve at least one reused encoder buffer"
    );
    println!("Pool reuse confirmed: subsequent operations hit the pool instead of allocating.");
}
