//! Fuzz target for Brotli decompression: `oxiarc_brotli::decompress`.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = oxiarc_brotli::decompress(data);
});
