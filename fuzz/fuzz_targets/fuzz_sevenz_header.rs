//! Fuzz target for 7z archive header/entry parsing:
//! `oxiarc_archive::sevenz::SevenZReader`.
//!
//! Exercises the 7z signature header, the (optionally LZMA/LZMA2/BCJ
//! filter-encoded) end-of-archive header, and folder/coder decoding for
//! every entry found.
#![no_main]

use libfuzzer_sys::fuzz_target;
use oxiarc_archive::sevenz::SevenZReader;
use std::io::Cursor;

fuzz_target!(|data: &[u8]| {
    let cursor = Cursor::new(data);
    let Ok(mut reader) = SevenZReader::new(cursor) else {
        return;
    };

    let entry_count = reader.entries().len();
    for index in 0..entry_count {
        let _ = reader.extract(index);
    }
});
