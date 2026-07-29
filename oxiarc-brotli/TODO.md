# oxiarc-brotli - Development Status (v0.4.0, 2026-07-30)

## Completed Features (COMPLETE)

### Core Compression (RFC 7932)
- [x] LZ77 compression engine with backward references
- [x] Context-dependent Huffman coding (decoder: full context maps + Section 7.1 LUTs)
- [x] Static dictionary (RFC 7932 Appendix A: byte-exact 122,784-byte DICT,
      CRC-verified, all 121 Appendix B transforms with UTF-8-aware ferment casing)
- [x] Distance codes: full Section 4 space (ring short codes 0-15, NDIRECT,
      NPOSTFIX, window-bounded dictionary references)
- [x] Insert-and-copy command alphabet (Section 5, 704 symbols, implicit
      distance-code-0 cells)
- [x] Block-type switching (NBLTYPES >= 2, decoder)
- [x] Metadata meta-blocks (MNIBBLES=0, decoder skips per Section 9.2)
- [x] Quality levels 0-11 (0 = stored meta-blocks)
- [x] Window: lgwin 10-24; window size = (1 << lgwin) - 16 (RFC Section 9.1)

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
- [x] Full RFC 7932 conformance overhaul (done 2026-07-13) — the previous
      decoder/encoder was a self-consistent private dialect; both sides were
      rewritten against the RFC and validated *differentially* against the
      reference `brotli` CLI:
      - Decode direction: 608/608 reference streams (qualities 0-11 ×
        windows 10-24, inputs from empty to 1.5 MiB incl. dictionary-heavy
        text, UTF-8, random, boundary sizes) decode **byte-identically**;
        zero errors, zero silent mismatches.
      - Encode direction: 426/426 OxiArc streams across the same grid are
        accepted and correctly decoded by `brotli -d`.
      - Permanent gates: `tests/reference_vectors.rs` (embedded
        reference-produced fixtures, always run) and `tests/brotli_oracle.rs`
        (full CLI sweep behind the `brotli-oracle` feature, self-skips
        without the binary).
- [x] Fuzzing: `fuzz/fuzz_targets/fuzz_brotli_decompress.rs` (workspace) +
      `tests/corruption_robustness.rs` (all truncations rejected, bit flips
      never panic, garbage never panics, long-code decode is O(1)/symbol)
- [x] Interop testing with reference Brotli implementation — superseded by the
      differential oracle above (the old "interop" suite was self-round-trip
      only and masked total reference incompatibility; kept as regression
      tests, no longer the interop evidence)
- [x] High-entropy / incompressible round-trip fix (done 2026-06-06) — near-uniform and incompressible inputs now decode byte-for-byte across all quality levels 1–11. Two encoder bugs fixed:
  1. **Incomplete length-limited Huffman codes** — the `compute_code_lengths` heuristic (`ceil(-log2 p)` + Kraft fix-up) could emit an incomplete prefix code (Kraft sum below 2^15), causing the decoder to fail with "invalid Huffman code: no matching code found". Replaced with the **package-merge algorithm** (Larmore–Hirschberg), which always yields a complete, length-optimal code under the length limit.
  2. **Insert lengths above 319 silently truncated** — a single incompressible meta-block is one insert-and-copy command spanning the whole block, but the encoder only had insert-length categories 0–15 (≤319) and wrapped the excess in 7 bits. **Unified the insert-length code table into one source of truth shared by encoder and decoder**, extending categories to cover inserts up to ~4 MiB.
  - **Tests:** NEW `tests/high_entropy_roundtrip.rs` regression suite — quality 1–11 over random 4 KiB / 64 KiB buffers, incompressible counter, all-distinct / all-same-byte, empty, and mixed content. +13 new tests (150 → 163 passing).

## Test Coverage

- Unit tests (lib): 119 — tables/context/dictionary CRC-checked against the
  RFC's own check values; huffman descriptor write/read round-trips;
  decoder primitives (WBITS tree, NBLTYPES VLC, distance ring semantics)
- reference_vectors: 11 (embedded reference-brotli fixtures; always run)
- brotli_oracle: 3 (full differential sweep vs the `brotli` CLI; feature-gated)
- corruption_robustness: 4 (truncation/bit-flip/garbage/CPU-DoS regression)
- interop_vectors: 19, high_entropy_roundtrip: 8, encoder_bugs: 7,
  pool: 8, progress_cancel: 10, proptest: 2, async: 11, doctests: 11
- Total: 213 tests passing (with `--all-features`)

## Code Statistics

| File | Lines |
|------|-------|
| huffman.rs | ~1,310 |
| decompress.rs | ~890 |
| compress.rs | ~650 |
| streaming.rs | ~510 |
| lz77.rs | ~470 |
| pool.rs | ~470 |
| dictionary.rs | ~460 (+ 122,784-byte `dict_data.bin`) |
| parallel.rs | ~360 |
| bit_reader.rs | ~340 |
| context.rs | ~290 |
| async_brotli.rs | ~270 |
| tables.rs | ~290 |
| bit_writer.rs | ~230 |
| lib.rs | ~270 |
| error.rs | ~90 |
| **Total** | **~6,900** |

## Known Limitations

1. **Encoder ratio gap vs reference**: one prefix code per category per
   meta-block; no block splitting, no context modeling (NTREES > 1), no
   static-dictionary reference emission, no ring-distance codes 1-15.
   Within a few percent of reference q6 on typical text, but notably behind
   at q10-11 and on structured binary data. (The *decoder* handles all of
   these features.)
2. Streaming types buffer the whole input/output in memory (documented in
   the `streaming` module); not bounded-memory streaming.
3. No shared/custom dictionary support yet
4. ~~Quality-1 encoder produces incorrect output for repeated-pattern data~~ — **Fixed 2026-05-17** (copy_length tail guard in `build_insert_copy_commands`)
5. ~~Multi-block encoder is broken: inputs > 256 KiB at quality 4 (> 1 MiB at quality 5+) produce invalid bitstreams~~ — **Fixed 2026-05-17** (same root cause as #4)
6. ~~High-entropy / incompressible data fails to decode at some quality levels ("invalid Huffman code: no matching code found" / truncated insert lengths)~~ — **Fixed 2026-06-06** (package-merge length-limited Huffman codes + unified insert-length code table covering inserts up to ~4 MiB)
