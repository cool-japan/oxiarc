//! Dictionary-based LZ4 block (de)compression with [`Lz4Dict`].
//!
//! A dictionary primes the encoder/decoder with up to 64 KiB of "shared
//! prefix" data that both sides already agree on, so small payloads that
//! share structure with the dictionary can reference it as match history,
//! improving compression ratio without needing to be large enough to
//! self-reference.
//!
//! Run with:
//! ```sh
//! cargo run -p oxiarc-lz4 --example dict_block
//! ```

use oxiarc_lz4::dict::{Lz4Dict, compress_with_dict, decompress_with_dict};
use oxiarc_lz4::{compress_bytes, decompress_bytes};

fn main() {
    // Shared "common patterns" dictionary both encoder and decoder know.
    let dict_data = b"the quick brown fox jumps over the lazy dog. \
                       error: connection refused. warning: retrying in 1s. "
        .repeat(4);
    let dict = Lz4Dict::new(&dict_data);
    println!(
        "Dictionary: {} bytes, id={:#010x}",
        dict_data.len(),
        dict.id()
    );

    // A short payload that overlaps heavily with the dictionary's vocabulary.
    let payload = b"warning: retrying in 1s. error: connection refused.";

    let plain = compress_bytes(payload).expect("compress_bytes (no dict)");
    let with_dict = compress_with_dict(payload, &dict).expect("compress_with_dict");

    println!(
        "Payload {} bytes -> {} bytes without dict, {} bytes with dict",
        payload.len(),
        plain.len(),
        with_dict.len()
    );

    // Decode both forms back and confirm they agree with the original.
    let decoded_plain = decompress_bytes(&plain, payload.len()).expect("decompress_bytes");
    assert_eq!(decoded_plain, payload);

    let decoded_dict =
        decompress_with_dict(&with_dict, payload.len(), &dict).expect("decompress_with_dict");
    assert_eq!(decoded_dict, payload);

    println!("Round-trip verified for both the plain and dictionary-primed blocks.");
}
