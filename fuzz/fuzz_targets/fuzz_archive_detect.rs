//! HIGH PRIORITY fuzz target: `oxiarc_archive::detect::ArchiveFormat::detect`.
//!
//! This is the very first code path any untrusted file/byte-stream hits
//! before OxiArc even decides which format-specific parser to hand it to,
//! so it must never panic on arbitrary input, regardless of how short,
//! truncated, or malformed that input is.
#![no_main]

use libfuzzer_sys::fuzz_target;
use oxiarc_archive::detect::ArchiveFormat;
use std::io::Cursor;

fuzz_target!(|data: &[u8]| {
    let mut cursor = Cursor::new(data);
    let _ = ArchiveFormat::detect(&mut cursor);

    // `from_magic` is a lower-level, infallible sibling that must likewise
    // never panic regardless of how few bytes it is given.
    let _ = ArchiveFormat::from_magic(data);
});
