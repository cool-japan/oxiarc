//! Encode and decode a sample array with the AEC/SZIP codec via
//! [`encode`]/[`decode`]/[`SzipParams`].
//!
//! `oxiarc_szip::encode` always emits the no-compression AEC option (real
//! entropy coding is the decoder's job — this crate targets HDF5/CCSDS
//! *decode* interoperability); the round-trip below demonstrates the
//! byte-stream framing (option ID, reference samples, RSI layout) using
//! that no-compression path, which is a valid full AEC/SZIP stream and
//! exercises exactly the same decode logic that would run against a
//! real HDF5-produced SZIP-compressed dataset.
//!
//! Run with:
//! ```sh
//! cargo run -p oxiarc-szip --example sample_decode
//! ```

use oxiarc_szip::{SzipParams, decode, encode};

fn main() {
    let params = SzipParams {
        bits_per_pixel: 16,
        pixels_per_block: 16,
        samples: 256,
        reference_sample_interval: 16 * 4, // one reference sample per 4 blocks
        msb: true,
        nn_preprocess: false,
        rsi_byte_align: false,
    };
    params.validate().expect("params should validate");

    // Synthetic 16-bit sample stream (e.g. stand-in for a scientific
    // instrument's raw sensor readings).
    let samples: Vec<u64> = (0..params.samples as u64).map(|i| i * 37 % 65536).collect();

    let encoded = encode(&samples, &params).expect("encode");
    println!(
        "Encoded {} samples ({} bits/sample) into {} AEC bytes",
        samples.len(),
        params.bits_per_pixel,
        encoded.len()
    );

    let raw_bytes = decode(&encoded, &params).expect("decode");
    let bytes_per_sample = params.bytes_per_sample();
    assert_eq!(raw_bytes.len(), samples.len() * bytes_per_sample);

    // Reconstruct u64 samples from the big-endian packed byte output and
    // confirm the round-trip matches the original samples exactly.
    let decoded: Vec<u64> = raw_bytes
        .chunks_exact(bytes_per_sample)
        .map(|chunk| {
            chunk
                .iter()
                .fold(0u64, |acc, &byte| (acc << 8) | byte as u64)
        })
        .collect();

    assert_eq!(decoded, samples);
    println!("Round-trip verified: all {} samples match.", samples.len());
}
