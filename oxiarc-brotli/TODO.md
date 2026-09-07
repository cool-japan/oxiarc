# oxiarc-brotli - Development Status (v0.4.2, 2026-09-08)

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
- [x] BrotliCompressor<W: Write> - incremental compressor
- [x] BrotliDecompressor<R: Read> - incremental decompressor (built on BrotliStream)
- [x] finish() for flushing final output

### Bounded incremental decoding (BrotliStream, 0.4.2)
- [x] `decode(&mut self, input, output, flush) -> BrotliProgress { consumed, produced,
      status: NeedInput | NeedOutput | StreamEnd }` push decoder; `finish()`, `reset()`
- [x] Meta-block preludes parsed atomically with bit-cursor rollback and retry,
      bounded by a 1 MiB header cap; the shared `MetaBlockHeader::read` parser is the
      one the one-shot decoder uses, so both read header bits at identical positions
- [x] Command loop resumable at every literal, every byte of a backward reference and
      every byte of a transformed static-dictionary word; up to 256 literal /
      insert-and-copy / distance prefix trees, both context maps, block-switch state
      and the distance ring all survive a `NeedInput` return
- [x] Real sliding-window ring (power-of-two, lazily allocated, grown on demand to the
      declared `1 << WBITS`) with bulk `copy_within` matches and periodic tiling for
      short distances; literals decoded straight into the ring's linear region
- [x] Uncompressed meta-blocks streamed without materialising
- [x] `with_max_output(u64)` — exact pre-decode MLEN check per meta-block
- [x] `with_max_window(usize)` — default 16 MiB, refused before allocation
      (`BrotliError::WindowTooLarge`)
- [x] `with_shape_recording(bool)` — `MetaBlockShape` differential vs
      `decompress_reporting_shapes`
- [x] Sticky fault latch, cleared only by `reset()`
- [x] Input consumed exactly once; the bounded carry is compacted in place, never drained
- [x] `BrotliAsyncDecompressor` re-based on `BrotliStream` (bounded staging buffer)
- [x] Tests: byte-at-a-time in and out, prime chunk sizes, proptest split schedules,
      truncation at every offset, cap exactness and chunk-independence, window-ceiling
      refusal, all quality levels 0-11, RFC 7932 reference vectors, brotli-oracle
      incremental leg (byte- and shape-identical vs the reference CLI), bit-flip
      agreement with the one-shot decoder, counting-allocator memory bounds

### Shared (custom LZ77) dictionaries and `dcb` (0.4.2)
- [x] `compress_with_dictionary(data, dict, params)`,
      `decompress_with_dictionary(data, dict)`,
      `decompress_with_dictionary_and_limit(data, dict, max_output)`
- [x] `BrotliStream::with_dictionary(Vec<u8>)` / `dictionary()`, surviving `reset()`;
      `BrotliDecompressor::with_dictionary`, `BrotliAsyncDecompressor::with_dictionary`
- [x] `shared_dict::classify_distance` — the three-way distance space
      (`1..=max_backward` output, then the dictionary, then the Appendix A static
      words), established by measurement against `brotli 1.1.0` rather than
      assumed: `p1`/`p2` are *not* seeded from the dictionary, the dictionary sits
      *beyond* the declared window, it stays reachable for the whole stream, and it
      is addressed relative to `max_backward = min(window_size, produced)`.
      A shared-dictionary reference *is* pushed onto the distance ring; a copy that
      runs past the end of the dictionary continues in the produced output at the
      same distance
- [x] `shared_dict::MAX_SHARED_DICTIONARY` (16 MiB), refused by every entry point
- [x] Encoder side: `lz77_compress_with_prefix` seeds the match finder with the
      dictionary, caps matches at the boundary and arbitrates by gain; each
      meta-block is encoded both with and without the dictionary and the smaller
      kept, so attaching a dictionary can never cost ratio. `prefix_len == 0` is
      byte-identical to the dictionary-free encoder (asserted)
- [x] `dcb` module — RFC 9842 framing: `DCB_MAGIC` (`FF 44 43 42`),
      `DCB_HEADER_LEN` (36), `dictionary_id` (SHA-256, from `oxiarc_core::sha256`),
      `write_header`, `parse_header`, `verify_header`, `compress`, `decompress`,
      `decompress_with_limit`
