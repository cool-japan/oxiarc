//! Fuzz target for `oxiarc_image`, the thin `image`-crate-shaped facade over
//! `oxiarc-png`/`oxiarc-jpeg`/`oxiarc-tiff`: `guess_format`,
//! `load_from_memory`, `load_from_memory_with_format` (tried against every
//! decodable format regardless of what the magic sniff guessed, so a
//! mismatched format argument is exercised too, not just the happy path)
//! and `ImageReader` must never panic on arbitrary bytes.
#![no_main]

use libfuzzer_sys::fuzz_target;
use oxiarc_image::{ImageFormat, ImageReader, guess_format, load_from_memory_with_format};
use std::io::Cursor;

/// The formats this build can actually decode (`ImageFormat::can_decode`).
const DECODABLE: [ImageFormat; 3] = [ImageFormat::Png, ImageFormat::Jpeg, ImageFormat::Tiff];

fuzz_target!(|data: &[u8]| {
    let guessed = guess_format(data);

    for format in DECODABLE {
        let _ = load_from_memory_with_format(data, format);
    }

    // `ImageReader::with_guessed_format` exercises the sniffing path
    // through a `BufRead + Seek` source rather than the plain byte-slice
    // entry points above.
    if let Ok(sniffed) = ImageReader::new(Cursor::new(data)).with_guessed_format() {
        let _ = sniffed.into_dimensions();
    }

    // A reader with the format pinned to whatever `guess_format` decided
    // (or PNG, if it could not tell), decoded end to end.
    let mut reader = ImageReader::new(Cursor::new(data));
    reader.set_format(guessed.unwrap_or(ImageFormat::Png));
    let _ = reader.decode();
});
