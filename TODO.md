
# OxiArc - Development Roadmap (v0.3.5, 2026-07-07)

## Version History

- **v0.3.5** (2026-07-07): LZH canonical bitstream rewrite — the lh4/lh5/lh6/lh7 codec previously implemented a private, self-consistent-only format; rewritten to genuine canonical LHA format (MSB-first bit order + corrected code-table/position encoding, new `oxiarc_core::msb_bitstream` module), validated against 6 real-LHA-produced `.lzh` archives and a live `lha` (Lhasa) oracle at both the codec level (`oxiarc-lzhuf`) and archive level (`oxiarc-archive`) via a new opt-in `lha-oracle` Cargo feature in both crates. Also: fixed genuine undefined behavior (Miri-detected) in oxiarc-core's CRC SIMD code (unsound pointer arithmetic, 7 call sites); fixed a resource leak in `ZipWriter`/`TarWriter`/`LzhWriter::into_inner()` in oxiarc-archive; fixed oxiarc-cli exit codes so `test`/`list` on unrecognized archive formats correctly return non-zero instead of silently exiting 0; dependency updates (`glob`, `clap_complete` bumps, `crossbeam-epoch` security fix for RUSTSEC-2026-0204, removed an unused oxiarc-core dependency from oxiarc-szip); added regression test coverage for deflate max-length-match decoding and snappy max-size-block handling (both already-correct, closing test gaps). 1,878 tests passing (all features), zero clippy/check/rustdoc warnings.
- **v0.3.4** (2026-07-06): Interoperability hardening release — spec-conformance defects found via downstream FVRS integration testing were root-caused and fixed across LZMA/LZMA2 (distance-slot probability layout, state mapping, LZMA2 control-byte reset field, embedded EOS markers), bzip2 (bit order, CRC-32 variant, MTF/Huffman pipeline, format minimums), 7z (substream sizes, 0-byte members, varint decoding, header/CRC handling), ZIP (Shift-JIS/EFS/CP437 name decoding, writer EFS flag), TAR (char-boundary panic, PAX long-name write support), XZ (block-header CRC, index unpadded size, self-describing chunk framing), and LZH (`-lh1-`/`-lhd-`/unknown-method listing, beyond-window LZSS/Huffman fixes, Shift_JIS filenames). All codecs validated bidirectionally against liblzma/libbz2/bsdtar/CPython; hermetic golden-vector test suites committed. 1,799 tests passing (all features), zero clippy/check/rustdoc warnings.
- **v0.3.3** (2026-06-06): oxiarc-brotli high-entropy/incompressible round-trip fix — incompressible data now round-trips byte-for-byte across all quality levels (1–11). Fixed two underlying bugs: (1) incomplete length-limited Huffman codes (replaced the `ceil(-log2 p)` heuristic with the package-merge algorithm, which always yields a complete, length-optimal code) and (2) insert lengths above 319 were silently truncated (unified the insert-length code table between encoder and decoder, extending categories up to ~4 MiB inserts). 13 new high-entropy regression tests. 1,679 tests passing, 2 skipped, zero warnings. No other crate changed.
- **v0.3.2** (2026-05-31): AEC/SZIP codec (oxiarc-szip) — full CCSDS-121.0-B-2 compliant encoder/decoder with `BitReader`/`BitWriter` bit manipulation primitives, `SzipParams` configuration struct, `SzipError` error enum. Round-trip tests for all sample scenarios.
- **v0.3.1** (2026-05-16): LZH custom dictionary (`LzhEncoder::with_dictionary`, `LzhDecoder::with_dictionary`, `LzssEncoder/Decoder::preload_dictionary`). LZMA custom dictionary (`LzmaEncoder/Decoder::with_dictionary`). LZMA memory pool (`LzmaPool`, `PooledBuf`, `LzmaDecoderPooled`) — amortizes large dict allocations. Archive repair/recovery (`repair_zip`, `repair_tar`, `ZipRepair`, `TarRepair`, `RepairReport`) for truncated/corrupt ZIP+TAR archives. Snappy + Brotli interop test vectors (35 new integration tests). 1446 tests (77 new), 3 skipped, zero warnings.
- **v0.3.0** (2026-05-17): DEFLATE Zopfli-style optimal parsing (`OptimalParser`, `with_optimal_parsing`). Snappy parallel frame compression (`compress_parallel`, `parallel` feature). LZ4 true bounded-memory streaming (block-level `Lz4Compressor`/`Lz4Decompressor`, `with_memory_budget`). LZH 4-byte hash + `LzssOptimalParser` + `with_optimal()` builder. LZMA BT4 binary tree match finder (`Bt4MatchFinder`, `MatchFinder` trait; level 9 now uses BT4). `MappedFile` zero-copy memory-mapped primitive in oxiarc-core (`mmap` feature). 1325 tests (44 new), 3 skipped, zero warnings.
- **v0.2.8** (2026-05-08): SIMD CRC32 via aarch64 PMULL (Apple Silicon) and x86_64 SSE 4.2 (Snappy CRC32C). Progress/cancel builders (`with_progress`, `with_cancel`) on lz4, zstd, and lzma2. Raw-preserve append in `oxiarc add` (ZIP/LZH byte-for-byte). ISO 9660 read support (list/extract/info/detect). CLI `--memory-limit` option for extract and list. 1281 tests passing (2 skipped), 1442 public API items, 58,356 lines, 12 crates, 182 Rust files.
- **v0.2.7** (2026-04-21): All workspace crates feature-complete, tested, and API-stable. All policies enforced. 1206 tests passing, 1394 public API items.
- **v0.2.6** (2026-03-21): Brotli fixes: is_single_symbol() bug fix, write_prefix_code_and_build_tree() function, Kraft inequality i32 fix, comprehensive roundtrip tests.
- **v0.2.5** (2026-03-18): New codecs: Brotli (RFC 7932) with quality levels 0-11, static dictionary, streaming; Snappy with block and framed formats, CRC32C. DEFLATE streaming (GzipStreamEncoder/Decoder, ZlibStreamEncoder/Decoder) with flush modes (sync_flush, full_flush, partial_flush). LZ4 acceleration parameter for compress_block_with_accel(). LZW streaming encoder/decoder (LzwStreamEncoder/LzwStreamDecoder, TIFF and GIF modes). Brotli/Snappy archive integration (BrotliReader/BrotliWriter, SnappyReader/SnappyWriter with format detection). EntryBuilder pattern with fluent API. Serde serialization for Entry types (optional feature). CLI: dry-run mode (--dry-run/-n), sort by ratio, Brotli/Snappy format support. Total: 1038 tests, ~47,241 lines, 150 files.
- **v0.2.4** (2026-03-16): Dependency updates (clap 4.5→4.6, clap_complete 4.5→4.6), clippy fixes (collapsible match guards, sort_by→sort_by_key, redundant .max(0)). Total: 799 tests, ~40,406 lines, 127 files.
- **v0.2.3** (2026-03-11): Async ZIP (async_zip), async deflate (async_deflate), GZip module, GIF LZW codec (gif_lzw), LSB bitstream (bitstream_lsb). Total: 799 tests, ~39,417 lines, 127 files.
- **v0.2.2**: Previous release
- **v0.2.1**: Previous release
- **v0.2.0**: LZW crate, full Zstandard encoder, Bzip2 parallel compression
- **v0.1.0**: Initial release — core codecs, archive formats, CLI

