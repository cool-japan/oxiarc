//! Fuzz target for TAR archive parsing: `oxiarc_archive::tar::TarReader`.
//!
//! Exercises header parsing (ustar/GNU/PAX variants, sparse extensions,
//! long-name/long-link records) and full extraction for every entry found.
#![no_main]

use libfuzzer_sys::fuzz_target;
use oxiarc_archive::tar::TarReader;
use std::io::Cursor;

fuzz_target!(|data: &[u8]| {
    let cursor = Cursor::new(data);
    let Ok(mut reader) = TarReader::new(cursor) else {
        return;
    };

    let entry_count = reader.entries().len();
    for index in 0..entry_count {
        let Some(entry) = reader.entries().get(index).cloned() else {
            continue;
        };
        let _ = reader.extract_to_vec(&entry);
    }
});
