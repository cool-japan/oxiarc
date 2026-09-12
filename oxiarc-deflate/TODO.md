
# oxiarc-deflate - Development Status (v0.4.2, 2026-09-08)

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

## Completed Features (Phase 8 — zlib-equivalent encoder, 0.4.2)

### Encoder core rewritten as a zlib port (W3-DEFENC)
- [x] `src/encoder/` — a faithful port of zlib's `deflate.c` + `trees.c`:
      `config.rs` (window geometry + `configuration_table`), `tables.rs`
      (length/distance/static-tree tables), `window.rs` (sliding window, slid
      hash chains, `longest_match`, `match_candidates`), `trees.rs`
      (`build_tree`/`gen_bitlen`/`gen_codes`/`scan_tree`/`send_tree`/
      `flush_block` + `BitSink`), `stored.rs` (`deflate_stored`),
      `optimal.rs` (the DP parser), `mod.rs` (the driver and
      `deflate_fast`/`deflate_slow`/`deflate_rle`/`deflate_huff`)
- [x] Output is **byte-identical to CPython `zlib.compress(data, level)`** at
      every level 1-9 on every corpus (previously up to 179 % larger)
- [x] Hash-chain match finder with the correct `cur_match > limit` termination
      (the old encoder spun the whole chain budget on the `NIL == 0` tail),
      `good_length` chain quartering, `nice_length` early exit, `TOO_FAR`
- [x] Lazy matching exactly as zlib (`match_length <= prev_length` emits the
      previous match; the lazy search itself is skipped once
      `prev_length >= max_lazy`)
- [x] Per-block stored / fixed / dynamic choice on real bit costs, blocks cut
      at 16 383 symbols, `Z_FILTERED` / `Z_HUFFMAN_ONLY` / `Z_RLE` / `Z_FIXED`
      via `Deflater::with_strategy`
- [x] Bit-continuous multi-call behaviour with **no per-call fixed cost**: a
      2 MiB stream costs 34.4 ms in 1 KiB calls vs 33.0 ms in 1 MiB calls and
      produces the same bytes (previously 13x more for small calls)
- [x] Level-6 throughput 0.77x-1.53x of CPython `zlib` (was 0.10x-0.40x)
- [x] `Deflater::with_optimal_parsing` is never larger than the default ladder
      at the same level: the candidate set now honours `TOO_FAR` (without it
      the DP bought rare long-distance codes for 3-byte matches and lost 2 % on
      noisy image rows), and each span keeps the cheaper of the DP path and a
      zlib-equivalent lazy parse judged on the real block cost
- [x] `tests/zlib_encoder_oracle.rs` (byte-identity oracle, `zlib-oracle`
      feature), `tests/encoder_behaviour.rs` (21 mechanism tests, no external
      tool), `tests/common/blocks.rs` (independent block walker),
      `examples/zlib_ab.rs` (criterion-free ratio/throughput/per-call table)

## Completed Features (Phase 8 — inflate throughput, 0.4.2)

### Packed decode tables and a rewritten fast loop (W3-INFLATEPERF)
- [x] `src/decode_table.rs` — a crate-private, decode-only two-level table
      whose `u32` entries carry the symbol *kind* (literal / end-of-block /
      sub-table / invalid), the code length, the extra-bit count and the
      payload (literal byte, length base, distance base, sub-table offset).
      Removes two `*_EXTRA_BITS` lookups, two `decode_*` base lookups and the
      `<256 / ==256 / >285` comparison chain from every symbol. `HuffmanTree`
      is untouched — it stays the crate's public Huffman type, the encoder's,
      and the bit-at-a-time fallback's; the 19-symbol code-length alphabet
      still decodes through it.
- [x] Built by **zlib's `inflate_table` algorithm**: symbols counting-sorted
      into canonical order and the table filled in one pass with the reversed
      code maintained by a backwards increment, instead of reversing every
      symbol's code with a per-bit loop twice. Sub-tables are created on
      demand and sized from the code space still unaccounted for.