## Phase 1: Core Foundation (COMPLETE)

- [x] BitStream (LSB-first bit packing)
  - [x] BitReader with u64 buffer
  - [x] BitWriter with u64 buffer
  - [x] Generic over Read/Write traits
- [x] RingBuffer for LZ77/LZSS
  - [x] Configurable sizes (4K-64K)
  - [x] Safe modulo wrapping
  - [x] Copy-from-self for match expansion
- [x] CRC implementations
  - [x] CRC-32 (ZIP/GZIP polynomial)
  - [x] CRC-32 slicing-by-8 optimization (8x table lookup, ~3-5x faster)
  - [x] CRC-64/ECMA-182 (XZ format)
  - [x] CRC-64 slicing-by-8 optimization (8x table lookup, ~3-5x faster)
  - [x] CRC-16/ARC (LZH polynomial)
- [x] Core traits
  - [x] Compressor/Decompressor streaming traits
  - [x] ArchiveReader/ArchiveWriter traits
  - [x] Entry metadata structure
- [x] Error handling with thiserror

## Phase 2: DEFLATE Codec (COMPLETE)

- [x] Huffman trees
  - [x] Canonical Huffman code generation
  - [x] Tree building from code lengths
  - [x] Fixed Huffman tables (RFC 1951)
  - [x] Dynamic Huffman tree generation
  - [x] Package-merge algorithm for length-limited codes
