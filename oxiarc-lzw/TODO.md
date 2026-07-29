# oxiarc-lzw - Development Status (v0.4.1, 2026-07-30)

## Completed Features (COMPLETE)

### LZW Core
- [x] TIFF-style (MSB-first) compression/decompression
- [x] GIF-style (LSB-first) compression/decompression
- [x] GIF LZW codec (`gif_compress`/`gif_decompress`)
- [x] Configurable code width (9-12 bits)
- [x] Early change (code width increases before table full)
- [x] Streaming encoder/decoder
- [x] `LzwConfig: Default` (TIFF preset) and `#[non_exhaustive] LzwError` (new in 0.3.6)
- [x] Property-based round-trip testing (proptest) (new in 0.3.6)
- [x] All features tested (79 tests passing)

## Milestone: COMPLETE

All features implemented and tested. API is stable.
