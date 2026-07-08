//! Fuzz target for ISO 9660 (+ Joliet) filesystem parsing:
//! `oxiarc_archive::iso9660::IsoReader`.
#![no_main]

use libfuzzer_sys::fuzz_target;
use oxiarc_archive::iso9660::IsoReader;
use std::io::{Cursor, Write};

/// A `Write` sink that discards everything, so extraction can run its full
/// decode path without needing a real destination.
struct Sink;

impl Write for Sink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fuzz_target!(|data: &[u8]| {
    let cursor = Cursor::new(data);
    let Ok(mut reader) = IsoReader::new(cursor) else {
        return;
    };

    let entry_count = reader.entries().len();
    for index in 0..entry_count {
        let Some(entry) = reader.entries().get(index).cloned() else {
            continue;
        };
        let mut sink = Sink;
        let _ = reader.extract(&entry, &mut sink);
    }
});