- [x] Proven equal to `HuffmanTree` for **every bit pattern** of every shape
      it is built from (`decode_table_agrees_with_huffman_tree`, plus a
      48-shape pseudorandom sweep and an in-place-rebuild differential), and
      then over a further 460 shapes / 12.5 M bit patterns
      (`a_wide_shape_sweep_agrees_with_huffman_tree`: single-symbol alphabets
      at every length 1..=15, whole populations pinned to one length, 200
      randomised shapes and the same shapes punched full of holes) plus 64
      in-place rebuilds each compared entry-for-entry with a fresh build
- [x] Fixed tables built once per process (`OnceLock`), never per block
- [x] Steady-state allocation-free: scratch reuse plus a power-of-two
      sub-table reservation. Resident scratch *fell* — the two per-tree
      root-sized scratch vectors (5 KiB) are gone, replaced by one
      alphabet-sized `Vec<u16>`; `oxiarc-http`'s streaming peak went from
      ~216 KiB to 178 KiB against its 224 KiB budget.
- [x] `fast_symbols` rewritten: one bulk refill per iteration (>= 56 bits,
      checked once as a single invariant so no inner step needs an
      availability test), one table lookup per symbol, up to three literals
      per 32-bit peek with a single `consume`, and the bit accumulator, the
      output cursor and the input cursor all in locals. `#[inline(never)]`,
      so the loop does not share a register allocation with the careful path
      (before: a store of the input cursor per refill and a stack reload of
      the input pointer per iteration).
