# oxiarc-lzw - Development Status (v0.4.2, 2026-09-08)

## Completed Features (COMPLETE)

### LZW Core
- [x] TIFF-style (MSB-first) compression/decompression
- [x] GIF-style (LSB-first) compression/decompression
- [x] GIF LZW codec (`gif_compress`/`gif_decompress`)
- [x] Configurable code width (9-16 bits; 12 is the TIFF/GIF ceiling, 16 the
      UNIX `compress` one) — the code table, the width-growth rule and the
      encoder's reset trigger are all computed in `u32` so the 65536-entry
      exhausted state is representable (new in 0.4.2)
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
- [x] `LzwConfig::bit_order` (`LzwBitOrder::{Msb, Lsb}`) wired through
      `compress`/`decompress`/`decompress_into`, `LzwEncoder`/`LzwDecoder`
      and `LzwStreamMode::Config` (new in 0.4.2). `LzwConfig::GIF` is now
      genuinely LSB-first and no longer equal to `LzwConfig::TIFF_OLD_STYLE`
      (the documented footgun; pinned by
      `tests/decoder_reuse.rs::the_gif_config_is_lsb_first_and_no_longer_equals_the_old_style_config`)
- [x] `LzwConfig::TIFF_COMPAT_LSB` for libtiff's pre-1993 `LZWDecodeCompat`
      strips (old-style width rule + LSB packing) (new in 0.4.2)
- [x] UNIX `compress` / `.Z` container (`z` module, new in 0.4.2): `1F 9D`
      header with block-mode flag and 9-16 bit widths, LSB-first 8-code
      groups with the reference's group-alignment and reset semantics,
      KwKwK, `decompress`/`decompress_with_limit`/`decompress_into`,
      `compress`/`compress_with_block_mode`, `ZReader`/`ZWriter`.
      Byte-identical to `compress -b N -c` in both directions
      (`tests/z_oracle.rs`, `z-oracle`; committed fixtures in
      `tests/data/z/`)
- [x] `.Z` robustness: truncation is a prefix (no EOI code exists),
      bit-flip and arbitrary-body sweeps, proptest, bounded decode
      everywhere (`tests/z_roundtrip.rs`, `tests/z_proptest.rs`)
- [x] `benches/z_bench.rs`: `.Z` encode/decode/writer throughput and ratios
- [x] All features tested (211 tests passing: 190 via nextest + 21 doctests)

## Milestone: COMPLETE

All features implemented and tested. API is stable.
