//! Fuzz target for LZ4 frame decompression: `oxiarc_lz4::decompress`.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // Cap the requested output buffer so a malicious/garbled frame header
    // cannot force an unbounded allocation; this mirrors how a real caller
    // would clamp `max_output` to some sane ceiling.
    const MAX_OUTPUT: usize = 16 * 1024 * 1024;
    let _ = oxiarc_lz4::decompress(data, MAX_OUTPUT);
});
