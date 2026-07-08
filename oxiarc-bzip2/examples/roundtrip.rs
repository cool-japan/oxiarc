//! BZip2 compress/decompress round-trip across compression levels 1-9 with
//! [`compress`]/[`decompress`]/[`CompressionLevel`].
//!
//! Run with:
//! ```sh
//! cargo run -p oxiarc-bzip2 --example roundtrip
//! ```

use oxiarc_bzip2::{CompressionLevel, compress, decompress};

fn main() {
    let data =
        "Hello, BZip2! This text repeats to give the BWT stage something to sort. ".repeat(80);
    println!("Input: {} bytes\n", data.len());

    for level in 1..=9u8 {
        let level = CompressionLevel::new(level);
        let compressed = compress(data.as_bytes(), level).expect("compress");
        let decompressed = decompress(&compressed[..]).expect("decompress");
        assert_eq!(decompressed, data.as_bytes());

        println!(
            "level={:>1} (block_size={:>7}): {} bytes -> {} bytes",
            level.level(),
            level.block_size(),
            data.len(),
            compressed.len()
        );
    }

    println!("\nRound-trip verified for every compression level.");
}
