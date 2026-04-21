# oxiarc-lzw - Development Status (v0.2.7, 2026-04-21)

## Completed Features (COMPLETE)

### LZW Core
- [x] TIFF-style (MSB-first) compression/decompression
- [x] GIF-style (LSB-first) compression/decompression
- [x] GIF LZW codec (`gif_compress`/`gif_decompress`)
- [x] Configurable code width (9-12 bits)
- [x] Early change (code width increases before table full)
- [x] Streaming encoder/decoder
- [x] All features tested (76 tests passing)

## Milestone: COMPLETE

All features implemented and tested. API is stable.
