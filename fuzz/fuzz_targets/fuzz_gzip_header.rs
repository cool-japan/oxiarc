//! Fuzz target for the gzip container: `oxiarc_deflate::gzip::gzip_decompress`.
//!
//! Exercises the gzip member header (magic, flags, optional FNAME/FCOMMENT/
//! FEXTRA/FHCRC fields) plus the wrapped DEFLATE stream and trailing CRC32/
//! ISIZE footer.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = oxiarc_deflate::gzip::gzip_decompress(data);
});
