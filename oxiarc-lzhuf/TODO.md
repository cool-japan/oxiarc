
# oxiarc-lzhuf - Development Status (v0.4.0, 2026-07-30)

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
- [x] lh1 (LZHUF: 4 KB window + adaptive Huffman) — decoder and spec-conformant greedy encoder (done 2026-07-06)
- [ ] lh2, lh3 (legacy methods)
- [ ] lzs (LZSS without Huffman)
- [ ] lz4, lz5 (LZ methods)
- [ ] pm0, pm2 (PMarc methods)

### Performance
- [x] Better hash function — 4-byte multiplicative hash with improved avalanche (done 2026-05-16)
- [x] Optimal parsing for compression — LzssOptimalParser two-pass DP with Huffman cost retraining + LzhEncoder::with_optimal() (done 2026-05-16)
- [ ] SIMD-accelerated matching
- [x] Parallel compression (done 2026-05-17)
  - `parallel` Cargo feature delivering `lzh_compress_parallel`: multi-entry LHA archive builder where each entry's LZSS+Huffman compression runs in a separate rayon worker. Output is a valid LHA archive decodable by any LHA reader.
  - **Files:** NEW `oxiarc-lzhuf/src/parallel.rs`; MODIFIED `Cargo.toml` (parallel = ["dep:rayon"]), `lib.rs`
  - **Tests (7):** `parallel_basic`, `parallel_determinism`, `parallel_methods`, `parallel_single_entry`, `parallel_empty_archive`, `parallel_builder_api`, `parallel_overlong_filename_error`
  - **Key design:** mtime=0 for determinism; no Lh0 fallback; level-1 header format

### Features
- [x] Streaming decompression (done 2026-05-17)
  - **Fix:** `StreamingHuffmanTree::decode` fallback path now correctly gates acceptance on `entry.length() <= available_bits`; skip_bits can no longer silently NOP on insufficient input. All Lh4/5/6/7 methods now round-trip across all chunk sizes.
  - **Goal:** Fix `StreamingLzhDecoder` for `LzhMethod::{Lh4, Lh5, Lh6, Lh7}`. Currently only Lh0 (stored) round-trips reliably. Bug is localized to streaming bit-pump or `StreamingHuffmanTree` PT code-3 skip-state across `decompress()` call boundaries.
  - **Design:** Three phases: (1) Reproduce — add #[ignore]-guarded failing tests per method; (2) Root-cause Lh5 — patch `StreamingHuffmanTree` to defer PT skip-state across invocations; validate at chunk sizes [1..4096]; (3) Assess Lh4/6/7 — if not fixed by Lh5 patch, return `status: deviated`. Do not exceed 2000 lines in `streaming.rs`; use `splitrs` if needed.
  - **Files:** MODIFY `oxiarc-lzhuf/src/streaming.rs`, `tests/streaming_integration.rs`
  - **Tests:** one roundtrip test per method (Lh4/5/6/7) × chunk sizes [1,2,4,16,64,256,1024,4096]; property test (random input + random chunk sequence); edge cases (1 byte, mid-Huffman, mid-PT-skip, window-size input)
- [x] Progress callbacks (planned 2026-04-20)
  - **Goal:** `encode_lzh` / `decode_lzh` batch APIs gain an optional `ProgressHandle` parameter OR a `.with_progress()` builder on the `LzhuffEncoder`/`LzhuffDecoder` streaming types in `streaming.rs`.
  - **Design:**
    - Preferred API: add `.with_progress(handle)` to the streaming encode/decode types that live in `oxiarc-lzhuf/src/streaming.rs` (1479 lines, full streaming implementation). Avoid changing the batch-API signatures.
    - Emit `on_progress(input_consumed, None)` at each block boundary during encode; `on_progress(output_produced, original_size_if_known)` during decode.
  - **Files:** MODIFY `oxiarc-lzhuf/src/streaming.rs`.
  - **Prerequisites:** core primitive already in.
  - **Tests:** streaming encode + decode round-trip; counting sink observes ≥1 call per block; total-consumed ≈ input length.
  - **Risk:** file is large but already well-structured; additions are localized.
