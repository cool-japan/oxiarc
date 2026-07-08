//! Fuzz target for the zlib container: `oxiarc_deflate::zlib::zlib_decompress`.
//!
//! Exercises the 2-byte CMF/FLG header (including the optional preset
//! dictionary flag/id), the wrapped DEFLATE stream, and the trailing
//! Adler-32 checksum.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = oxiarc_deflate::zlib::zlib_decompress(data);
});
