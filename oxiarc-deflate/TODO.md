
# oxiarc-deflate - Development Status (v0.4.2, 2026-09-07)

## Completed Features (COMPLETE)

### Huffman Trees (438 lines)
- [x] Canonical Huffman code generation
- [x] Tree building from code lengths
- [x] Fast table-based decoding
- [x] `HuffmanBuilder` for creating trees from frequencies
- [x] Length-limited code generation
- [x] Reverse bit order for DEFLATE

### LZ77 Encoder (371 lines)
- [x] 32KB sliding window
- [x] Hash chain for pattern matching (3-byte hash)
- [x] Minimum match length: 3 bytes
- [x] Maximum match length: 258 bytes
- [x] Lazy matching for better compression
- [x] Compression level support (0-9)
- [x] `Lz77Token` enum (Literal/Match)

### Fixed Huffman Tables (311 lines)
- [x] Literal/Length code lengths (RFC 1951)
- [x] Distance code lengths
- [x] Length extra bits table
- [x] Distance extra bits table
- [x] Length base values (3-258)
- [x] Distance base values (1-32768)
- [x] Pre-computed fixed trees

### Inflate (349 lines)
- [x] Block type 00: Stored (uncompressed)
- [x] Block type 01: Fixed Huffman codes
- [x] Block type 10: Dynamic Huffman codes
- [x] BFINAL flag handling
- [x] Code length decoding for dynamic blocks
- [x] End-of-block (symbol 256) detection
- [x] Length/distance decoding
- [x] Extra bits handling
- [x] Streaming interface
- [x] One-shot `inflate()` function

### Deflate (347 lines)
- [x] Fixed Huffman encoding
- [x] LZ77 token encoding
- [x] Block header writing
- [x] End-of-block marker
- [x] Compression levels 0-9
- [x] Stored blocks (level 0)
- [x] Streaming interface
- [x] One-shot `deflate()` function

## Completed Features (Phase 2)

### Dynamic Huffman Compression
- [x] Build optimal Huffman trees from data
- [x] Emit dynamic block headers
- [x] Decide between fixed/dynamic per block
- [x] Code length encoding (RLE with 16,17,18)
- [x] Frequency counting and code generation
- [x] Size estimation for block type selection

### Performance Optimizations (Latest)
- [x] Improved hash function with better avalanche properties
- [x] Optimized match finding with early rejection tests
- [x] Loop unrolling for first 3 bytes in match comparison
- [x] Fixed large input handling with proper window sliding
- [x] Performance benchmarks (lz77_bench)
  - Level 1: 48-400 MB/s (depending on data type)
  - Level 5: 13-275 MB/s
  - Level 9: 0.3-253 MB/s
  - Up to 246x compression ratio on highly compressible data

## Completed Features (Phase 3)

### Streaming Compression/Decompression (NEW in 0.2.6)
- [x] GzipStreamEncoder (Write trait, buffered streaming compression)
- [x] GzipStreamDecoder (Read trait, eager-read streaming decompression)
- [x] ZlibStreamEncoder (Write trait, Zlib streaming compression)
- [x] ZlibStreamDecoder (Read trait, Zlib streaming decompression)
- [x] Configurable block size (default 128 KiB, via with_block_size())
- [x] Produces concatenated GZIP/Zlib members
- [x] Zero-copy streaming pipeline design

## Completed Features (Phase 8 — resumable inflate, 0.4.2)

### Resumable core (W1-A1)
- [x] `InflateStream` — 14-state raw-DEFLATE push decoder; an arbitrary byte
      split of a stream yields byte-identical output
- [x] `WrappedInflate` — gzip / zlib / raw / auto framing, multi-member,
      `TrailingPolicy::{Reject, AllowZeros, Stop}`, `GzipHeaderInfo`
- [x] `with_max_output` / `with_ratio_guard` enforced **inside** a block
- [x] `BitCache::{refill_bulk, refill_bytes, align_to_byte, take_byte}` in
      oxiarc-core; `HuffmanTree::rebuild_from_code_lengths` (in-place reuse)

### Adapters and re-basing (W1-A2)
- [x] `InflateReader<R: Read>` — 64 KiB output staging (mandatory),
      `Interrupted` retry, `WouldBlock` propagated, inner `Ok(0)` switches to
      `FlushMode::Finish`, no-progress guard is an `Err`, zlib
      "fewer than 6 unconsumed bytes at EOF" rule
- [x] `AsyncInflateReader<R: AsyncRead>` (feature `async-io`) — same rules,
      `Poll::Pending` propagated
- [x] `GzipStreamDecoder` / `ZlibStreamDecoder` re-based: incremental, no
      `read_to_end`, bounded memory; `with_max_output` enforced during decode;
      `decompressed_size()` now means "produced so far"
