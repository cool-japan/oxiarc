//! Fuzz target for bzip2 decompression: `oxiarc_bzip2::decode::decompress`
//! (the `decode` module is private; the function is re-exported at the
//! crate root as `oxiarc_bzip2::decompress`).
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // `decompress` takes any `Read`; a byte slice implements it directly.
    let _ = oxiarc_bzip2::decompress(data);
});
