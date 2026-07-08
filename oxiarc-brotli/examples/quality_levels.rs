//! Compare Brotli quality levels (0-11) with [`compress`]/[`decompress`] and
//! the tunable [`BrotliParams`]/[`compress_with_params`] API.
//!
//! Higher quality trades encode time for a better compression ratio; this
//! demo shows the ratio trend across the full quality range plus a custom
//! `BrotliParams` window/block-size configuration.
//!
//! Run with:
//! ```sh
//! cargo run -p oxiarc-brotli --example quality_levels
//! ```

use oxiarc_brotli::{BrotliParams, compress, compress_with_params, decompress};

fn main() {
    let data = "The quick brown fox jumps over the lazy dog. ".repeat(200);
    println!("Input: {} bytes\n", data.len());

    for quality in [0u32, 1, 4, 6, 9, 11] {
        let compressed = compress(data.as_bytes(), quality).expect("compress");
        let decompressed = decompress(&compressed).expect("decompress");
        assert_eq!(decompressed, data.as_bytes());

        let ratio = data.len() as f64 / compressed.len() as f64;
        println!(
            "quality={quality:>2}: {} bytes -> {} bytes (ratio {ratio:.2}x)",
            data.len(),
            compressed.len()
        );
    }

    // Custom parameters: max quality, larger window for long-range matches.
    let params = BrotliParams {
        quality: 11,
        lgwin: 24,
        ..BrotliParams::default()
    };
    params.validate().expect("params should validate");
    let compressed = compress_with_params(data.as_bytes(), &params).expect("compress_with_params");
    let decompressed = decompress(&compressed).expect("decompress");
    assert_eq!(decompressed, data.as_bytes());
    println!(
        "\ncustom params (quality=11, lgwin=24, window={} bytes): {} bytes -> {} bytes",
        params.window_size(),
        data.len(),
        compressed.len()
    );

    println!("\nRound-trip verified for every configuration.");
}
