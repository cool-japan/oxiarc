//! Fuzz target for Snappy block decompression: `oxiarc_snappy::decompress`.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = oxiarc_snappy::decompress(data);
});
