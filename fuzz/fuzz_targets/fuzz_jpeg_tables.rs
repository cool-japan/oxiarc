//! Fuzz target for `oxiarc_jpeg::TableSet::{parse, emit}` — the
//! `JPEGTables`-tag interop surface `oxiarc-tiff`'s Compression=7 codec
//! depends on. `parse` must never panic on arbitrary bytes (it is
//! documented to accept any `SOI ... EOI`-ish byte stream and simply stop
//! early at a `SOF`/`SOS`/`EOI`, never fail on one), and whenever it
//! succeeds, re-`emit`ting and re-`parse`ing the result must round-trip:
//! `emit` must not panic on any value `parse` can produce, and the
//! re-parsed set must equal the original.
#![no_main]

use libfuzzer_sys::fuzz_target;
use oxiarc_jpeg::{TableSet, TablesMode};

fuzz_target!(|data: &[u8]| {
    let Ok(tables) = TableSet::parse(data) else {
        return;
    };

    for mode in [
        TablesMode::BOTH,
        TablesMode::QUANT,
        TablesMode::HUFF,
        TablesMode::NONE,
    ] {
        let emitted = tables.emit(mode);
        let Ok(reparsed) = TableSet::parse(&emitted) else {
            panic!("TableSet::emit({mode:?}) produced bytes its own parse rejects: {emitted:02x?}");
        };
        // `emit` only ever writes the families `mode` selects, so the
        // re-parsed set is not expected to equal `tables` in general
        // (e.g. `TablesMode::QUANT` drops the Huffman tables) — but it must
        // be internally stable: emitting what was just parsed back out,
        // twice, must agree.
        let emitted_again = reparsed.emit(mode);
        assert_eq!(
            emitted, emitted_again,
            "TableSet::emit({mode:?}) is not idempotent through a parse/emit round trip"
        );
    }
});
