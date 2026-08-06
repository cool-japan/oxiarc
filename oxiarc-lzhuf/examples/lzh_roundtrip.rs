//! LZH compress/decompress round-trip across every implemented method
//! (lh0/lh1/lh4/lh5/lh6/lh7) with [`encode_lzh`]/[`decode_lzh`].
//!
//! Run with:
//! ```sh
//! cargo run -p oxiarc-lzhuf --example lzh_roundtrip
//! ```

use oxiarc_lzhuf::{LzhMethod, decode_lzh, encode_lzh};

fn main() {
    let data = "The quick brown fox jumps over the lazy dog. ".repeat(200);
    println!("Input: {} bytes\n", data.len());

    let methods = [
        LzhMethod::Lh0,
        LzhMethod::Lh1,
        LzhMethod::Lh4,
        LzhMethod::Lh5,
        LzhMethod::Lh6,
        LzhMethod::Lh7,
    ];

    for method in methods {
        let compressed = encode_lzh(data.as_bytes(), method).expect("encode_lzh");
        let decompressed = decode_lzh(&compressed, method, data.len() as u64).expect("decode_lzh");
        assert_eq!(decompressed, data.as_bytes());

        println!(
            "{method:?}: {} bytes -> {} bytes ({:.1}% of original)",
            data.len(),
            compressed.len(),
            100.0 * compressed.len() as f64 / data.len() as f64
        );
    }

    println!("\nRound-trip verified for every implemented LZH method.");
}