- [x] LZ77 encoder
  - [x] 32KB sliding window
  - [x] Hash chain pattern matching
  - [x] Lazy matching for better compression
- [x] Inflate (decompression)
  - [x] Stored blocks (type 00)
  - [x] Fixed Huffman blocks (type 01)
  - [x] Dynamic Huffman blocks (type 10)
- [x] Deflate (compression)
  - [x] Fixed Huffman encoding
  - [x] Dynamic Huffman encoding (RLE code length encoding)
  - [x] Automatic block type selection (size estimation)
  - [x] Compression levels 0-9
  - [x] Frequency counting and optimal tree building
- [x] Zlib wrapper (RFC 1950)
  - [x] Adler-32 checksum implementation
  - [x] Zlib header with compression level indicator
  - [x] Streaming compressor/decompressor

## Phase 3: LZH Codec (Complete)

- [x] LZSS encoder/decoder
  - [x] Ring buffer implementation
  - [x] Length/distance coding
- [x] LZH Huffman trees
  - [x] CODES tree (literals + lengths)
  - [x] OFFSETS tree (distances)
- [x] Method support
  - [x] lh0 (stored)
  - [x] lh5 (8KB window, most common)
  - [x] lh4, lh6, lh7 (other window sizes)

## Phase 4: Container Formats (Partial)

### ZIP
- [x] Local file header parsing
- [x] Central directory parsing
- [x] File extraction with DEFLATE
- [x] File extraction with stored method
- [x] Archive creation (ZipWriter)
- [x] Zip64 extensions (large files)
- [x] Data descriptor support (FLAG_DATA_DESCRIPTOR, central directory based reading)
- [x] ZIP encryption (traditional) — ZipCrypto implemented + e2e test (2026-04-20)
- [x] ZIP encryption (AES) — WinZip AE-2 AES-256 implemented + e2e test (2026-04-20)

### GZIP
- [x] Header parsing (RFC 1952)
- [x] Decompression with CRC-32 verification
- [x] Compression (archive creation)
- [x] Optional fields (FNAME, FCOMMENT, etc.)

### TAR
- [x] UStar header parsing
- [x] Entry listing
- [x] File extraction
- [x] Archive creation (TarWriter)
- [x] PAX extended headers (long filenames, metadata)
- [x] GNU LongName/LongLink headers

### LZH
- [x] Level 0/1/2/3 header parsing
- [x] Extension headers (filename, directory, etc.)
- [x] More extension headers (0x40 OS attr, 0x41 Windows timestamps, 0x42/0x43 64-bit sizes, 0x44 comment, 0x46 Unix perms, 0x50 owner names, 0x51 owner IDs, 0x54 Unix mtime)
- [x] Shift_JIS filename decoding
- [x] Path sanitization
- [x] File extraction with CRC-16 verification
- [x] Archive creation (LzhWriter - stored mode)
- [x] LH5 compression encoding (roundtrip working)

