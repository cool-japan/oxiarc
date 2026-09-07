//! Fuzz target for whole-buffer JPEG decoding: `oxiarc_jpeg::Decoder`. Must
//! never panic on arbitrary bytes — baseline, extended, progressive,
//! lossless, arithmetic-coded and OJPEG streams, restart markers, DNL, every
//! sampling factor, and any truncated or corrupted mixture of the above are
//! all expected to end in either a decoded image or a structured
//! [`oxiarc_jpeg::JpegError`].
#![no_main]

use libfuzzer_sys::fuzz_target;
use oxiarc_jpeg::Decoder;
use std::io::Cursor;

/// A frame header alone can claim dimensions that would make the output
/// allocation itself the slow part of a fuzz iteration; this is a
/// never-panics target, not a benchmark, so absurdly large claimed frames
/// are skipped rather than allocated.
const MAX_OUTPUT_BYTES: usize = 64 * 1024 * 1024;

fuzz_target!(|data: &[u8]| {
    let mut decoder = Decoder::new(Cursor::new(data));
    let Ok(_info) = decoder.read_info() else {
        return;
    };
    if let Some(size) = decoder.output_buffer_size() {
        if size > MAX_OUTPUT_BYTES {
            return;
        }
    }
    let _ = decoder.decode();
});
