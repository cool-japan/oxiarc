# oxiarc-snappy - Development Status (v0.2.7, 2026-04-21)

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
- [ ] CRC32C hardware acceleration (SSE 4.2)
- [ ] Multi-threaded frame compression
- [ ] Memory pool for allocations

### Features
- [ ] Dictionary support
- [x] Progress callbacks (planned 2026-04-20)
  - **Goal:** `FrameEncoder`/`FrameDecoder` (frame format with CRC32C + chunks) accept `ProgressHandle`; emit `on_progress(processed, None)` per chunk (64 KiB max in Snappy frame).
  - **Design:** Same pattern as brotli. `.with_progress(handle)` builder on both Frame types. Hook inside per-chunk read/write loop.
  - **Files:** MODIFY `oxiarc-snappy/src/frame.rs` (or equivalent — locate during implementation).
  - **Prerequisites:** core primitive already in.
  - **Tests:** counting-sink on encode + decode; assert `processed` is monotonic and ≈ input size.
  - **Risk:** none significant.
- [ ] Async I/O support

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
| compress.rs | ~300 |
| decompress.rs | ~250 |
| crc32c.rs | ~200 |
| error.rs | ~80 |
| lib.rs | ~148 |
| **Total** | **~1,428** |

## Known Limitations

1. Single-threaded only
2. No hardware CRC32C acceleration
3. No dictionary support
