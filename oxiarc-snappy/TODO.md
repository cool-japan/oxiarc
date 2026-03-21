# oxiarc-snappy - Development Status

## Completed Features (v0.2.6)

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
- [ ] Progress callbacks
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
