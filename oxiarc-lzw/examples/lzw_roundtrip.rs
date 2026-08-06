//! LZW compress/decompress round-trip in both supported bitstream
//! configurations — [`LzwConfig::TIFF`] (MSB-first, early code change) and
//! [`LzwConfig::GIF`] (LSB-first) — with the generic [`compress`]/
//! [`decompress`] functions.
//!
//! Run with:
//! ```sh
//! cargo run -p oxiarc-lzw --example lzw_roundtrip
//! ```

use oxiarc_lzw::{LzwConfig, compress, decompress};

fn main() {
    let data = b"TOBEORNOTTOBEORTOBEORNOT".repeat(40);
    println!("Input: {} bytes\n", data.len());

    for (name, config) in [("TIFF", LzwConfig::TIFF), ("GIF", LzwConfig::GIF)] {
        let compressed = compress(&data, config).expect("compress");
        let decompressed = decompress(&compressed, data.len(), config).expect("decompress");
        assert_eq!(decompressed, data);

        println!(
            "{name:>4}: {} bytes -> {} bytes ({:.1}% of original)",
            data.len(),
            compressed.len(),
            100.0 * compressed.len() as f64 / data.len() as f64
        );
    }

    println!("\nRound-trip verified for both TIFF and GIF LZW configurations.");
}
