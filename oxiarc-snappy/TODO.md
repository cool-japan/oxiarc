# oxiarc-snappy - Development Status (v0.3.0, 2026-05-16)

## Completed Features (COMPLETE)

### Block Format
- [x] Snappy block compression with hash-based matching
- [x] Block decompression with literal/copy command parsing
- [x] Variable-length literal encoding (tag byte + optional length bytes)
- [x] Copy operations: 1-byte offset, 2-byte offset, 4-byte offset
- [x] Maximum block size: 64 KiB

### Framed Format (Streaming)
- [x] Frame magic number and stream identifier chunk
- [x] Compressed data chunks with CRC32C checksums
- [x] Uncompressed data chunks
- [x] Padding and skippable chunks
- [x] FrameEncoder<W: Write> - streaming frame encoder
- [x] FrameDecoder<R: Read> - streaming frame decoder

### CRC32C
- [x] Pure Rust CRC32C implementation (Castagnoli polynomial)
- [x] Slicing-by-4 optimization
- [x] Masked CRC for Snappy framing format

### Public API
- [x] compress(data) -> Vec<u8>
- [x] decompress(data) -> Result<Vec<u8>>
- [x] max_compress_len(input_len) -> usize
- [x] decompress_len(compressed) -> Result<usize>

## Future Enhancements

### Performance
- [ ] SIMD-accelerated matching
- [x] Snappy CRC32C with SSE 4.2 hardware acceleration on x86_64 (done 2026-05-06)
  - **Goal:** `crc32c()` in `crc32c.rs` dispatches to `_mm_crc32_u8`/`_mm_crc32_u64` intrinsics on x86_64 when `is_x86_feature_detected!("sse4.2")` is true, else falls back to scalar. Output bitwise-identical to scalar.
  - **Design:** Private fn `crc32c_sse42` under `#[target_feature(enable = "sse4.2")]` + `unsafe`: 8 bytes at a time via `_mm_crc32_u64` (`read_unaligned`), 1–7 trailing via `_mm_crc32_u8`. Runtime dispatcher via `OnceLock<fn(&[u8]) -> u32>`. Snappy masked form applied after raw CRC. Scalar fallback unchanged.
  - **Files:** MODIFY `oxiarc-snappy/src/crc32c.rs`
  - **Tests:** `test_crc32c_sse_matches_scalar_lengths` (0..=4096 step 17), `test_crc32c_sse_random` (100 inputs fixed seed), `test_crc32c_known_vectors`; all `#[cfg(target_arch = "x86_64")]`
  - **Risk:** Mac is aarch64 — SSE4.2 path not exercised locally; scalar fallback is default so non-blocking.
- [x] Multi-threaded frame compression — compress_parallel() via parallel feature flag, rayon-based (done 2026-05-16)
- [x] Memory pool for allocations (done 2026-05-17)
  - **Goal:** Buffer pool for Snappy's FrameEncoder/FrameDecoder scratch (two buckets: `encoder_scratch` 64 KiB + `decoder_scratch` 64 KiB). Mirrors `DeflatePool` structure.
  - **Design:** `SnappyPool` with two `Mutex<Vec<Vec<u8>>>` buckets. Direct `PoolInner` field access pattern (pub(crate)). Builder: `FrameEncoder::with_pool(&SnappyPool)`, `FrameDecoder::with_pool(&SnappyPool)`, free fn `compress_frame_pooled`. Default cap 4/bucket; `with_cap(usize)` builder. Existing constructors unchanged (pool is strictly opt-in).
  - **Files:** NEW `oxiarc-snappy/src/pool.rs`; MODIFIED `frame.rs`, `lib.rs`, `Cargo.toml`; NEW `tests/pool_snappy.rs`
  - **Tests:** `test_pool_encoder_hits` (hits ≥ 2), `test_pool_decoder_hits` (hits ≥ 2), `test_pool_roundtrip_equality` (byte-identical cross-API), `test_pool_concurrent` (8 rayon threads), `test_pool_cap` (with_cap(2)), `test_pool_default`

