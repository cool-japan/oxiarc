//! Fuzz target for the AEC/SZIP decoder: `oxiarc_szip::decode`.
//!
//! `decode` needs an `SzipParams` describing the bit layout alongside the
//! compressed bytes, so a handful of header bytes are consumed to build a
//! (still-adversarial, but structurally sane) parameter set, and the
//! remainder is handed to the decoder as the compressed stream.
#![no_main]

use arbitrary::Unstructured;
use libfuzzer_sys::fuzz_target;
use oxiarc_szip::SzipParams;

fuzz_target!(|data: &[u8]| {
    let mut unstructured = Unstructured::new(data);

    let Ok(bits_tag) = unstructured.arbitrary::<u8>() else {
        return;
    };
    let Ok(block_tag) = unstructured.arbitrary::<u8>() else {
        return;
    };
    let Ok(samples_raw) = unstructured.arbitrary::<u16>() else {
        return;
    };
    let Ok(rsi_tag) = unstructured.arbitrary::<u8>() else {
        return;
    };
    let Ok(flags) = unstructured.arbitrary::<u8>() else {
        return;
    };

    // bits_per_pixel: 1..=32 (the decoder documents this valid range).
    let bits_per_pixel = (bits_tag % 32) + 1;
    // pixels_per_block must be a power of 2 in {8, 16, 32}.
    let pixels_per_block = match block_tag % 3 {
        0 => 8,
        1 => 16,
        _ => 32,
    };
    // Cap sample count so a bogus header can't force an unbounded
    // allocation attempt in the decoder's output buffer.
    let samples = (samples_raw as usize) % 65_536;
    let reference_sample_interval = u32::from(rsi_tag) * pixels_per_block;

    let params = SzipParams {
        bits_per_pixel,
        pixels_per_block,
        samples,
        reference_sample_interval,
        msb: flags & 0b001 != 0,
        nn_preprocess: flags & 0b010 != 0,
        rsi_byte_align: flags & 0b100 != 0,
    };

    let payload = unstructured.take_rest();
    let _ = oxiarc_szip::decode(payload, &params);
});