## Phase 5: LZMA Codec (Complete)

- [x] Range coder
  - [x] Range encoder with cache mechanism
  - [x] Range decoder
  - [x] 11-bit probability model
- [x] LZMA model
  - [x] Literal model with context
  - [x] Length model (match/rep lengths)
  - [x] Distance model (slot + direct + align)
  - [x] State machine (12 states)
- [x] LZMA encoder
  - [x] Literal encoding
  - [x] Match encoding
  - [x] Rep match encoding
  - [x] End marker encoding
  - [x] Optimal parsing with price calculation and dynamic programming
  - [x] Compression levels 0-9 (greedy for levels 0-6, optimal for levels 7-9)
- [x] LZMA decoder
  - [x] Full LZMA stream decoding
  - [x] Known/unknown uncompressed size

## Phase 6: Advanced Features (Future)

### Additional Codecs
- [x] LZMA2 (for 7z/xz)
- [x] BZip2 (BWT + MTF + Huffman + Zero-run encoding, full roundtrip support)
- [x] LZ4 (official frame format with XXHash32 checksums)
  - [x] Official LZ4 frame format (RFC compatible)
  - [x] XXHash32 implementation for frame/block/content checksums
  - [x] Block independence flag and configurable block sizes (64KB-4MB)
  - [x] Content size in header and content checksum verification
  - [x] LZ4-HC high compression mode (levels 1-12)
  - [x] Optimal parsing for level 12 (dynamic programming)
- [x] Zstandard (FSE + Huffman + XXHash64, full encoder with bitwriter, compressed_block, fse_encoder, huffman_encoder, lz77, streaming, dict modules)
- [x] GIF LZW codec (gif_lzw module in oxiarc-lzw, LSB bitstream)
- [x] LSB bitstream (bitstream_lsb module in oxiarc-lzw)
- [x] Brotli (RFC 7932 with LZ77, context-dependent Huffman coding, static dictionary, quality levels 0-11, streaming API)
- [x] Snappy (block format + framed format with CRC32C checksums, streaming Write/Read API)

### Additional Formats
- [x] 7z archive format (read support with LZMA/LZMA2 decompression)
- [x] XZ file format (compression and decompression)
- [x] CAB (Microsoft Cabinet, read support with None/MSZIP decompression)
- [ ] RAR (read-only, legal constraints)

### Performance
- [x] CRC-32 slicing-by-8 optimization (8x table lookup, ~3-5x faster)
- [x] CRC-64 slicing-by-8 optimization (8x table lookup, ~3-5x faster)
- [x] LZ77 hash function optimization (improved avalanche properties, multiplication-based)
- [x] LZ77 match finding optimization (early rejection, loop unrolling, best_len check)
- [x] LZ77 large input handling (proper chunking and window sliding)
- [x] BWT key-based sorting optimization (4-byte prefix keys for faster sorting)
- [x] Comprehensive performance benchmarks
  - [x] CRC benchmarks (crc_bench)
  - [x] LZ77 benchmarks (lz77_bench, deflate_bench)
  - [x] BWT benchmarks (bwt_bench, bzip2_bench)
  - [x] LZ4 benchmarks (lz4_bench)
  - [x] LZH benchmarks (lzhuf_bench)
  - [x] LZMA benchmarks (lzma_bench)
  - [x] Zstandard benchmarks (zstd_bench, parallel_bench)
  - [x] LZW benchmarks (lzw_bench)
  - [x] Brotli benchmarks (brotli_bench)
  - [x] Snappy benchmarks (snappy_bench)
  - Performance numbers:
    - LZ77: 48-400 MB/s (level 1), 13-275 MB/s (level 5), 0.3-253 MB/s (level 9)
    - BWT Forward: 2-11 MB/s, Inverse: 60-320 MB/s
