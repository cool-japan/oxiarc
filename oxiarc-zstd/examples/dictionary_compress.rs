//! Dictionary-based Zstandard compression for small, similarly-structured
//! payloads (e.g. JSON log lines) using [`ZstdDict`]/[`train_dictionary`].
//!
//! Small inputs compress poorly on their own because there is not enough
//! data for the encoder to build good entropy tables. A trained dictionary
//! captures common structure across many *samples* so a single small input
//! can reference it instead of paying for its own tables from scratch.
//!
//! Run with:
//! ```sh
//! cargo run -p oxiarc-zstd --example dictionary_compress
//! ```

use oxiarc_zstd::{ZstdEncoder, decompress_with_dict, train_dictionary};

fn main() {
    // Training samples: short, similarly-shaped JSON log records.
    let samples: Vec<Vec<u8>> = (0..64)
        .map(|i| {
            format!(r#"{{"ts":{i},"level":"info","service":"oxiarc","msg":"request handled ok"}}"#)
                .into_bytes()
        })
        .collect();
    let sample_refs: Vec<&[u8]> = samples.iter().map(|s| s.as_slice()).collect();

    let dict = train_dictionary(&sample_refs, 4096).expect("train_dictionary");
    println!(
        "Trained dictionary: id={:#010x}, {} bytes",
        dict.id(),
        dict.len()
    );

    // A brand-new small payload sharing the trained structure/vocabulary.
    let payload =
        br#"{"ts":9999,"level":"info","service":"oxiarc","msg":"request handled ok"}"#.to_vec();

    // Compress without a dictionary.
    let mut plain_encoder = ZstdEncoder::new();
    plain_encoder.set_level(19);
    let plain = plain_encoder
        .compress(&payload)
        .expect("compress (no dict)");

    // Compress with the trained dictionary.
    let mut dict_encoder = ZstdEncoder::new();
    dict_encoder.set_level(19);
    dict_encoder.set_dictionary(dict.data());
    let with_dict = dict_encoder
        .compress(&payload)
        .expect("compress (with dict)");

    println!(
        "Payload {} bytes -> {} bytes without dict, {} bytes with dict",
        payload.len(),
        plain.len(),
        with_dict.len()
    );

    let decoded = decompress_with_dict(&with_dict, dict.data()).expect("decompress_with_dict");
    assert_eq!(decoded, payload);
    println!("Round-trip verified using the trained dictionary.");
}