- [x] Match copies in machine words: short non-overlapping matches inline in
      8-byte chunks (never `memmove`, whose call costs more than the copy at
      DEFLATE's average match length), byte fill for distance 1, word tiling
      with pattern doubling for distances 2-7, and `copy_within` above 64
      bytes (`WORD_COPY_MAX`) where the vector `memmove` wins. **No write ever
      goes past the reported output** — the caller's buffer tail is untouched,
      as before. The half of a match that starts in the history window and
      finishes inside the caller's buffer goes through
      `copy_straddling_match`, out of line, and its tail is the *bounds
      checked* copy: it resumes at a cursor the fast loop's once-per-iteration
      output guard never saw.
- [x] The growable path (`inflate()` / `InflateStream::inflate_to_vec`)
      decodes into the tail of the buffer it is filling through
      `BoundedSink::resumed`, so the window is written once and
      back-references resolve in place; the first buffer is sized from the
      input instead of always starting at 64 KiB
- [x] `examples/inflate_ab.rs` — interleaved A/B of all four decode entry
      points against CPython `zlib.decompress` over six data shapes x three
      sizes x levels 1/6/9, every arm's output checked before it is timed
- [x] `benches/deflate_bench.rs::inflate_shapes` — per-shape decode
      throughput (image rows, PNG-filtered rows, text, long matches, stored)
- [x] Measured by running the pre-track binary (commit `9a3b7bc`, built as a
      standalone snapshot) and the current one **alternately** on the same
      corpora, so a shared machine cannot favour one side. All figures are
      medians (median across the interleaved rounds of each round's own
      median): on the four decode-bound shapes `inflate_into` is
      **1.04x-3.3x faster** and the worst-arm-vs-python ratio improves in
      all 36 measured rows (the two `memcpy`-bound shapes scatter within
      noise). Against the
      ">= 0.60x" gate: met on every shape and level at 1 MiB (0.65x-2.92x),
      on four of six shapes at 64 KiB (`rgb8-image-rows` 0.46x at L6,
      `many-blocks-text` 0.59x at L9) and on three of six at 16 MiB
      (png-filtered 0.51x-0.60x, rgb8 0.58x-0.67x, text 0.58x-0.77x). On the
      load-robust best-of-round figure every 64 KiB and 1 MiB shape clears
      0.60x and five of six do at 16 MiB. Re-measured 2026-09-08 (three more
      full 54-row passes, interleaved, load 60-71): png-filtered at 16 MiB
      lands at 0.47x-0.67x on medians and 0.48x-0.57x on best-of-round — same
      band, same verdict. Reference: CPython 3.14 linking
      Apple's tuned `/usr/lib/libz.1.dylib` 1.2.12; full tables in
      `README.md`.
### Defects found and fixed by the adversarial verification pass

- [x] **Silent output corruption at the output-buffer edge.** A match that
      starts in the history window resumes inside `dst` at a cursor the fast
      loop's once-per-iteration output guard never saw — exactly `distance`,
      which can be within eight bytes of the end of the caller's buffer. The
      word-wide tail store there wrote *nothing* (`store_u64` is bounds
      checked), so the last one to seven bytes of the match were left as
      whatever the caller's buffer already held while the decoder reported
      them as produced (a `debug_assert` caught it only in debug builds).
      Reachable from `InflateStream::inflate` with any output buffer of
      32 KiB or less — the shape `oxiarc-png` uses for IDAT rows,
      `oxiarc-tiff` for strips and `oxiarc-http` for body chunks. The
      straddling copy now lives in `copy_straddling_match` and its tail is
      the bounds-checked `copy_within_dst`. Pinned by
      `tests/inflate_fast_boundary.rs` (crafted fixed-Huffman streams that
      end a match at every offset in the last words of buffers of eight
      different sizes) and by
      `fast::tests::a_straddling_copy_is_exact_including_at_the_end_of_the_buffer`.
- [x] **Panic / non-terminating loop on a crafted dynamic block.** The port
      of zlib's `inflate_table` kept `root = min(root, max_len)` but dropped
      `if (min > root) root = min`. An alphabet whose *shortest* code exceeds
      the root index is necessarily **incomplete** (a complete code with every
      code >= 11 bits needs 2048 symbols; the alphabet has 288), and zlib
      rejects incomplete sets outright — verified: CPython answers
      `zlib.error: Error -3 ... invalid literal/lengths set`. That is why no
      oracle in the suite covered it. This crate deliberately tolerates
      incomplete codes (as `HuffmanTree` always has, and `DecodeTable` is
      required to agree with it), so for it the shape is reachable —
      `HDIST = 1` with a single 15-bit distance code, say. The replication
      stride then exceeded the
      table size and `fill -= incr` wrapped: a subtract-overflow panic in
      debug, and in release a fill loop whose counter never reaches zero
      (confirmed: a release test timed out after 120 s). The sub-table
      creation now runs *before* the first fill instead of after it, which
      leaves every non-degenerate shape byte-identical while giving the
      degenerate ones the sub-table their codes need. It is a **0.4.2
      regression**, not a pre-existing hole: the crate's other decoder,
      `Inflater` (the pre-0.4.2 `HuffmanTree` symbol loop), decodes the
      fixture correctly. Pinned by
      `tests/inflate_fast_boundary.rs::a_block_whose_shortest_code_exceeds_the_table_root_is_handled`,
      by `a_long_code_block_agrees_with_the_huffman_tree_decoder` (the same
      shape carrying real output, decoded through the packed-table path, the
      push decoder and the independent `HuffmanTree` decoder, all three
      required to agree, plus an assertion that CPython still rejects it), and
      by the 460-shape `HuffmanTree` differential — which now carries a
      vacuity guard requiring 400+ shapes, 10 M+ bit patterns and all 11
      degenerate shapes.
- [x] Both fixes A/B'd against the pre-fix binary, interleaved in both
      orders over 12 rounds: every shape within ±1.3 %, against a ±2-3 %
      noise floor measured on the `memcpy`-bound rows.

- [x] Changes measured and **rejected**, all reproduced across rounds: an
      11-bit literal/length root (libdeflate's choice) costs 7 % on image
      rows; a branchless two-literal commit costs 7 % on image rows (it puts
      a second dependent table load on every match's critical path too);
      folding the distance decode into the length's peek costs 8 % on
      PNG-filtered rows; and shrinking the fast loop's output margin below
      the longest match — worth ~3 % on a push decoder — changes how many
      trailing bytes land in the accumulator, which
      `reset_clears_the_accumulator_but_next_member_keeps_it` pins. Each is
      documented at the constant or the step it concerns.

## Future Enhancements

### Advanced LZ77
- [x] Better hash function (4-byte hash) — already implemented in v0.2.8
- [x] Optimal parsing (graph-based) — Zopfli-style OptimalParser with iterative cost retraining (done 2026-05-16; fed the full `TOO_FAR`-filtered candidate set and guarded by a real-block-cost comparison against a zlib-equivalent lazy parse, 2026-09-08)
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
(re-measured 2026-09-08 by the docs track, from the real per-binary nextest
output, after DEFENC/DEFENC-verify's encoder rewrite and INFLATEPERF/
INFLATEPERF-verify's fast-loop work both landed, plus the FINALGATE F5
regression test): **507 tests** — 455
nextest (214 in-crate unit tests + 241 integration tests across 15 files)
and 52 doctests. Zero failures, zero skips.

Integration suites (exact per-binary counts from a real `nextest` run, not
estimated):

| Suite | Tests | Covers |
|-------|-------|--------|
| `inflate_stream` | 50 | Split invariance (S1-S8), format coverage (F1-F12), robustness (R1-R17) for `InflateStream`/`WrappedInflate` |
| `compliance` | 32 | RFC 1951 block types, spec-inflater cross-checks, parallel GZIP round-trips |
| `inflate_reader` | 31 | `Interrupted`/`WouldBlock`, truncation as `io::Error`, the zlib short-tail rule, raw padding-bit vs trailing-byte framing, read granularity, the `Decompressor` sticky-fault two-call regression |
| `encoder_behaviour` | 23 | (**new, DEFENC**) Level table, lazy-vs-greedy decisions, `max_lazy`/`TOO_FAR`, block-type selection through an independent block walker, call-size invariance, cross-call matching, the optimal parser over every corpus — no external tool needed |
| `wrapper_regressions` | 17 | DEFLATE-01..05, with CPython gzip/zlib fixtures, plus the FINALGATE F5 Adler-32-is-not-a-CRC message regression |
| `inflate_differential` | 15 | Seven decode paths compared byte-for-byte over the corpus at four levels |
| `edge_cases` | 14 | Empty input, maximum-length matches, boundary sizes |
| `zlib_oracle` | 13 | Live CPython `zlib`/`gzip` + system `gzip` CLI differential, both directions, both `Read` adapters (self-skipping) |
| `adversarial_verify` | 13 | Mutation/truncation robustness sweeps |
| `zlib_encoder_oracle` | 11 | (**new, DEFENC**) Byte-identity with CPython `zlib.compress`/`compressobj` at every level and strategy, behind `zlib-oracle` |
| `inflate_fast_boundary` | 11 | (**new, INFLATEPERF/INFLATEPERF-verify**) Hand-built fixed-Huffman streams pinning the straddling-match buffer-edge fix and the malformed-Huffman-table-root fix, both mutation-checked to fail in a **release** build with either fix reverted |
| `encoder_adversarial` | 8 | (**new, DEFENC-verify**) 26 boundary sizes × 11 shapes × every level/strategy/call pattern/flush mode/dictionary |
| `proptest_roundtrip` | 2 | Property-based round-trip and no-panic |
| `allocation_steady_state` | 1 | (**new, DEFENC-verify**) Counting global allocator: `InflateStream` allocates zero times in the steady state across chunk/output-buffer shapes |

In-crate unit tests: 214, spread across the modules the encoder rewrite and
the fast-loop work both touched (`encoder/*`, `decode_table`, `fast`, plus
every pre-existing module below) — not re-broken-down per module this pass
(the docs track's own scope is cross-file consistency, not a full internal
audit); the per-suite table above is the exact, re-measured figure that
matters for cross-referencing against the root `TODO.md`/`README.md`
totals.

## Code Statistics

Lines per file (`wc -l oxiarc-deflate/src/*.rs oxiarc-deflate/src/inflate_core/*.rs`,
verified 2026-09-08; every file is under the 2000-line policy limit and
under the 1500-line target):

| File | Lines |
|------|-------|
| wrapper.rs | 1,396 |
| streaming.rs | 1,346 |
| huffman.rs | 1,216 |
| zlib.rs | 1,199 |
| inflate.rs | 1,166 |
| inflate_core.rs | 1,054 |
| stream.rs | 935 |
| sink.rs | 737 |
| decode_table.rs | 719 |
| reader.rs | 717 |
| inflate_core/fast.rs | 677 |
| lz77.rs | 642 |
| window.rs | 578 |
| parallel.rs | 562 |
| pool.rs | 534 |
| deflate.rs | 513 |
| tables.rs | 345 |
| async_reader.rs | 330 |
| async_deflate.rs | 322 |
| raw_stream.rs | 274 |
| gzip.rs | 268 |
| optimal.rs | 261 |
| lib.rs | 116 |
| **Total** | **15,907** |

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
