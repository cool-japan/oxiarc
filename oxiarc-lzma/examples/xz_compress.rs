//! LZMA2 chunked encoding — the XZ-compatible payload format.
//!
//! `oxiarc-lzma` intentionally does **not** implement the full `.xz`
//! container (magic header, stream flags, index, footer, integrity checks);
//! that container format lives in `oxiarc-archive::xz` (which depends on
//! this crate). What `oxiarc-lzma` *does* provide is the LZMA2 chunked
//! encoding used **inside** every XZ block: [`encode_lzma2_chunked`] /
//! [`decode_lzma2_chunked`] produce and consume exactly the byte format an
//! XZ block payload contains.
//!
//! For a full, self-contained `.xz` file, see
//! `oxiarc-archive`'s `XzWriter`/`XzReader` (which wrap this crate's LZMA2
//! chunked codec with the surrounding container framing).
//!
//! Run with:
//! ```sh
//! cargo run -p oxiarc-lzma --example xz_compress
//! ```

use oxiarc_lzma::{LzmaLevel, decode_lzma2_chunked, encode_lzma2_chunked};

fn main() {
    let data = "The quick brown fox jumps over the lazy dog. ".repeat(200);

    for level in [LzmaLevel::FAST, LzmaLevel::DEFAULT, LzmaLevel::BEST] {
        let encoded = encode_lzma2_chunked(data.as_bytes(), level).expect("encode_lzma2_chunked");

        // The dictionary size used for decoding must be >= the one implied
        // by the encoding level (using the encoder's own level->dict_size
        // mapping keeps caller and encoder in agreement).
        let decoded =
            decode_lzma2_chunked(&encoded, level.dict_size()).expect("decode_lzma2_chunked");

        assert_eq!(decoded, data.as_bytes());
        println!(
            "level {:>2}: {} bytes -> {} bytes (dict_size={})",
            level.level(),
            data.len(),
            encoded.len(),
            level.dict_size()
        );
    }

    println!("Round-trip verified for all three levels.");
}
