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
      per-code `Vec` allocation on either side (new in 0.4.2), packed one
      entry per `u64` so a code costs one table load and one table store
      (prefix, length, first byte, last byte and an all-bytes-equal bit)
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
- [x] Decoder rebuilt to libtiff's `LZWDecode` shape and measured against it
      (new in 0.4.2), on strips written by `tiffcp -c lzw`, both arms
      interleaved per round. The three-arm table below comes from a
      scratchpad harness that links `libtiff` directly (not committed — this
      workspace is C-free); the committed `examples/lzw_vs_libtiff.rs`
      reproduces the same two-arm comparison with `tiffcp` alone, which is
      noisier and needs a quiet machine (see the README's Performance
      section). Three-arm result (libtiff / pre-rewrite / now), 4096x4096
      pages, 60 KiB-1 MiB strips, medians of 7 interleaved rounds at load
      10:
      RGB8 117 / 233 / **156 ms** (1.99x -> **1.34x** of libtiff),
      Gray16 55 / 103 / **69 ms** (1.86x -> **1.25x**),
      text 23 / 38 / **30 ms** (1.62x -> **1.28x**),
      incompressible 30 / 107 / **41 ms** (3.56x -> **1.36x**).
      Flat across the strip-size sweep, so one table per strip costs
      under 1.3 us
- [x] `gif_decompress` moved onto the shared decode loop (new in 0.4.2): it
      used to clone a `Vec<u8>` per emitted code. 1 MiB payloads:
      image 14.4 -> 1.14 ms (12.6x), text 4.87 -> 0.84 ms (5.8x),
      one repeated byte 0.30 -> 0.03 ms (10.5x), incompressible
      67.7 -> 2.80 ms (24.2x). `benches/lzw_into_bench.rs::gif_decode`
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
- [x] GIF differential suite against Pillow's own GIF decoder in both
      directions (`tests/gif_oracle.rs`, self-skipping `gif-oracle` feature,
      new in 0.4.2): 20 Pillow-written GIFs (interlaced and not) decode
      byte-identically, 20 GIFs built around `gif_compress` output read back
      byte-identically in Pillow, and every minimum code size 2-8 round
      trips through Pillow. Until now the GIF codec had no reference check
      at all
- [x] Adversarial decode hardening (`tests/decode_hardening.rs`) and
      heap-budget gates (`tests/decode_memory.rs`), new in 0.4.2: every
      output-buffer length on run-heavy and table-filling payloads in all
      six dialect/bit-order combinations, truncation at (almost) every
      offset of the pinned libtiff strips plus single-byte drops, inserts,
      bit flips and splices, the GIF codec under the same attacks at every
      minimum code size, and four allocation bounds — a lying
      `expected_size` cannot force a large allocation, the growable sink
      charges for bytes produced, `decompress_tiff_into` allocates only the
      code table, and a reused `LzwDecoder` allocates nothing per strip
- [x] All features tested (254 tests passing: 233 via nextest + 21 doctests)

## Milestone: COMPLETE

All features implemented and tested. API is stable.

## Known limitations (measured, not blocking)

- `gif_compress` still builds a `HashMap<Vec<u8>, u16>` and pushes the
  current match into a `Vec<u8>` per input byte. The *decode* side was the
  contracted target and is done; the encoder is the obvious next candidate
  (`LzwEncoder` already has the allocation-free `LzwCodeIndex` it needs),
  but its output is pinned byte-for-byte by
  `tests/decoder_reuse.rs` and `tests/bit_order_differential.rs`, so it is a
  deliberate change, not a refactor.
- `z::ZDecoder` (the `.Z` push decoder) reverses each code's bytes through a
  `stack: Vec<u8>` rather than writing the run backwards from its end the
  way the TIFF/GIF loop now does. It is a separate decoder, pinned
  byte-for-byte against `compress`/`uncompress`/`gzip -dc`, and it was not
  in the throughput target; the same rewrite would apply to it.
- Decoding a whole image through `decompress_tiff_into` builds one code
  table per strip. Measured at under 1.3 us per strip (the strip-size sweep
  is flat), but a caller with many small strips should reuse one
  `LzwDecoder` and call `decode_into`, which resets the table in O(1).
