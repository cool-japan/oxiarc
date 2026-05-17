
# oxiarc-deflate - Development Status (v0.3.0, 2026-05-16)

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
- [ ] Pre-allocated output buffers

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
- [ ] Fuzzing tests
- [ ] Edge case handling (empty input, max length matches)

## Test Coverage

- inflate: 8 tests
- deflate: 7 tests
- huffman: 4 tests
- lz77: 7 tests
- tables: 7 tests
- zlib: 27 tests
- streaming: ~15 tests (gzip stream, zlib stream)
- async_deflate: ~12 tests
- gzip: ~7 tests
- edge_cases: 14 tests (integration test)
- Total: 126 tests

## Code Statistics

| File | Lines |
|------|-------|
| optimal.rs | ~534 (NEW in v0.3.0) |
| streaming.rs | 1,047 (NEW) |
| zlib.rs | 931 |
| huffman.rs | 438 |
| lz77.rs | 371 |
| inflate.rs | 349 |
| deflate.rs | 347 |
| tables.rs | 311 |
| async_deflate.rs | ~300 |
| gzip.rs | ~250 |
| lib.rs | ~135 |
| **Total** | **~3,479** |

## Known Limitations

1. Single-threaded only
