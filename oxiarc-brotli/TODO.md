# oxiarc-brotli - Development Status (v0.3.1, 2026-05-30)

## Completed Features (COMPLETE)

### Core Compression (RFC 7932)
- [x] LZ77 compression engine with backward references
- [x] Context-dependent Huffman coding (insert-and-copy lengths)
- [x] Static dictionary support (RFC 7932 Appendix A, 122,784 entries)
- [x] Distance codes with short-distance ring buffer cache
- [x] Insert-and-copy length encoding
- [x] Quality levels 0-11
- [x] Window size: configurable (16-24 bits, default 22 = 4MB)

### Bit I/O
- [x] BrotliBitReader for decompression
- [x] BrotliBitWriter for compression
- [x] Byte-aligned and unaligned operations

### Huffman Coding
- [x] Prefix code generation
- [x] Simple and complex prefix codes
- [x] Context map decoding/encoding
- [x] Block type switching

### Streaming API
- [x] BrotliCompressor<W: Write> - streaming compressor
- [x] BrotliDecompressor<R: Read> - streaming decompressor
- [x] finish() for flushing final output

### Public API
- [x] compress(data, quality) -> BrotliResult<Vec<u8>>
- [x] compress_with_params(data, params) -> BrotliResult<Vec<u8>>
- [x] decompress(data) -> BrotliResult<Vec<u8>>
- [x] BrotliParams configuration struct

## Future Enhancements

### Performance
- [ ] SIMD-accelerated matching
- [ ] Multi-threaded compression
- [x] Memory pool for per-encode allocations (`BrotliPool`)
  - **Implemented:** Thread-safe buffer pool with three typed `Mutex<Vec<Vec<T>>>` buckets:
    `lz77_cmd` (Lz77Command Vec), `hash_u32` (131072-entry hash-head table, 512 KiB),
    `huffman_scratch` (1024 u32s). RAII handles (`PooledCmdBuf`, `PooledU32Buf`).
    `BrotliPool::new()`, `BrotliPool::with_cap(n)`, `BrotliPool::clone()` (cheap Arc clone).
    `BrotliPool::stats()` → `PoolStats` with six counters. `compress_with_params_pooled()`.
    `BrotliCompressor::with_pool(&BrotliPool)` builder.
  - **Files:** NEW `src/pool.rs`; MODIFIED `compress.rs`, `lz77.rs`, `streaming.rs`, `lib.rs`
  - **Tests:** 8 integration tests in `tests/pool_brotli.rs` — all passing.
  - **Encoder bugs fixed (2026-05-17):**
    1. Quality-1 repeated-pattern corruption — root cause: `build_insert_copy_commands` could produce `copy_length == 1` (unencodable; decoder always reads minimum 2). Fixed by reducing any split chunk that would leave a 1-byte tail, ensuring every chunk ≥ 2.
    2. Multi-block encoder broken for inputs > block-size boundary — same root cause: the 1-byte copy tail caused bit-alignment drift that corrupted subsequent meta-block headers, producing "unexpected end of stream". Fixed by the same one-line guard in `build_insert_copy_commands`.
- [ ] Optimal parsing improvements

### Features
- [ ] Dictionary preloading (shared dictionary)
- [ ] Quality level fine-tuning
- [x] Progress callbacks (planned 2026-04-20)
  - **Goal:** `BrotliEncoder`, `BrotliDecoder`, and the streaming reader/writer types accept `ProgressHandle` and emit `on_progress(bytes_in, Some(total))` at each encode/decode call boundary.
  - **Design:**
    - Add `progress: Option<ProgressHandle>` field on `BrotliEncoder`/`BrotliDecoder` + streaming types; `.with_progress(handle)` builder.
    - In `encode(input) -> output`, emit after producing output; in streaming `flush`/`finish`, emit with `(produced, Some(estimated_total))` when known; `None` when unknown (streaming writer with unknown total).
    - Wire `CancellationToken` in the same motion for symmetry with lzma's dual item — emit `token.check()?` at the top of each encode/decode iteration.
  - **Files:**
    - MODIFY `oxiarc-brotli/src/encode.rs`, `decode.rs`, and any streaming module exposing `BrotliStreamEncoder`/`BrotliStreamDecoder` (detect via grep during implementation).
    - MODIFY `oxiarc-brotli/Cargo.toml` — `oxiarc-core.workspace = true` already likely; otherwise add.
  - **Prerequisites:** `ProgressSink` + `CancellationToken` already in `oxiarc-core`.
  - **Tests:** counting-sink fixture on encode + decode round-trip; cancellation fixture that cancels mid-decode and asserts `OxiArcError::Cancelled`.
  - **Risk:** Progress at iteration boundary only (not per byte) to avoid overhead. Mitigated by virtual-call-amortization (one call per chunk).
- [x] Async I/O support
  - **Goal:** `async-io` Cargo feature implementing `oxiarc_core::async_io::{AsyncCompressor, AsyncDecompressor}` on `BrotliCompressor`/`BrotliDecompressor`. Mirrors `async_deflate.rs`: read-all → sync-process → write-all. NOT bounded-memory streaming; docs state this explicitly.
  - **Design:** NEW `oxiarc-brotli/src/async_brotli.rs` gated by `#[cfg(feature = "async-io")]`. Feature: `async-io = ["oxiarc-core/async-io", "dep:tokio"]`. Body: `AsyncReadExt::read_to_end` → `compress_with_params` / `decompress` → `write_all` → `flush`.
  - **Files:** NEW `oxiarc-brotli/src/async_brotli.rs`; MODIFY `Cargo.toml`, `lib.rs`
  - **Tests:** async_roundtrip (qualities 1/5/11), async_decode_serial_output, async_encode_serial_decode, async_empty

### Compatibility
- [ ] Full RFC 7932 conformance testing
- [ ] Fuzzing tests
- [x] Interop testing with reference Brotli implementation — 19 integration tests covering all quality levels 0–11, empty/single-byte/binary/text/large-input roundtrips, `compress_with_params` variations, minimum-window (lgwin=16), compression-is-beneficial assertion, invalid parameter rejection (done 2026-05-30)

## Test Coverage

- compress: ~20 tests (roundtrip, quality levels, edge cases)
- decompress: ~15 tests (various input patterns)
- huffman: ~10 tests (prefix codes, context maps)
- streaming: ~15 tests (compressor/decompressor, empty, large data)
- bit_reader/bit_writer: ~15 tests
- Total: 79 tests

## Code Statistics

| File | Lines |
|------|-------|
| compress.rs | ~900 |
| decompress.rs | ~700 |
| huffman.rs | ~500 |
| lz77.rs | ~350 |
| streaming.rs | ~330 |
| dictionary.rs | ~200 |
| context.rs | ~150 |
| bit_reader.rs | ~120 |
| bit_writer.rs | ~110 |
| error.rs | ~50 |
| lib.rs | ~50 |
| **Total** | **~3,460** |

## Known Limitations

1. Quality levels 10-11 may not achieve compression ratios matching reference implementation
2. Single-threaded only (parallel feature not yet implemented)
3. No shared dictionary support yet
4. ~~Quality-1 encoder produces incorrect output for repeated-pattern data~~ — **Fixed 2026-05-17** (copy_length tail guard in `build_insert_copy_commands`)
5. ~~Multi-block encoder is broken: inputs > 256 KiB at quality 4 (> 1 MiB at quality 5+) produce invalid bitstreams~~ — **Fixed 2026-05-17** (same root cause as #4)
