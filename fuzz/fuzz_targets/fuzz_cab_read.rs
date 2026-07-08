//! Fuzz target for Microsoft CAB archive parsing: `oxiarc_archive::cab::CabReader`.
#![no_main]

use libfuzzer_sys::fuzz_target;
use oxiarc_archive::cab::CabReader;
use std::io::Cursor;

fuzz_target!(|data: &[u8]| {
    let cursor = Cursor::new(data);
    let Ok(mut reader) = CabReader::new(cursor) else {
        return;
    };

    let entry_count = reader.entries().len();
    for index in 0..entry_count {
        let _ = reader.extract_by_index(index);
    }
});
