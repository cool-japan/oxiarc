# oxiarc-zstd - Development Status (v0.3.1, 2026-05-30)

## Completed Features (COMPLETE)

### Zstandard Core
- [x] Pure Rust implementation
- [x] Excellent compression ratios
- [x] Fast decompression
- [x] Parallel compression (Rayon)
- [x] Dictionary support
- [x] Checksum support (XXH64)
- [x] Streaming API
- [x] All features tested (170 tests passing)

## Milestone: COMPLETE

All features implemented and tested. API is stable.

## Pending

- [x] Add `with_progress` / `with_cancel` builders to zstd codecs (done 2026-05-06)
  - **Goal:** `ZstdEncoder`, `ZstdStreamEncoder<W>`, `ZstdStreamDecoder<R>` gain `with_progress` and `with_cancel` builders. Per-block hooks.
  - **Design:** Mirror bzip2 template. `ZstdEncoder` (encode.rs:33) — progress once after compress, cancel at start. `ZstdStreamEncoder` (streaming.rs:51) + `ZstdStreamDecoder` (streaming.rs:206) — hook per zstd-block boundary.
  - **Files:** MODIFY `oxiarc-zstd/src/encode.rs`, MODIFY `oxiarc-zstd/src/streaming.rs`, possibly MODIFY `oxiarc-zstd/Cargo.toml`
  - **Tests:** `test_zstd_stream_encoder_progress_reports`, `test_zstd_stream_encoder_cancel_aborts`, same for StreamDecoder
  - **Risk:** low