### Features
- [x] Dictionary support (done 2026-05-17)
  - **Goal:** Block-level and frame-level dictionary compression/decompression as an OxiArc-specific extension (Snappy spec has no dict semantics).
  - **Design:** `compress_block_with_dict` pre-seeds the hash table from the dict prefix then encodes only the input bytes; `decompress_block_with_dict` pre-populates the output window with dict bytes and strips them at the end. Frame functions add an OxiArc skippable chunk (`0xFE`, body `"OXIAD" | crc32c(dict) | dict_len`) before the data chunks; the decoder validates the dict CRC before decoding. Dicts > 64 KiB are truncated to the last 64 KiB. Empty dict is byte-identical to non-dict path.
  - **Files:** MODIFY `compress.rs`, `decompress.rs`, `frame.rs`, `lib.rs`; NEW `tests/dict_snappy.rs`
  - **Tests:** block roundtrip, better compression, wrong dict garbles, empty dict parity, overlong dict truncation, boundary cases; frame roundtrip, skippable chunk structure, wrong dict rejected, empty dict, standard frame rejected.
- [x] Progress callbacks (planned 2026-04-20)
  - **Goal:** `FrameEncoder`/`FrameDecoder` (frame format with CRC32C + chunks) accept `ProgressHandle`; emit `on_progress(processed, None)` per chunk (64 KiB max in Snappy frame).
  - **Design:** Same pattern as brotli. `.with_progress(handle)` builder on both Frame types. Hook inside per-chunk read/write loop.
  - **Files:** MODIFY `oxiarc-snappy/src/frame.rs` (or equivalent — locate during implementation).
  - **Prerequisites:** core primitive already in.
  - **Tests:** counting-sink on encode + decode; assert `processed` is monotonic and ≈ input size.
  - **Risk:** none significant.
- [x] Async I/O support
  - **Goal:** `async-io` Cargo feature implementing `oxiarc_core::async_io::{AsyncCompressor, AsyncDecompressor}` on Snappy frame encode/decode. Mirrors `async_deflate.rs` pattern.
  - **Design:** NEW `oxiarc-snappy/src/async_snappy.rs` gated by `#[cfg(feature = "async-io")]`. Wrapper types `AsyncFrameEncoder<W: AsyncWrite>` / `AsyncFrameDecoder<R: AsyncRead>`. Internal flow: `read_to_end` → bridge to `FrameEncoder`/`FrameDecoder` → `write_all`. Free helpers `compress_frame_async` / `decompress_frame_async`.
  - **Files:** NEW `oxiarc-snappy/src/async_snappy.rs`; MODIFY `Cargo.toml`, `lib.rs`
  - **Tests:** async roundtrip, cross-API parity (sync↔async + parallel output decodable), async_empty (zero bytes returns Ok(0))

### Compatibility
- [ ] Interop testing with Google Snappy reference
- [ ] Fuzzing tests
- [ ] Edge case handling (max-size blocks)

## Test Coverage

- compress: ~15 tests (roundtrip, edge cases, various patterns)
- decompress: ~10 tests (invalid input, corrupt data)
- frame: ~15 tests (streaming, CRC verification, chunks)
- crc32c: ~10 tests (known vectors, masked CRC)
- lib: ~4 tests
- Total: 58 tests

## Code Statistics

| File | Lines |
|------|-------|
| frame.rs | ~450 |
| frame_parallel.rs | ~185 (NEW in v0.3.0) |
| compress.rs | ~300 |
| decompress.rs | ~250 |
| crc32c.rs | ~200 |
| error.rs | ~80 |
| lib.rs | ~148 |
| **Total** | **~1,428** |

## Known Limitations

1. Single-threaded only
2. Hardware CRC32C acceleration (SSE 4.2) implemented on x86_64
3. No dictionary support
