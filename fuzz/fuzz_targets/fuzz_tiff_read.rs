//! Fuzz target for whole-image TIFF decoding: `oxiarc_tiff::Decoder`. Must
//! never panic on arbitrary bytes — classic and BigTIFF, either byte order,
//! any codec (None/PackBits/CCITT/LZW/Deflate/Zstd/LZMA/JPEG), any
//! photometric interpretation, strips or tiles, are all expected to end in
//! either decoded samples or a structured [`oxiarc_tiff::TiffError`].
#![no_main]

use libfuzzer_sys::fuzz_target;
use oxiarc_tiff::Decoder;
use std::io::Cursor;

/// A tag alone can claim dimensions that would make the output allocation
/// itself the slow part of a fuzz iteration; this is a never-panics target,
/// not a benchmark. `Limits::default()`'s own `max_image_bytes` already
/// guards this in the crate, but capping here too keeps every iteration
/// fast regardless of how that default evolves.
const MAX_OUTPUT_BYTES: usize = 64 * 1024 * 1024;

/// Bound on `next_image()` calls, held identical to `fuzz_tiff_ifd`'s so the
/// two targets agree on what "the IFD chain does not terminate" means.
///
/// A corrupted offset chain that is never detected as a cycle would otherwise
/// spin forever, and it costs more here than in the header-only sibling:
/// every turn can additionally allocate and fill a pixel buffer, so an
/// undetected cycle is an out-of-memory abort or a multi-hour hang rather
/// than a tight spin. That cost argues for *having* the guard, not for a
/// tighter number — a chain long enough to approach this bound is
/// necessarily made of minimal, tag-less IFDs whose `dimensions()` call
/// fails, so no buffer is allocated on those turns anyway, and the per-turn
/// cost of the inputs that *do* allocate is already bounded by
/// `MAX_OUTPUT_BYTES` above. Real files have at most a handful of pages.
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
        if let Ok((width, height)) = decoder.dimensions() {
            // A generous 8-bytes-per-pixel upper bound (covers up to 4
            // samples at 16 bits each; a wider combination just gets a
            // clean `UsageError::BufferTooSmall` below, never a panic).
            let claimed = (width as u64)
                .saturating_mul(height as u64)
                .saturating_mul(8);
            if claimed > 0 && claimed <= MAX_OUTPUT_BYTES as u64 {
                let mut buf = vec![0u8; claimed as usize];
                let _ = decoder.read_image_bytes(&mut buf);
            }
        }
        let _ = decoder.all_tags();
        let _ = decoder.icc_profile();

        match decoder.next_image() {
            Ok(true) => continue,
            Ok(false) | Err(_) => break,
        }
    }
});
