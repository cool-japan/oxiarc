
# oxiarc-lzhuf - Development Status (v0.2.8, 2026-05-08)

## Completed Features (COMPLETE)

### Methods
- [x] `LzhMethod` enum (Lh0, Lh4, Lh5, Lh6, Lh7)
- [x] Window size calculation
- [x] Position bits calculation
- [x] Method detection from string (e.g., "-lh5-")
- [x] Validation of method parameters

### LZSS
- [x] `LzssEncoder` with configurable window
- [x] `LzssDecoder` with ring buffer
- [x] `LzssToken` enum (Literal/Match)
- [x] Minimum match length: 3 bytes
- [x] Maximum match length: 256 bytes
- [x] Window sizes: 4KB, 8KB, 32KB, 64KB
- [x] Hash chain for pattern matching
- [x] Copy-from-self for overlapping matches

### Huffman
- [x] `LzhHuffmanTree` structure
- [x] Tree building from code lengths
- [x] Fast table-based decoding
- [x] CODES tree (literals + lengths)
- [x] OFFSETS tree (distances)
- [x] Code length decoding from bitstream
- [x] Standard LZH tree format
- [x] PT code 3 skip mechanism handling

### Encode
- [x] `LzhEncoder` high-level API
- [x] LZSS tokenization
- [x] Huffman tree building from frequencies
- [x] Tree serialization to bitstream
- [x] Token encoding with proper PT/C-tree mapping
- [x] `encode_lzh()` one-shot function
- [x] lh0 (stored) support
- [x] LH5 compression (full roundtrip working)

### Decode
- [x] `LzhDecoder` high-level API
- [x] Huffman tree reading from bitstream
- [x] Token decoding
- [x] LZSS expansion
- [x] `decode_lzh()` one-shot function
- [x] lh0 (stored) support
- [x] LH5 decompression (full roundtrip working)
- [x] Size validation

## Future Enhancements

### Additional Methods
- [ ] lh1, lh2, lh3 (legacy methods)
- [ ] lzs (LZSS without Huffman)
- [ ] lz4, lz5 (LZ methods)
- [ ] pm0, pm2 (PMarc methods)

### Performance
- [ ] Better hash function
- [ ] Optimal parsing for compression
- [ ] SIMD-accelerated matching
- [ ] Parallel compression

### Features
- [ ] Streaming decompression
- [x] Progress callbacks (planned 2026-04-20)
  - **Goal:** `encode_lzh` / `decode_lzh` batch APIs gain an optional `ProgressHandle` parameter OR a `.with_progress()` builder on the `LzhuffEncoder`/`LzhuffDecoder` streaming types in `streaming.rs`.
  - **Design:**
    - Preferred API: add `.with_progress(handle)` to the streaming encode/decode types that live in `oxiarc-lzhuf/src/streaming.rs` (1479 lines, full streaming implementation). Avoid changing the batch-API signatures.
    - Emit `on_progress(input_consumed, None)` at each block boundary during encode; `on_progress(output_produced, original_size_if_known)` during decode.
  - **Files:** MODIFY `oxiarc-lzhuf/src/streaming.rs`.
  - **Prerequisites:** core primitive already in.
  - **Tests:** streaming encode + decode round-trip; counting sink observes ≥1 call per block; total-consumed ≈ input length.
  - **Risk:** file is large but already well-structured; additions are localized.
- [ ] Custom dictionary initialization

### Compatibility
- [ ] Extended testing with real LZH archives
- [ ] Fuzzing tests
- [ ] Edge case handling

## Test Coverage

- Total: 57 tests (34 lib + 20 streaming_integration + 3 doctests)

## Code Statistics

| File | Lines |
|------|-------|
| lzss.rs | ~500 |
| huffman.rs | ~500 |
| encode.rs | ~400 |
| decode.rs | ~400 |
| streaming.rs | ~600 |
| methods.rs | ~200 |
| lib.rs | ~146 |
| **Total** | **~2,746** |

## Method Comparison

| Method | Window | Bits | Typical Ratio |
|--------|--------|------|---------------|
| lh0 | - | - | 0% (stored) |
| lh4 | 4096 | 12 | ~40-50% |
| lh5 | 8192 | 13 | ~50-60% |
| lh6 | 32768 | 15 | ~55-65% |
| lh7 | 65536 | 16 | ~60-70% |

## Known Limitations

1. Legacy methods (lh1-lh3) not implemented
2. No streaming decompression yet
3. Single-threaded only
4. No dictionary preload
