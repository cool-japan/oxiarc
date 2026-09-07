//! Fuzz target for whole-buffer PNG decoding: `oxiarc_png::decode` and the
//! pull `Decoder`/`Reader` pair it is built on. Must never panic on
//! arbitrary bytes, however malformed — a successfully decoded image or a
//! structured [`oxiarc_png::DecodingError`] are the only acceptable
//! outcomes.
#![no_main]

use libfuzzer_sys::fuzz_target;
use oxiarc_png::Decoder;
use std::io::Cursor;

fuzz_target!(|data: &[u8]| {
    // The whole-buffer convenience, which allocates the frame eagerly (but
    // only after `checked_output_buffer_size` has validated it against
    // `DecodeLimits::max_alloc_bytes`, so a hostile `IHDR` cannot force an
    // unbounded allocation here either).
    let _ = oxiarc_png::decode(data);

    // The pull `Decoder`/`Reader` API, exercised separately since `decode`
    // always requests `Transformations::IDENTITY` and always reads the
    // default (first) image of an APNG — this path also drives
    // `next_frame`/row-by-row access and multi-frame navigation.
    let decoder = Decoder::new(Cursor::new(data));
    let Ok(mut reader) = decoder.read_info() else {
        return;
    };
    let Ok(size) = reader.checked_output_buffer_size() else {
        return;
    };
    let mut buf = vec![0u8; size];
    let _ = reader.next_frame(&mut buf);
    let _ = reader.finish();
});
