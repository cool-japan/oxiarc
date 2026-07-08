//! Fuzz target for raw DEFLATE decompression: `oxiarc_deflate::inflate::inflate`.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // Must never panic on arbitrary (attacker-controlled) input: either a
    // successfully decoded buffer or a structured error is acceptable.
    let _ = oxiarc_deflate::inflate::inflate(data);
});
