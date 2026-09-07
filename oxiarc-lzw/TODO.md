# oxiarc-lzw - Development Status (v0.4.2, 2026-09-07)

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
- [x] Prefix/suffix code table shared by the encoder and decoder — no
      per-code `Vec` allocation on either side (new in 0.4.2)
- [x] `decompress_into` / `decompress_tiff_into`: decode straight into a
      caller-supplied buffer, bounds-checked, no intermediate `Vec`
      (new in 0.4.2; the TIFF `Compression = 5` entry point)
- [x] `LzwConfig::TIFF_OLD_STYLE`: writers that use the standard (late)
      code-width change instead of TIFF's early change (new in 0.4.2).
      Not libtiff's `LZWDecodeCompat`, which is additionally LSB-first;
      fallback rules and their limits are pinned in
      `tests/old_style_fallback.rs`
- [x] libtiff `tiffcp -c lzw` / `-c lzw:2` multi-strip differential suite
      (`tests/tiffcp_strip_decode.rs`, `tiff-oracle`) (new in 0.4.2)
- [x] A/B benchmark against a pinned copy of the pre-0.4.2 decoder
      (`benches/lzw_into_bench.rs`): 5.6x-16.5x across strip shapes over
      three runs (worst case 7.25x in the last one), well above the >= 3x
      target (new in 0.4.2)
- [x] All features tested (130 tests passing: 119 via nextest + 11 doctests)

## Milestone: COMPLETE

All features implemented and tested. API is stable.
