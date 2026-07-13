# oxiarc-zstd - Development Status (v0.4.0, 2026-07-08)

## Completed Features (COMPLETE)

### Zstandard Core
- [x] Pure Rust implementation
- [x] **Reference interoperability, both directions** (new in 0.3.6): the FSE/Huffman
      backward bitstream now follows the RFC 8878 / reference `BIT_*` semantics
      (LIFO, sentinel-anchored) in reader AND writer; Huffman weight decoding uses
      two interleaved FSE states with the implied-last-weight deduction and full
      Kraft validation; the 4-stream jump table is parsed as stream sizes with
      monotonic bounds checks. Verified: real `zstd`-CLI frames (levels 1-19,
      `--ultra -22`, `--long`, `--no-check`, `--no-content-size`, raw dictionaries,
      multi-frame) decode byte-identically, and all oxiarc frames are accepted by
      `zstd -d` (embedded fixtures always-on; live matrix behind `zstd-oracle`)
- [x] Huffman-compressed literal sections on the encode path (self-verified, with
      Raw/RLE fallback); real FSE compression-table (`FSE_buildCTable`) sequence
      encoding with the RFC 8878 predefined/RLE tables (new in 0.3.6)
- [x] Fixed `set_content_size(false)` corrupting inputs >= 256 B (truncated 1-byte
      FCS): such frames now use an explicit windowed header (new in 0.3.6)
- [x] Raw-content dictionaries carry no Dictionary_ID, so `zstd -d -D <dict>`
      accepts oxiarc dictionary frames (new in 0.3.6)
- [x] Fast decompression
- [x] Parallel compression (Rayon)
- [x] Dictionary support (raw-content, interoperable both directions)
- [x] Checksum support (XXH64)
- [x] Streaming API
- [x] Hardened frame-header handling: `try_reserve`-bounded output allocation against untrusted `Frame_Content_Size`, and a bounded `Window_Descriptor` for large one-shot frames (new in 0.3.6)
- [x] Every FSE/Huffman table and state index bounds-checked; malformed or
      truncated input returns `Err` (60k-case mutation fuzz: zero panics)
- [x] `BlockType`/`LiteralsBlockType` marked `#[non_exhaustive]`; `lz77`/`bitwriter` advanced re-exports demoted to `#[doc(hidden)]` (API freeze, new in 0.3.6)
- [x] All features tested (195 tests + 6 live-oracle tests passing)

## Milestone: COMPLETE

All features implemented and tested. API is stable.

## Known ratio limitation (honest scope)

Sequences are entropy-coded with the RFC 8878 **predefined** FSE tables (or RLE
tables) only; custom block-optimal `FSE_Compressed` sequence tables are decoded
but not yet emitted (`src/fse_encoder.rs` stays dormant until then). Frames are
fully interoperable, but the ratio on some inputs trails the reference encoder.

## Pending

- [x] Add `with_progress` / `with_cancel` builders to zstd codecs (done 2026-05-06)
  - **Goal:** `ZstdEncoder`, `ZstdStreamEncoder<W>`, `ZstdStreamDecoder<R>` gain `with_progress` and `with_cancel` builders. Per-block hooks.
  - **Design:** Mirror bzip2 template. `ZstdEncoder` (encode.rs:33) — progress once after compress, cancel at start. `ZstdStreamEncoder` (streaming.rs:51) + `ZstdStreamDecoder` (streaming.rs:206) — hook per zstd-block boundary.
  - **Files:** MODIFY `oxiarc-zstd/src/encode.rs`, MODIFY `oxiarc-zstd/src/streaming.rs`, possibly MODIFY `oxiarc-zstd/Cargo.toml`
  - **Tests:** `test_zstd_stream_encoder_progress_reports`, `test_zstd_stream_encoder_cancel_aborts`, same for StreamDecoder
  - **Risk:** low