- [x] Parallel compression (partially complete)
  - [x] LZ4 parallel frame compression (rayon-based block-level parallelism)
  - [x] Zstandard parallel compression (rayon-based block-level parallelism)
  - [x] Bzip2 parallel compression (rayon-based block-level parallelism)
  - [x] Parallel GZIP — pigz-style multi-member parallel GZIP (`gzip_compress_parallel`, `ParallelGzipEncoder`, `parallel` feature in oxiarc-deflate)
  - [x] Parallel LZMA2 — multi-threaded LZMA2 compression (`lzma2_compress_parallel`, `ParallelLzma2Encoder`, `parallel` feature in oxiarc-lzma)
- [x] LZ4 block-layer prefix dictionary support (`Lz4DictBlockEncoder`, `Lz4DictBlockDecoder`, `compress_block_with_dict`, `decompress_block_dict`)
- [x] LZ77 heuristics tuning API (`Lz77Params`, `Lz77Preset` — nice_match + chain configuration in oxiarc-deflate)
- [x] DEFLATE thread-safe memory pool (`DeflatePool`, `PooledBuf` — amortizes buffer allocations in oxiarc-deflate)
- [x] DEFLATE streaming compression/decompression (GzipStreamEncoder/Decoder, ZlibStreamEncoder/Decoder with configurable block sizes)
- [x] LZ4 acceleration parameter (compress_block_with_accel, adaptive skip scaling)
- [x] Memory-mapped file support (`MappedFile` in oxiarc-core, `mmap` feature, done 2026-05-16)
- [x] LZMA custom dictionary support (`LzmaEncoder::with_dictionary`, `LzmaDecoder::with_dictionary`)
- [x] LZMA memory pool (`LzmaPool`, `PooledBuf`, `LzmaDecoderPooled` — amortizes large dict allocations, `parallel` feature in oxiarc-lzma)
- [x] LZH custom dictionary support (`LzhEncoder::with_dictionary`, `LzhDecoder::with_dictionary`, `LzssEncoder/Decoder::preload_dictionary`)
- [x] Archive repair/recovery (`repair_zip`, `repair_tar`, `ZipRepair`, `TarRepair`, `RepairReport` — handles truncated/corrupt ZIP+TAR archives in oxiarc-archive)
- [x] Snappy memory pool (`SnappyPool`, `PoolStats`, `compress_frame_pooled` — thread-safe buffer reuse for FrameEncoder/FrameDecoder in oxiarc-snappy)
- [x] Snappy dictionary APIs (`compress_block_with_dict`, `decompress_block_with_dict`, `compress_frame_with_dict`, `decompress_frame_with_dict` in oxiarc-snappy)
- [x] Snappy async I/O (`AsyncSnappyCompressor`, `AsyncSnappyDecompressor` — async-io feature in oxiarc-snappy)
- [x] Zstd multi-frame decompression (`decompress_multi_frame`, `decompress_multi_frame_with_dict`; streaming dict multi-frame fix in oxiarc-zstd)
- [x] CLI man pages — full set of troff `.1` man pages for all CLI subcommands in `man/` directory
- [x] Async deflate (async_deflate module in oxiarc-deflate, async-io feature)
- [x] GZip module (gzip module in oxiarc-deflate)
- [x] Async ZIP support (async_zip module in oxiarc-archive, async-io feature)
- [x] DEFLATE flush modes (sync_flush, full_flush, partial_flush for GzipStreamEncoder/ZlibStreamEncoder)
- [x] LZW streaming encoder/decoder (LzwStreamEncoder/LzwStreamDecoder with TIFF and GIF modes)
- [x] EntryBuilder pattern with fluent API (oxiarc-core)
- [x] Serde serialization for Entry types (optional serde feature in oxiarc-core)
- [x] Streaming with async I/O (completed 2026-07-07) — all per-crate async I/O done: core traits/wrappers (AsyncCompressor/AsyncDecompressor, StreamingAsyncCompressor/Decompressor, compress_concurrent/decompress_concurrent), brotli (BrotliAsyncCompressor/Decompressor), lzma (async_lzma), archive TAR/LZH/ZIP async readers, snappy async I/O, async DEFLATE (GzipStream/ZlibStream). True bounded-memory async streaming through codec internals is an explicit non-goal (each sub-crate's own TODO documents this); open a new item if that scope is wanted later.

### Quality / Testing
- [x] Snappy interop integration tests (16 tests against wire-format golden vectors covering block and framed formats in oxiarc-snappy)
- [x] Brotli interop integration tests (19 tests across quality levels 0-11 covering RFC 7932 compliance in oxiarc-brotli)
- [x] Brotli high-entropy/incompressible round-trip fix (v0.3.3) — package-merge length-limited Huffman codes + unified encoder/decoder insert-length table (inserts up to ~4 MiB); incompressible data round-trips byte-for-byte across quality 1–11; 13 new high-entropy regression tests in oxiarc-brotli

### Platform
- [ ] WASM bindings
- [ ] Python bindings (PyO3)

## Phase 7: CLI Enhancements & Documentation (Complete)

- [x] List command
  - [x] JSON output support (--json flag with pretty-printing)
- [x] Extract command (ZIP, GZIP, TAR, LZH, XZ, 7z, CAB, LZ4, Zstd, Bzip2, Brotli, Snappy)
  - [x] Timestamp preservation (--preserve-timestamps, -t)
  - [x] Permission preservation (--preserve-permissions)
  - [x] All metadata preservation (-p for timestamps + permissions)
  - [x] Overwrite modes (--overwrite, --skip-existing, --prompt)
  - [x] Stdin/stdout support for single-file formats
- [x] Info command
- [x] Detect command
- [x] Test command (archive integrity)
- [x] Create command (ZIP, TAR, GZIP, LZH, XZ, LZ4, Zstd, Bzip2, Brotli, Snappy)
  - [x] Stdin/stdout support for single-file formats
- [x] Convert command (format conversion including 7z, CAB, Brotli, Snappy input)
- [x] Dry-run mode (--dry-run, -n for create and extract commands)
- [x] Sort by ratio (Ratio variant added to SortBy enum)
- [x] Progress bars and verbose output (extract command with -v/--verbose and -P/--progress)
- [x] Recursive directory handling
- [x] Filter patterns (include/exclude)
- [x] Shell completion scripts (bash, zsh, fish, powershell)
- [x] Comprehensive README documentation
  - [x] Installation instructions (cargo install, build from source)
  - [x] Format support matrix with full feature breakdown
  - [x] Performance benchmarks with real data
  - [x] Detailed CLI examples for all commands
  - [x] API usage examples for all major codecs
  - [x] Contributing guidelines following COOLJAPAN policies

## Known Issues

1. Bzip2 parallel compression bit-alignment issues have been RESOLVED
2. LZMA/LZMA2 liblzma interop issues have been RESOLVED (v0.3.4: distance-slot probability layout, state mapping, LZMA2 control-byte reset field, embedded LZMA2 EOS marker)
3. BWT has O(n² log n) worst-case for highly repetitive data (mitigated by 900KB block size limit)

## Test Coverage

- oxiarc-core: 132 tests
  - CRC-32/64 slicing-by-8, DualCrc optimization, SIMD CRC32 (aarch64 PMULL), size boundary tests, bitstream, ringbuffer, EntryBuilder, Serde serialization
- oxiarc-deflate: 217 tests
  - Dynamic Huffman, Zlib wrapper, Adler-32, edge cases, compression levels, async deflate, gzip module, streaming (GzipStreamEncoder/Decoder, ZlibStreamEncoder/Decoder, flush modes), optimal parsing, streaming improvements, parallel GZIP (gzip_compress_parallel, ParallelGzipEncoder), LZ77 tuning (Lz77Params, Lz77Preset), DEFLATE memory pool (DeflatePool, PooledBuf), streaming compliance fixes
- oxiarc-lzhuf: 122 tests
  - LH1/LH5 roundtrip encoding/decoding (incl. beyond-window regression tests), LZSS, Huffman trees, optimal parser, streaming integration, custom dictionary (LzhEncoder::with_dictionary, LzhDecoder::with_dictionary, LzssEncoder/Decoder::preload_dictionary), extension header DOS attributes
- oxiarc-bzip2: 68 tests
  - MSB-first bit I/O, non-reflected CRC-32, BWT, MTF/symbol-map Huffman, roundtrip, libbz2 interop, parallel compression
- oxiarc-lz4: 138 tests
  - Official frame format, XXHash32, LZ4-HC, block/frame compression, parallel compression, acceleration parameter tests, progress/cancel builders, bounded-memory streaming, memory_budget, block-layer prefix dictionary (Lz4DictBlockEncoder, Lz4DictBlockDecoder, compress_block_with_dict, decompress_block_dict)
- oxiarc-zstd: 179 tests
  - FSE, Huffman, XXHash64, frame parsing, full encoder (bitwriter, compressed_block, fse_encoder, huffman_encoder, lz77, streaming, dict), parallel compression, progress/cancel builders, multi-frame decompression (decompress_multi_frame, decompress_multi_frame_with_dict), streaming dict multi-frame fix
- oxiarc-archive: 380 tests
  - ZIP/TAR/LZH/XZ/7z/CAB/LZ4/Zstd/Bzip2 support, PAX headers (incl. write-side PAX long names), Zip64 and data descriptors, async ZIP, Brotli/Snappy integration, ISO 9660 read support, raw-preserve append, archive repair/recovery (repair_zip, repair_tar, ZipRepair, TarRepair, RepairReport), liblzma/libbz2/bsdtar/CPython interop suites (7z, ZIP name encoding, TAR PAX Japanese names)
- oxiarc-lzma: 151 tests
  - LZMA/LZMA2 (spec-conformant distance-slot table, state mapping, LZMA2 control-byte, chunked encoder), optimal parsing, range coder, price calculation, progress/cancel builders, BT4 match finder, Bt4MatchFinder, MatchFinder trait, parallel LZMA2 (lzma2_compress_parallel, ParallelLzma2Encoder, parallel feature), custom dictionary (LzmaEncoder::with_dictionary, LzmaDecoder::with_dictionary), LZMA memory pool (LzmaPool, PooledBuf, LzmaDecoderPooled), liblzma golden-vector interop
- oxiarc-lzw: 76 tests
  - GIF/TIFF configurations, GIF LZW codec (gif_lzw), LSB bitstream (bitstream_lsb), dictionary management, roundtrip tests, streaming encoder/decoder tests
- oxiarc-brotli: 163 tests
  - Brotli RFC 7932, LZ77, context-dependent Huffman coding, static dictionary, quality levels 0-11, streaming API, interop integration tests, encoder bug fixes, high-entropy/incompressible round-trip regression tests (package-merge Huffman + unified insert-length table)
- oxiarc-snappy: 112 tests
  - Snappy block format, framed format with CRC32C checksums (SSE 4.2 hardware acceleration on x86_64), streaming Write/Read API, interop integration tests, parallel frame compression, memory pool (SnappyPool, PoolStats, compress_frame_pooled), dictionary APIs (compress_block_with_dict, compress_frame_with_dict, decompress_block_with_dict, decompress_frame_with_dict), async I/O (AsyncSnappyCompressor, AsyncSnappyDecompressor)
- oxiarc-szip: 19 tests
  - AEC/SZIP CCSDS-121.0-B-2 encoder/decoder, BitReader/BitWriter bit primitives, SzipParams configuration, SzipError error type, round-trip encode/decode tests
- oxiarc-cli: 42 tests
- Total: 1,799 tests (1,799 passed, 0 skipped, zero warnings, all features)

## Code Statistics (v0.3.5, 2026-07-07)

| Crate | Lines of Code |
|-------|---------------|
| oxiarc-core | ~4,969 (CRC-32/64 slicing-by-8, optimized DualCrc, EntryBuilder, Serde) |
| oxiarc-deflate | ~7,835 (Zlib wrapper, Adler-32, async_deflate, gzip, streaming modules) |
| oxiarc-lzhuf | ~5,889 (lh1/lh5 beyond-window fixes) |
| oxiarc-bzip2 | ~2,500 (MSB-first bit I/O, libbz2-compatible CRC/Huffman) |
| oxiarc-lz4 | ~5,484 (official frame, XXHash32, LZ4-HC, acceleration; refactored frame/ module) |
| oxiarc-zstd | ~6,566 (full encoder: bitwriter, compressed_block, fse_encoder, huffman_encoder, lz77, streaming, dict) |
| oxiarc-brotli | ~5,677 (LZ77, context Huffman, static dictionary, streaming) |
| oxiarc-snappy | ~3,451 (block format, framed format, CRC32C) |
| oxiarc-szip | ~776 (BitReader/BitWriter, encode, decode, encode_bytes, SzipParams, SzipError, CCSDS-121.0-B-2) |
| oxiarc-archive | ~18,067 (7z/ZIP/TAR/XZ/LZH interop hardening, name_codec, source_map) |
| oxiarc-cli | ~4,988 |
| oxiarc-lzma | ~7,222 (spec-conformant distance-slot table, state mapping) |
| oxiarc-lzw | ~2,140 (gif_lzw module, bitstream_lsb module, streaming encoder/decoder) |
| **Total** | **~76,755** (256 files) |

## Stubs to implement (added 2026-06-22 by /cooljapan-stub-check)

- [x] LZH compression (lh5) encoder now spec-compatible (completed 2026-07-07) — MSB-first canonical rewrite of the lh4/lh5/lh6/lh7 codec (new `oxiarc-core::msb_bitstream`; `oxiarc-lzhuf/src/{encode,huffman,decode,lzss,optimal,methods}.rs` + `streaming/{decoder,huffman}.rs`) producing genuine LHA-wire-format archives; `oxiarc-archive/src/lzh/{writer,header}.rs` needed **no** changes — the default level-2 writer already emits headers real `lha` reads. Validated byte-exact against 6 genuine real-LHA `.lzh` fixtures (levels 0/1/2, incl. a 1.24 MB multi-block file, cross-tool) AND a live `lha` (Lhasa 0.6.0) oracle at **both** the codec level (`oxiarc-lzhuf` `lha-oracle`: `lha t`/`x` round-trips) and the **archive** level (new `oxiarc-archive` `lha-oracle`: `lha l`/`t`/`x`/`pq` over small, ~150 KB multi-block, multi-file, Shift_JIS-named, empty and stored archives); reverse direction (real archive → `LzhReader`) added as always-run `tests/lzh_corpus_reader.rs`. Suites green (oxiarc-archive 389, oxiarc-lzhuf 163), clippy clean, oracle self-skips without `lha`. Residual (out of scope, non-blocking, pre-existing): the opt-in level-1/0 writer stores Unix time where canonical LHA uses DOS FAT time (cosmetic date only — `lha` still CRC-tests + extracts fine), and Lhasa does not read level-3 headers at all; the default level-2 path is fully clean. Prior fix (v0.3.4, 2026-07-06): an earlier, already-shipped beyond-window corruption bug (LZSS clobbering history past the window, a 16-bit per-block size field overflowing past 65535 bytes, wrong p-tree count-field width for np=16/17) was fixed under this same item with 8/16/64/100 KB CRC-16 regression tests; that fix stands independently — this canonical-format rewrite was the separate, now-closed real-tool-readability work.
