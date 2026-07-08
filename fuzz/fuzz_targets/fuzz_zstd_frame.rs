//! Fuzz target for zstd frame decoding: `oxiarc_zstd::decompress`.
//!
//! Covers frame header parsing (magic, descriptor, window/dict-id/content
//! size fields) and the compressed-block pipeline (literals, sequences, FSE).
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = oxiarc_zstd::decompress(data);
});
