//! Fuzz target for TIFF header/IFD parsing only — deliberately narrower and
//! cheaper than `fuzz_tiff_read` (no pixel data is ever decoded), so it
//! spends its whole budget on the surface most exposed to a hostile file:
//! classic/BigTIFF header detection, IFD offset-chain walking (loop
//! detection, `next_image`/`more_images` multi-image navigation), tag
//! parsing for all 18 types (inline vs. offset values, `LONG8`/`SLONG8`/
//! `IFD8`, count-overflow guards), and the `SubIFD` tree walk (EXIF/GPS/
//! Interop directories plus arbitrary nested `SubIFD`s). Must never panic
//! or hang on arbitrary bytes.
#![no_main]

use libfuzzer_sys::fuzz_target;
use oxiarc_tiff::Decoder;
use std::io::Cursor;

/// Bound on `next_image()` calls, so a corrupted IFD chain that
/// (incorrectly) never re-visits an already-seen offset cannot spin the
/// fuzzer forever — real files have at most a handful of pages.
const IMAGE_GUARD: u32 = 10_000;

fuzz_target!(|data: &[u8]| {
    let Ok(mut decoder) = Decoder::new(Cursor::new(data)) else {
        return;
    };

    let mut images = 0u32;
    loop {
        images += 1;
        assert!(
            images < IMAGE_GUARD,
            "no loop detection in the IFD chain walk"
        );

        let _ = decoder.directory();
        let _ = decoder.all_tags();
        let _ = decoder.sub_ifds();
        let _ = decoder.sub_ifd_tree();
        let _ = decoder.exif_directory();
        let _ = decoder.gps_directory();
        let _ = decoder.interop_directory();
        let _ = decoder.dimensions();
        let _ = decoder.color_type();
        let _ = decoder.sample_type();
        let _ = decoder.chunk_type();
        let _ = decoder.chunk_count();
        let _ = decoder.layout();

        match decoder.next_image() {
            Ok(true) => continue,
            Ok(false) | Err(_) => break,
        }
    }

    // `image_count` walks the whole chain up front by a different path
    // (rather than `next_image`'s one-at-a-time navigation) and must agree
    // on "never panics" independently of the loop above.
    let _ = decoder.image_count();
});