- [x] `Decompressor for Inflater` re-based (`FlushMode::Finish`, sticky fault
      latch) — a second call after a mid-stream EOF errors instead of
      returning `Ok((0, n, Done))` with truncated output
- [x] `RawInflateReader` (RFC 4978) re-based on `FlushMode::None`; no more
      snapshot/restore re-decoding across partial TCP segments
- [x] `async_deflate`'s `AsyncDecompressor` is a bounded pump (2 x buffer_size)
- [x] `inflate()`, `inflate_into()` and `gzip_decompress()` routed through the
      new core; `gzip_decompress` now verifies `FHCRC`
- [x] `zlib_decompress`/`zlib_decompress_into` documented as exact-slice
      functions (Adler-32 read from the last 4 input bytes), with a
      regression test pinning it

## Future Enhancements

### Advanced LZ77
- [x] Better hash function (4-byte hash) — already implemented in v0.2.8
- [x] Optimal parsing (graph-based) — Zopfli-style OptimalParser with iterative cost retraining (done 2026-05-16)
- [x] Match filtering heuristics + nice match length parameter (planned 2026-05-17)
  - **Goal:** Expose two well-known zlib LZ77 tuning knobs on the DEFLATE encoder: `nice_match_length` (early-exit when any match ≥ this length is found) and `max_chain_length` / `good_length` (cap on hash-chain walks, with a tighter cap once a "good enough" match is found).
  - **Design:**
    - Add fields `nice_length: u16`, `max_chain: u32`, `good_length: u16` to `Deflater` (default tuning table mirrors zlib's `configuration_table` indexed by level — values for level 1..9 in `src/lz77.rs`).
    - Builder API: `Deflater::with_lz77_params(self, nice_length: u16, max_chain: u32, good_length: u16) -> Self` plus `Deflater::with_lz77_preset(self, preset: Lz77Preset) -> Self` for `Lz77Preset::{Fast, Default, Best, Ultra}`.
    - In the match-finder loop (`lz77::find_longest_match`): (1) if `current_best_length >= nice_length` → break; (2) if `current_best_length >= good_length`, halve `max_chain` for remainder of hash-chain walk.
    - **No semantic change to output for default-level encoders** — the default per-level numbers reproduce existing encoder output bit-for-bit.
  - **Files:** `oxiarc-deflate/src/lz77.rs` (match-finder loop, configuration table), `oxiarc-deflate/src/deflate.rs` (Deflater builder), `oxiarc-deflate/src/lib.rs` (re-export `Lz77Preset`), `oxiarc-deflate/TODO.md`.
  - **Prerequisites:** none.
  - **Tests:**
    - Regression: existing roundtrip tests at every level must produce byte-identical output to pre-change for the default tuning table.
    - Speed-vs-ratio: `Lz77Preset::Fast` produces output ≥ 95% the size of `Lz77Preset::Default`.
    - Edge case: `nice_length = u16::MAX` → behaves like un-capped match finder.
    - Edge case: `nice_length = 3` → encoder emits very short matches and output remains decodable.
  - **Risk:** changing the match finder is highest-risk. Mitigation: keep existing code path as default; only new builder methods can change behavior.

### Performance
- [ ] SIMD-accelerated hash computation
- [x] Multi-threaded compression (planned 2026-05-17)
  - **Goal:** Implement the already-declared `parallel` Cargo feature for oxiarc-deflate. Output is a valid GZIP stream consisting of N concatenated GZIP members, one per chunk, decodable by any conforming gzip reader. Mirrors pigz behavior at the format level.
  - **Design:**
    - New module `oxiarc-deflate/src/parallel.rs` (gated by `#[cfg(feature = "parallel")]`).
    - Public API: `pub fn gzip_compress_parallel(input: &[u8], level: u32, chunk_size: usize) -> Vec<u8>` plus a builder `ParallelGzipEncoder { level, chunk_size, num_threads: Option<usize> }`.
    - Algorithm: chunk input by `chunk_size` (default 1 MiB; minimum 64 KiB); each rayon worker compresses one chunk as an **independent GZIP member** (header + DEFLATE stream + CRC32 + ISIZE); serial assembly concatenates the members in order. ISIZE per member equals that member's uncompressed length (mod 2³²); a final 0-byte member is NOT appended.
    - DEFLATE inside each member is the existing serial encoder at `level`; the encoder must emit BFINAL=1 on its last block. No cross-chunk LZ77 dictionary sharing in this first cut.
    - Re-export: `pub use parallel::{gzip_compress_parallel, ParallelGzipEncoder}` in `lib.rs` under the same `#[cfg]`.
  - **Files:** `oxiarc-deflate/src/parallel.rs` (new), `oxiarc-deflate/src/lib.rs` (re-export under `parallel` feature), `oxiarc-deflate/Cargo.toml` (verify `parallel = ["dep:rayon"]` exists), `oxiarc-deflate/TODO.md`.
  - **Prerequisites:** none — `gzip` module and `Deflater` already exist; rayon already in workspace deps.
  - **Tests:**
    - Roundtrip via serial `GzipDecoder` on chunked outputs at levels 1, 5, 9.
    - Equivalence test: parallel output decompresses to byte-identical original for 1 MiB, 5 MiB, 100 KiB (sub-chunk), and 1 byte inputs.
    - Determinism test: same input → same output (rayon's stable order preserved by serial assembly).
  - **Risk:** multi-member outputs are bigger than single-member at small chunk sizes. Mitigation: default to 1 MiB chunks to amortize overhead under 0.002%.
- [x] Memory pool for allocations (planned 2026-05-17)
  - **Goal:** Thread-safe buffer pool for the per-encode allocations of DEFLATE: the 32 KiB sliding window, the ~64 KiB hash chain head/prev arrays, and the per-block literal/length frequency tables. Mirrors `oxiarc-lzma::LzmaPool` (memory-pool primitive added in 0.3.1).
  - **Design:**
    - New module `oxiarc-deflate/src/pool.rs` with `DeflatePool` (capacity-bucketed pool), `PooledBuf<'a>` RAII wrapper (returns the buffer on drop), and `Deflater::with_pool(&DeflatePool) -> Deflater` builder.
    - Bucket sizes: `WINDOW_BUF` (32 KiB), `HASH_HEAD` (32 KiB × `u16`), `HASH_PREV` (32 KiB × `u16`), `OUTPUT_SCRATCH` (defaults to 8 KiB, grows as needed).
    - Internals: each bucket is a `Mutex<Vec<Vec<u8>>>` with a configurable per-bucket cap (default 4 buffers).
    - When `Deflater::with_pool` is set, the encoder pulls buffers via `pool.get(BucketId)` instead of `Vec::with_capacity`; on drop the `PooledBuf` returns them.
    - No-pool path is preserved (existing allocation behavior is the default; pool is strictly opt-in).
  - **Files:** `oxiarc-deflate/src/pool.rs` (new), `oxiarc-deflate/src/deflate.rs` (Deflater integration), `oxiarc-deflate/src/lz77.rs` (window/hash-chain allocation sites), `oxiarc-deflate/src/lib.rs` (re-export `DeflatePool`, `PooledBuf`), `oxiarc-deflate/TODO.md`.
  - **Prerequisites:** none — `LzmaPool`'s structure in oxiarc-lzma is the reference.
  - **Tests:**
    - Pool basic: three sequential `Deflater::with_pool` runs reuse the same window buffer (assert via `pool.stats()` counters).
    - Roundtrip equality: pooled and non-pooled `Deflater` at the same level produce byte-identical output.
    - Concurrent pool: 8 rayon threads each compress a 1 MiB input via the same pool; total allocations < 16 buffers.
    - Pool boundary: per-bucket cap respected (cap of 2 → third buffer beyond cap is dropped, not returned).
  - **Risk:** stale buffer contents being read as uninitialized data. Mitigation: `PooledBuf::get_mut` zeroes the slice before handing back to caller.
- [x] Pre-allocated output buffers (done 2026-07-30) — `Inflater::with_output_capacity(size_hint)` pre-sizes the decompressor's output buffer from a size hint (clamped to the new `MAX_OUTPUT_CAPACITY_HINT` = 64 MiB); GZIP decoding seeds this automatically from the trailing ISIZE field. Encoder-side (`Deflater`) output is still a plain growable `Vec`.

### Features
- [x] Zlib wrapper (RFC 1950)
  - [x] Adler-32 checksum implementation
  - [x] Zlib header (CMF/FLG bytes)
  - [x] Compression level indicator
  - [x] Streaming ZlibCompressor/ZlibDecompressor
- [x] Gzip wrapper integration
- [x] Custom dictionary support
  - [x] Deflater.with_dictionary() and set_dictionary()
  - [x] Inflater.with_dictionary() and set_dictionary()
  - [x] zlib_compress_with_dict() and zlib_decompress_with_dict()
  - [x] FDICT flag support in zlib header
  - [x] Dictionary checksum verification (Adler-32)
- [x] Flush modes (sync_flush, full_flush, partial_flush for GzipStreamEncoder/ZlibStreamEncoder, v0.2.6)

### Compliance
- [x] Round-trip testing (zlib/gzip format compliance, 2026-05-17)
- [x] `proptest`-based round-trip test suite (`tests/proptest_roundtrip.rs`: `roundtrip`, `inflate_never_panics`) (done 2026-07-08)
- [x] Fuzzing tests (cargo-fuzz style; proptest round-trip suite above is a related but distinct property-based check) — `fuzz/fuzz_targets/fuzz_inflate.rs` (4.1M corpus), `fuzz_inflate_into.rs` (1.9M corpus, added in the 0.4.0 cycle), `fuzz_zlib_header.rs` (3.4M corpus), and `fuzz_gzip_header.rs` (5.0M corpus) at the workspace `fuzz/` root
- [x] Edge case handling (empty input, max length matches) (completed 2026-07-07) — both cases already correct (empty-input special case in write_stored_blocks; length 258→code 285 in length_to_code); added decoder-only hand-built length-258 vector to close the coverage gap.

## Test Coverage

`cargo nextest run -p oxiarc-deflate --all-features` + `cargo test --doc -p oxiarc-deflate --all-features`
(verified 2026-09-07): **422 tests** — 381 nextest (217 in-crate unit tests +
164 integration tests) and 41 doctests. Zero failures, zero skips.

Integration suites:

| Suite | Tests | Covers |
|-------|-------|--------|
| `inflate_stream` | 50 | Split invariance (S1-S8), format coverage (F1-F12), robustness (R1-R17) for `InflateStream`/`WrappedInflate` |
| `compliance` | 32 | RFC 1951 block types, spec-inflater cross-checks, parallel GZIP round-trips |
| `inflate_reader` | 23 | `Interrupted`/`WouldBlock`, truncation as `io::Error`, the zlib short-tail rule, raw padding-bit vs trailing-byte framing, read granularity, the `Decompressor` sticky-fault two-call regression |
| `wrapper_regressions` | 16 | DEFLATE-01..05, with CPython gzip/zlib fixtures |
| `inflate_differential` | 15 | Seven decode paths compared byte-for-byte over the corpus at four levels |
| `edge_cases` | 14 | Empty input, maximum-length matches, boundary sizes |
| `zlib_oracle` | 12 | Live CPython `zlib`/`gzip` + system `gzip` CLI differential, both directions, both `Read` adapters (self-skipping) |
| `proptest_roundtrip` | 2 | Property-based round-trip and no-panic |

In-crate unit tests by module: streaming 40, zlib 30, parallel 19, lz77 18,
deflate 15, window 14, sink 11, optimal 10, huffman 9, tables 7, inflate 7,
reader 6, pool 6, gzip 6, stream 5, wrapper 4, raw_stream 4, async_deflate 4,
async_reader 2.

## Code Statistics

Lines per file (`wc -l oxiarc-deflate/src/*.rs`, verified 2026-09-07; every
file is under the 2000-line policy limit, and under the 1500-line target):

| File | Lines |
|------|-------|
| deflate.rs | 1,372 |
| streaming.rs | 1,346 |
| lz77.rs | 1,236 |
| inflate_core.rs | 1,164 |
| wrapper.rs | 1,138 |
| inflate.rs | 1,127 |
| zlib.rs | 1,097 |
| huffman.rs | 1,050 |
| stream.rs | 717 |
| reader.rs | 595 |
| window.rs | 578 |
| sink.rs | 577 |
| parallel.rs | 562 |
| pool.rs | 534 |
| optimal.rs | 521 |
| tables.rs | 343 |
| async_reader.rs | 316 |
| async_deflate.rs | 312 |
| raw_stream.rs | 274 |
| gzip.rs | 268 |
| lib.rs | 113 |
| **Total** | **15,240** |

## Known Limitations

1. Single-threaded only for the plain `Deflater`/`Inflater` batch path; the `parallel` feature enables multi-threaded GZIP/DEFLATE via `gzip_compress_parallel`/`compress_deflate_parallel`/`ParallelGzipEncoder`

2. `Inflater::inflate<BitReader<R>>` / `inflate_consumed` still run the
   pre-0.4.2 symbol loop (`inflate_block_into`). This is deliberate: ZIP's
   data-descriptor path (`oxiarc-archive/src/zip/stream.rs`) builds an
   **exact-mode** `BitReader` and keeps reading from it after the DEFLATE
   stream ends, which the push core cannot reproduce without
   `BitReader::push_back`. The two loops are cross-checked on every corpus
   entry by `tests/inflate_differential.rs::all_decode_paths_agree`.
3. `Decompressor::decompress` is a **whole-remaining-input** contract: each
   call must receive all the compressed input still available, and a slice
   ending mid-symbol is an error. Callers with genuine chunks must use
   `InflateStream`/`WrappedInflate` or the `InflateReader` adapters, where
   the flush mode is explicit. `AsyncDecompressorWrapper<Inflater>` cannot
   satisfy this and is documented as unsupported — use `AsyncInflateReader`.
4. `max_output` and the ratio guard bound **one stream** and are cleared by
   `reset()`. A container that resets the decoder per frame or per strip
   (APNG, TIFF) must carry its own file-level budget.
