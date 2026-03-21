# OxiArc - Development Roadmap

## Version History

- **v0.2.6** (2026-03-21): Brotli fixes: is_single_symbol() bug fix, write_prefix_code_and_build_tree() function, Kraft inequality i32 fix, comprehensive roundtrip tests.
- **v0.2.5** (2026-03-18): New codecs: Brotli (RFC 7932) with quality levels 0-11, static dictionary, streaming; Snappy with block and framed formats, CRC32C. DEFLATE streaming (GzipStreamEncoder/Decoder, ZlibStreamEncoder/Decoder) with flush modes (sync_flush, full_flush, partial_flush). LZ4 acceleration parameter for compress_block_with_accel(). LZW streaming encoder/decoder (LzwStreamEncoder/LzwStreamDecoder, TIFF and GIF modes). Brotli/Snappy archive integration (BrotliReader/BrotliWriter, SnappyReader/SnappyWriter with format detection). EntryBuilder pattern with fluent API. Serde serialization for Entry types (optional feature). CLI: dry-run mode (--dry-run/-n), sort by ratio, Brotli/Snappy format support. Total: 1038 tests, ~47,241 lines, 150 files.
- **v0.2.4** (2026-03-16): Dependency updates (clap 4.5→4.6, clap_complete 4.5→4.6), clippy fixes (collapsible match guards, sort_by→sort_by_key, redundant .max(0)). Total: 799 tests, ~40,406 lines, 127 files.
- **v0.2.3** (2026-03-11): Async ZIP (async_zip), async deflate (async_deflate), GZip module, GIF LZW codec (gif_lzw), LSB bitstream (bitstream_lsb). Total: 799 tests, ~39,417 lines, 127 files.
- **v0.2.2**: Previous release
- **v0.2.1**: Previous release
- **v0.2.0**: LZW crate, full Zstandard encoder, Bzip2 parallel compression
- **v0.1.0**: Initial release — core codecs, archive formats, CLI

## Phase 1: Core Foundation (Complete)

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

## Phase 2: DEFLATE Codec (Complete with Dynamic Huffman & Zlib)

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
- [ ] ZIP encryption (traditional, AES)

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
- [x] DEFLATE streaming compression/decompression (GzipStreamEncoder/Decoder, ZlibStreamEncoder/Decoder with configurable block sizes)
- [x] LZ4 acceleration parameter (compress_block_with_accel, adaptive skip scaling)
- [ ] Memory-mapped file support
- [x] Async deflate (async_deflate module in oxiarc-deflate, async-io feature)
- [x] GZip module (gzip module in oxiarc-deflate)
- [x] Async ZIP support (async_zip module in oxiarc-archive, async-io feature)
- [x] DEFLATE flush modes (sync_flush, full_flush, partial_flush for GzipStreamEncoder/ZlibStreamEncoder)
- [x] LZW streaming encoder/decoder (LzwStreamEncoder/LzwStreamDecoder with TIFF and GIF modes)
- [x] EntryBuilder pattern with fluent API (oxiarc-core)
- [x] Serde serialization for Entry types (optional serde feature in oxiarc-core)
- [~] Streaming with async I/O (partial: DEFLATE streaming GzipStream/ZlibStream; full streaming pipeline pending)

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
2. LZMA encoder has known issues with certain complex data patterns (simple data works)
3. BWT has O(n² log n) worst-case for highly repetitive data (mitigated by 900KB block size limit)

## Test Coverage

- oxiarc-core: 111 tests
  - CRC-32/64 slicing-by-8, DualCrc optimization, size boundary tests, bitstream, ringbuffer, EntryBuilder, Serde serialization
- oxiarc-deflate: 120 tests
  - Dynamic Huffman, Zlib wrapper, Adler-32, edge cases, compression levels, async deflate, gzip module, streaming (GzipStreamEncoder/Decoder, ZlibStreamEncoder/Decoder, flush modes)
- oxiarc-lzhuf: 54 tests (34 lib + 20 streaming_integration)
  - LH5 roundtrip encoding/decoding, LZSS, Huffman trees
- oxiarc-bzip2: 37 tests (2 skipped)
  - BWT, MTF, RLE, Huffman, roundtrip, parallel compression
- oxiarc-lz4: 110 tests
  - Official frame format, XXHash32, LZ4-HC, block/frame compression, parallel compression, acceleration parameter tests
- oxiarc-zstd: 170 tests
  - FSE, Huffman, XXHash64, frame parsing, full encoder (bitwriter, compressed_block, fse_encoder, huffman_encoder, lz77, streaming, dict), parallel compression
- oxiarc-archive: 165 tests
  - ZIP/TAR/LZH/XZ/7z/CAB/LZ4/Zstd/Bzip2 support, PAX headers, Zip64 and data descriptors, async ZIP, Brotli/Snappy integration
- oxiarc-lzma: 66 tests
  - LZMA/LZMA2, optimal parsing, range coder, price calculation
- oxiarc-lzw: 76 tests
  - GIF/TIFF configurations, GIF LZW codec (gif_lzw), LSB bitstream (bitstream_lsb), dictionary management, roundtrip tests, streaming encoder/decoder tests
- oxiarc-brotli: 78 tests
  - Brotli RFC 7932, LZ77, context-dependent Huffman coding, static dictionary, quality levels 0-11, streaming API
- oxiarc-snappy: 54 tests
  - Snappy block format, framed format with CRC32C checksums, streaming Write/Read API
- oxiarc-cli: 0 tests
- Total: 1041 tests (1041 passed, 2 skipped, zero warnings)

## Code Statistics (v0.2.6, 2026-03-18)

| Crate | Lines of Code |
|-------|---------------|
| oxiarc-core | ~3,565 (CRC-32/64 slicing-by-8, optimized DualCrc, EntryBuilder, Serde) |
| oxiarc-deflate | ~3,479 (Zlib wrapper, Adler-32, async_deflate, gzip, streaming modules) |
| oxiarc-lzhuf | ~2,746 |
| oxiarc-bzip2 | ~1,548 |
| oxiarc-lz4 | ~3,656 (official frame, XXHash32, LZ4-HC, acceleration; refactored frame/ module) |
| oxiarc-zstd | ~5,741 (full encoder: bitwriter, compressed_block, fse_encoder, huffman_encoder, lz77, streaming, dict) |
| oxiarc-brotli | ~3,460 (LZ77, context Huffman, static dictionary, streaming) |
| oxiarc-snappy | ~1,428 (block format, framed format, CRC32C) |
| oxiarc-archive | ~7,897 (ZIP header refactored; async_zip module; Brotli/Snappy integration) |
| oxiarc-cli | ~2,443 |
| oxiarc-lzma | ~3,868 |
| oxiarc-lzw | ~1,092 (gif_lzw module, bitstream_lsb module, streaming encoder/decoder) |
| **Total** | **~47,303** (150 files) |
