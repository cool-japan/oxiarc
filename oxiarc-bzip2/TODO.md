# oxiarc-bzip2 - Development Status (v0.4.2, 2026-08-06)

## Completed Features (COMPLETE)

### BZip2 Core
- [x] Pure Rust implementation
- [x] Parallel compression (Rayon)
- [x] Compression levels 1-9 (block sizes)
- [x] Streaming and one-shot APIs
- [x] Property-based round-trip testing (proptest) and `examples/roundtrip.rs` (new in 0.3.6)

### Production hardening (2026-07-13)
- [x] Multi-stream (concatenated) `.bz2` decode — pbzip2/lbzip2/`cat` files decode in full; trailing garbage errors instead of silent truncation (BZIP2-01)
- [x] Multi-table Huffman encoder — libbz2 `sendMTFValues` clustering (2-6 tables, 4 refinement passes, real per-group selectors); structured-data output now ~99.9% of `bzip2 -9` (was ~150%) (BZIP2-02)
- [x] Legacy randomised-block decode (bzip2 <= 0.9.0, `BZ2_rNums` schedule) (BZIP2-03)
- [x] `decompress_with_limit` decompression-bomb guard + documented memory characteristics (BZIP2-04)
- [x] Fallible MTF/RLE utility helpers (no panic/overflow trap doors) (BZIP2-05)
- [x] `BzEncoder::write_block` buffers input and emits only full-size blocks; remainder flushed in `finish()` (BZIP2-06)
- [x] Differential oracle suite vs the system `bzip2` CLI, both directions (`bzip2-oracle` feature, self-skipping); always-on multi-stream / randomised fixtures
- [x] All features tested (108 tests passing with `--all-features`)

## Milestone: COMPLETE

All features implemented and tested. API is stable.