- [x] The same `dcb` items at the crate root under names that say what they frame
      (`parse_dcb_header`, `verify_dcb_header`, `write_dcb_header`, `compress_dcb`,
      `decompress_dcb`, `decompress_dcb_with_limit`, `dictionary_id`, `DCB_MAGIC`,
      `DCB_HEADER_LEN`) — what `oxiarc-http` imports for `Content-Encoding: dcb`;
      `compress`/`decompress` keep their plain-Brotli meaning at the crate root
- [x] Tests: `tests/shared_dictionary.rs` (dictionary sizes 0 / 1 KiB / 64 KiB /
      larger than the window, every quality and lgwin, chunk invariance, boundary
      distances, straddling copies, wrong-dictionary rejection, budget, `reset`,
      the `Read` adapter, a `dcb` fixture assembled from our own encoder, the
      crate-root aliases, and the dictionary id against the FIPS 180-4 vectors),
      the async adapter's dictionary round trip through a 7-byte trickling source
      with a 256-byte staging buffer (`tests/async_brotli_tests.rs`), plus two
      self-skipping `brotli-oracle` legs: 45/45 reference `-D` streams decode
      byte-identically and 48/48 of our `-D` streams are accepted by
      `brotli -d -D`

### Public API
- [x] compress(data, quality) -> BrotliResult<Vec<u8>>
- [x] compress_with_params(data, params) -> BrotliResult<Vec<u8>>
- [x] decompress(data) -> BrotliResult<Vec<u8>>
- [x] BrotliParams configuration struct

## Future Enhancements

### Performance
- [x] **Push-decoder throughput pass (0.4.2).** The window-memory-traffic
      diagnosis in the previous entry was wrong, and disproving it is what
      unlocked the fix: disabling the ring mirror entirely (an incorrect build,
      for measurement only) moved a literal-dense decode by 4 %, while the same
      payload at `lgwin = 10` was 40 % faster — because the *stream* differs, not
      because the ring is smaller. The real costs were, in order: one `memmove`
      **call** per literal run and per short match (`_platform_memmove` was
      12.7 % of the profile — the calls, not the bytes); a 256-byte periodic tile
      built to emit a 5-byte copy; a `Vec::resize` memset of the whole declared
      window; and a `BrotliError` constructed and dropped on every literal by an
      eager `ok_or`. What landed:
      * `SHORT_MATCH = 32` byte-loop paths in `copy_match` and `push_slice`, with
        the ring mask and every bounds check hoisted out when neither end wraps,
        and `#[inline(never)]` bulk halves so the short halves inline;
      * ring sized from the meta-block's declared `MLEN` (`expect_total`) and
        grown by a fresh lazily-zeroed allocation instead of `Vec::resize`;
      * stored meta-blocks longer than the declared window mirror only their
        tail (`push_slice_tail`) — a window only ever needs its last
        `1 << WBITS` bytes;
      * `BrotliStream::decode` runs straight off the caller's slice when nothing
        is held back, so a stored meta-block never copies its bytes into the
        carry at all;
      * the fast command path no longer requires `insert_length * 69` bits of
        buffered input to *enter* — that silently disabled it for every stream
        smaller than a few hundred bytes, and `fast_path_commands()` now asserts
        it engages.
      Reproduce with `cargo run --release --example decode_profile`, which
      reports **min and median** ratios (read the median on a loaded machine) and
      prints a `copy floor` row for the stored shape.
- [x] **Caller's buffer as the newest history segment (0.4.2).** The redesign
      the previous entry proposed is implemented: nothing in the command loop
      writes to the ring any more. Literals and matches go straight to the
      caller's slice, `command::copy_into_pending` resolves a match's *source*
      across the boundary between the ring (everything produced before this
      call) and `out[..at]` (everything produced during it), and `run_commands`
      mirrors the produced bytes into the ring with one bulk `push_slice` at the
      end — which also runs on the error path, so the window stays consistent.
      Verified by a differential test that drives `copy_into_pending` with the
      same history split at every interesting boundary (`mirrored`/`pending` ×
      distance × count) against one flat history.
      Measured, 64 KiB in / 64 KiB out, median of five runs of 25 interleaved
      repetitions: repetitive **9.5x** (lgwin 22) / **24.9x** (lgwin 10), single
      repeated byte **19.7x** / **29.6x**, literal-dominated hex dump **1.21x**,
      stored **0.33x** at lgwin 22 against a **0.34x** measured copy floor and
      **1.01x** at lgwin 10 against **1.01x** — i.e. the stored shape is at its
      physical floor, 97-100 % of it. The `copy floor` row models exactly the
      irreducible work (every byte to the caller, the still-reachable tail to
      the ring) and is timed against the same one-shot decode as every other
      row, so the two ratios share a denominator.
