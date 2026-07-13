//! Fuzz target for LZMA1 decompression: `oxiarc_lzma::decompress_bytes`.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = oxiarc_lzma::decompress_bytes(data);
});
