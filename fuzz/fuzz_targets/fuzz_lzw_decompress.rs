//! Fuzz target for LZW decompression: `oxiarc_lzw::decompress`.
//!
//! `decompress` needs an `expected_size` and an `LzwConfig` alongside the
//! compressed bytes. The fuzz input's first 4 bytes (little-endian, masked
//! to a sane ceiling) become `expected_size`, the 5th byte selects between
//! the TIFF and GIF bit-stream configurations (the two real presets used by
//! this crate's own format decoders), and the remainder is the compressed
//! payload.
#![no_main]

use arbitrary::Unstructured;
use libfuzzer_sys::fuzz_target;
use oxiarc_lzw::LzwConfig;

fuzz_target!(|data: &[u8]| {
    let mut unstructured = Unstructured::new(data);

    let Ok(raw_size) = unstructured.arbitrary::<u32>() else {
        return;
    };
    let Ok(config_tag) = unstructured.arbitrary::<u8>() else {
        return;
    };

    // Cap the expected output size so a bogus header value can't force an
    // unbounded allocation attempt.
    let expected_size = (raw_size % (16 * 1024 * 1024)) as usize;
    let config = if config_tag % 2 == 0 {
        LzwConfig::TIFF
    } else {
        LzwConfig::GIF
    };

    let payload = unstructured.take_rest();
    let _ = oxiarc_lzw::decompress(payload, expected_size, config);
});