- [ ] Copy-dense streams at large windows: 0.76x, against a 0.85x target — the
      one shape of the four that misses it.
      The 2.94 MB hex-dump payload is literal-dominated only at lgwin 10 (95,605
      copy commands, 1.21x — faster than the one-shot decoder); at lgwin 22 the
      same payload becomes 570,440 copy
      commands of a mean 5.0 bytes at a mean distance of 116,525 — 97 % of the
      output — and 65 % of them read their source out of the ring because the
      distance reaches back past everything produced in the current call.
      (Census taken 2026-09-08 with temporary counters in `copy_into_pending`;
      the function shares below with `sample`, run against one decoder at a time
      through `PROFILE_PUSH_ONLY` / `PROFILE_ONE_SHOT_ONLY`. Neither instrument
      ships, so `decode_profile` alone does not reproduce those two figures —
      the wall-clock ratios it prints are the reproducible part.)
      Diagnosis, all measured rather than argued: the window mirror is free
      (compiling it out moves the row from 21.9 ms to 22.1 ms); the Huffman work
      is identical in the two decoders (`decode_symbol` 29 vs 30.6 units of a
      normalised 100, `decode_distance` 19.1 vs 19.6); the difference is the
      resumable command machinery, spread over half a million 5-byte commands
      (`run` 51 + `copy_into_pending` 16.3 + `memmove` 11.4 + `decode_literal_run`
      10.7 = 89.4 units against the one-shot's single 50-unit loop). Removing the
      per-copy `memmove` call (7.9 % of the profile) was tried and reverted:
      LLVM's loop-idiom pass rewrites a `zip` loop and an 8-byte-chunked loop
      back into the same call, and the wrapping-index loop that does remove it
      (7.9 % -> 0.5 %) costs what the call cost. What is left is a structural
      change — a whole-meta-block fast path that runs the one-shot loop when the
      caller's slice is large enough to hold the rest of the meta-block — which
      would not help the 64 KiB gate row and so was not taken.
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
- [x] Dictionary preloading (shared dictionary) — done, see above
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
  - **Goal:** `async-io` Cargo feature implementing `oxiarc_core::async_io::{AsyncCompressor, AsyncDecompressor}` on `BrotliCompressor`/`BrotliDecompressor`.
  - **2026-09-07:** the decompressor is now bounded — it drives `BrotliStream`
    with a small compressed staging buffer and writes each decoded chunk as it
    is produced. The compressor still reads its input fully before compressing.
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

- Unit tests (lib): 166 — tables/context/dictionary CRC-checked against the
  RFC's own check values; huffman descriptor write/read round-trips;
  decoder primitives (WBITS tree, NBLTYPES VLC, distance ring semantics);
  sliding-window ring checked byte-for-byte against a growing-`Vec` reference
  for every small distance, across the wrap, and through short output slices;
  `copy_into_pending` driven against one flat history at every split of the
  ring/pending boundary; the `dcb` dictionary id against FIPS 180-4
- reference_vectors: 11 (embedded reference-brotli fixtures; always run)
- stream_conformance: 26 — chunk invariance (1-byte in and out), prime chunk
  sizes, `MetaBlockShape` differential vs `decompress_reporting_shapes`,
  truncation at every offset, cap exactness and chunk-independence,
  window-ceiling refusal, bit-flip agreement with the one-shot decoder,
  hand-built metadata meta-blocks, streams larger than the internal carry,
  fault latch, reset isolation
- stream_adversarial: 10 — declared lengths that never arrive (metadata
  `MSKIPLEN`, uncompressed `MLEN`), carry saturation under `Finish`, multi-byte
  corruption differential vs the one-shot decoder, the bulk literal path,
  degenerate window/output ceilings, idle-after-`StreamEnd`
- stream_proptest: 4 (random split schedules vs the one-shot oracle, shape
  preservation, arbitrary bytes, arbitrary truncations)
- stream_adapters: 11 (`Interrupted` retry, `WouldBlock` propagation, output
  before EOF, truncated source, caps through the adapter, a >2 MiB compressed
  stream through the staging buffer)
- shared_dictionary: 16 — every dictionary size × quality × lgwin, chunk
  invariance with a dictionary attached, boundary distances, a hand-built
  straddling copy, wrong-dictionary rejection, budgets, `reset`, the `Read`
  adapter, `dcb` bodies from our own encoder, and the crate-root `dcb` aliases
- brotli_oracle: 17 (differential sweeps vs the `brotli` CLI, including an
  incremental-decode leg asserting byte- *and* shape-identity, rejection of
  every prefix of every reference stream, and both shared-dictionary legs —
  45/45 reference `-D` streams in and 48/48 of ours out; feature-gated,
  self-skipping)
- memory_limit: 7 (counting global allocator; bomb rejection and the
  window-bounded peak of the push decoder and the `Read` adapter)
- corruption_robustness: 4, interop_vectors: 19, high_entropy_roundtrip: 8,
  encoder_bugs: 7, pool: 8, progress_cancel: 10, proptest: 2, async: 17
  (including the async adapter's shared-dictionary round trip), doctests: 21
- Total: 343 tests + 21 doctests passing (with `--all-features`)

## Code Statistics

| File | Lines |
|------|-------|
| compress.rs | 1,356 |
| huffman.rs | 1,335 |
| decompress.rs | 1,272 |
| stream/mod.rs | 1,255 |
| block_split.rs | 1,105 |
| stream/command.rs | 1,048 |
| streaming.rs | 1,033 |
| lz77.rs | 621 |
| bit_reader.rs | 559 |
| pool.rs | 473 |
| dictionary.rs | 468 (+ 122,784-byte `dict_data.bin`) |
| stream/window.rs | 421 |
| async_brotli.rs | 386 |
| parallel.rs | 361 |
| lib.rs | 339 |
| tables.rs | 313 |
| context.rs | 268 |
| bit_writer.rs | 234 |
| dcb.rs | 205 |
| shared_dict.rs | 179 |
| stream/meta.rs | 170 |
| error.rs | 128 |
| stream/budget.rs | 93 |

Total: 13,622 lines of Rust in `src/` (every file well under the
2,000-line ceiling), plus the 122,784-byte Appendix A dictionary blob.

## Known Limitations

1. **Encoder ratio gap vs reference**: one prefix code per category per
   meta-block; no block splitting, no context modeling (NTREES > 1), no
   static-dictionary reference emission, no ring-distance codes 1-15.
   Within a few percent of reference q6 on typical text, but notably behind
   at q10-11 and on structured binary data. (The *decoder* handles all of
   these features.)
2. ~~Streaming types buffer the whole input/output in memory; not
   bounded-memory streaming.~~ — **Fixed 2026-09-07** for the decode side:
   `BrotliStream` is a real push decoder and `BrotliDecompressor<R>` /
   `BrotliAsyncDecompressor` are thin adapters over it, bounded by the declared
   sliding window (asserted with a counting allocator in
   `tests/memory_limit.rs`). `BrotliAsyncCompressor` still reads its input fully
   before compressing; `BrotliCompressor<W>` was already incremental.
3. No shared/custom dictionary support yet (RFC 8478-style shared brotli
   dictionaries; needed before `oxiarc-http` can support the `dcb`
   content-coding)
4. ~~Quality-1 encoder produces incorrect output for repeated-pattern data~~ — **Fixed 2026-05-17** (copy_length tail guard in `build_insert_copy_commands`)
5. ~~Multi-block encoder is broken: inputs > 256 KiB at quality 4 (> 1 MiB at quality 5+) produce invalid bitstreams~~ — **Fixed 2026-05-17** (same root cause as #4)
6. ~~High-entropy / incompressible data fails to decode at some quality levels ("invalid Huffman code: no matching code found" / truncated insert lengths)~~ — **Fixed 2026-06-06** (package-merge length-limited Huffman codes + unified insert-length code table covering inserts up to ~4 MiB)