- [x] Custom dictionary initialization — `LzhEncoder::with_dictionary(method, dict)` / `set_dictionary`, `LzhDecoder::with_dictionary(method, size, dict)` / `set_dictionary`; delegates to `LzssEncoder/Decoder::preload_dictionary` which seeds ring buffer and hash chains (done 2026-05-16)

### Compatibility
- [x] Extended testing with real LZH archives (done 2026-07-07) — `tests/data/` corpus (6 genuine third-party `.lzh` fixtures from `fragglet/lhasa`, header levels 0/1/2) exercised by `tests/corpus_fixtures.rs` (decode direction) and `tests/lha_oracle.rs` (encode direction, live `lha`/Lhasa CLI oracle via the opt-in `lha-oracle` feature)
- [x] Fuzzing tests (done 2026-07-08) — `tests/corrupt_input.rs` (`decode_lzh_bit_flip_fuzz_never_panics_or_silently_succeeds`, `decode_lzh_heavy_multi_byte_corruption_can_return_err`: bit-flip and heavy multi-byte corruption of real fixtures, asserting the decoder never panics and either errors cleanly or round-trips) and `tests/proptest_roundtrip.rs` (proptest-based property fuzzing)
- [x] Edge case handling (done 2026-07-08) — `LzssDecoder::new` no longer panics on a zero/non-power-of-two window size (now rounds up via `next_power_of_two().max(16)`, matching `LzssEncoder::new`'s normalization); a malformed/truncated `-lh1-` stream paired with an oversized declared output size no longer loops fabricating output to OOM — the bit reader now signals exhaustion and `decode_lh1` returns an error instead

## Test Coverage

Per-module/binary test counts (`cargo nextest list -p oxiarc-lzhuf --all-features`):

- encode: 24 tests
- streaming (decoder + huffman submodules): 22 tests
- lzss: 16 tests
- lh1: 11 tests
- methods: 7 tests
- optimal: 6 tests
- parallel: 4 tests
- decode: 3 tests
- huffman: 3 tests
- streaming_integration: 42 tests (integration test)
- window_regression: 8 tests (integration test)
- corpus_fixtures: 7 tests (integration test)
- corrupt_input: 7 tests (integration test)
- parallel_lzhuf: 7 tests (integration test)
- lha_oracle: 6 tests (integration test)
- proptest_roundtrip: 2 tests (integration test)
- **Total: 175 tests** (`cargo nextest run -p oxiarc-lzhuf --all-features`) + 5 doctests (`cargo test --doc -p oxiarc-lzhuf --all-features`), verified 2026-07-08

## Code Statistics

Code lines per file (`tokei oxiarc-lzhuf/src`, code lines only, verified 2026-07-08):

| File | Code Lines |
|------|-----------|
| encode.rs | 957 |
| streaming/decoder.rs | 876 |
| lzss.rs | 571 |
| lh1.rs | 507 |
| streaming/huffman.rs | 402 |
| optimal.rs | 314 |
| decode.rs | 233 |
| huffman.rs | 231 |
| methods.rs | 203 |
| parallel.rs | 159 |
| lib.rs | 26 |
| streaming.rs | 7 |
| **Total** | **4,486** |

## Method Comparison

| Method | Window | Bits | Typical Ratio |
|--------|--------|------|---------------|
| lh0 | - | - | 0% (stored) |
| lh4 | 4096 | 12 | ~40-50% |
| lh5 | 8192 | 13 | ~50-60% |
| lh6 | 32768 | 15 | ~55-65% |
| lh7 | 65536 | 16 | ~60-70% |

## Known Limitations

1. Legacy methods lh2/lh3 not implemented (lh1 implemented since 0.3.4)
2. Single-threaded only (batch path; parallel feature available for multi-entry archives)
