//! Fuzz target for ZIP archive parsing: `oxiarc_archive::zip::read_zip`.
//!
//! Exercises end-of-central-directory / central-directory-record parsing
//! and, for every entry found, both metadata access and full extraction
//! (which in turn drives whichever compression method the entry claims:
//! stored, deflate, bzip2, LZMA, etc.).
#![no_main]

use libfuzzer_sys::fuzz_target;
use std::io::Cursor;

fuzz_target!(|data: &[u8]| {
    let cursor = Cursor::new(data);
    let Ok(mut reader) = oxiarc_archive::zip::read_zip(cursor) else {
        return;
    };

    let entry_count = reader.entries().len();
    for index in 0..entry_count {
        let Some(entry) = reader.entries().get(index).cloned() else {
            continue;
        };
        let _ = reader.extract(&entry);
    }
});
