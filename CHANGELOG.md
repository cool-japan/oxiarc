# Changelog

All notable changes to the OxiArc project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.2.8] - 2026-05-08

### Added
- **oxiarc-core**: SIMD CRC32 via aarch64 PMULL — hardware-accelerated CRC32 computation on Apple Silicon / aarch64 using PMULL instructions; constants pinned from crc32fast reference; bitwise-identical to scalar path
- **oxiarc-lz4**: `with_progress(Arc<dyn ProgressSink>)` and `with_cancel(CancellationToken)` builders on `Lz4Compressor`, `Lz4Decompressor`, `Lz4DictFrameEncoder`, and `Lz4DictFrameDecoder`
- **oxiarc-zstd**: `with_progress(Arc<dyn ProgressSink>)` and `with_cancel(CancellationToken)` builders on `ZstdEncoder`, `ZstdStreamEncoder`, and `ZstdStreamDecoder`
- **oxiarc-lzma**: `with_progress(Arc<dyn ProgressSink>)` and `with_cancel(CancellationToken)` builders on `Lzma2Encoder`, `Lzma2Decoder`, and `Lzma2ChunkedEncoder`
- **oxiarc-archive**: Raw-preserve append in `oxiarc add` — ZIP and LZH entries are now preserved byte-for-byte when appending new entries, eliminating the decompress→recompress round-trip; added `ZipWriter::add_file_raw`, `LzhReader::read_raw_method_data`, and `LzhWriter::add_file_raw`
- **oxiarc-archive**: ISO 9660 read support via new `IsoReader` with PVD + Joliet UCS-2 filename support; format detection via magic bytes at LBA 16
- **oxiarc-cli**: `list`, `extract`, `info`, and `detect` commands now support `.iso` images
- **oxiarc-snappy**: Snappy CRC32C SSE 4.2 — hardware-accelerated CRC32C for x86_64 using SSE 4.2 intrinsics (`_mm_crc32_u64`) with runtime dispatch via `OnceLock`
- **oxiarc-cli**: `--memory-limit <BYTES>` option for `extract` and `list` subcommands (accepts suffixes such as `100M`, `1G`) to cap peak allocation per entry

### Quality
- Zero clippy warnings (strict mode with `-D warnings`)
- Zero rustdoc warnings
- All policies compliant (no unwrap in production code, pure Rust, latest crates, workspace)

### Crates in This Release
All crates published at version 0.2.8:
- oxiarc-core, oxiarc-deflate, oxiarc-lzhuf, oxiarc-lzw, oxiarc-lzma
- oxiarc-bzip2, oxiarc-lz4, oxiarc-zstd, oxiarc-brotli, oxiarc-snappy
- oxiarc-archive, oxiarc-cli

## [0.2.7] - 2026-04-21

### Added
- **oxiarc-cli**: `oxiarc add` command for appending files to existing archives (ZIP, TAR, LZH formats); supports `--dry-run` and `--verbose` options
- **oxiarc-archive**: `lenient` mode enhancements — robust handling of malformed/partial archives in list and extract operations
- **oxiarc-archive**: `LzhExtensions` module for extended LZH archive manipulation (appending, rewriting entries)
- **oxiarc-archive**: Async LZH and TAR streaming support (`async_lzh.rs`, `async_tar.rs`)
- **oxiarc-core**: `CancellationToken` cooperative cancellation for archive operations
- **oxiarc-core**: `ProgressHandle` / `ProgressSink` progress reporting infrastructure
- **oxiarc-cli**: Man page generation (`man` subcommand via `clap_mangen`)
- **oxiarc-cli**: Colored output with ANSI support (`style.rs`, respects `NO_COLOR`/`--no-color`)
- **oxiarc-cli**: Windows long path support and reserved filename handling during extraction

### Testing
- End-to-end tests for ZIP AES-256 and ZipCrypto encryption (`zip_encryption_e2e.rs`)
- Progress callbacks and cancellation tests for Brotli (`progress_cancel.rs`)
- CLI integration tests: add command, color output, man page generation, password extraction, lenient mode, compression threshold, tree listing, Windows filenames

### Quality
- Zero clippy warnings (strict mode with `-D warnings`)
- Zero rustdoc warnings
- All policies compliant (no unwrap in production code, pure Rust, latest crates, workspace)
- Updated: `clap` 4.6.1, `tokio` 1.52.1

### Crates in This Release
All crates published at version 0.2.7:
- oxiarc-core, oxiarc-deflate, oxiarc-lzhuf, oxiarc-lzw, oxiarc-lzma
- oxiarc-bzip2, oxiarc-lz4, oxiarc-zstd, oxiarc-brotli, oxiarc-snappy
- oxiarc-archive, oxiarc-cli

## [0.2.6] - 2026-03-21

### Added
- **oxiarc-brotli**: `write_prefix_code_and_build_tree()` — unified prefix code writing and Huffman tree construction for encoder use
- **oxiarc-brotli**: Comprehensive roundtrip tests for compress/decompress (simple, binary pattern, uniform data)

### Fixed
- **oxiarc-brotli**: `is_single_symbol()` now correctly identifies true single-symbol Huffman trees (all code lengths must be 0); previously returned true for trees with exactly one non-zero code length, causing incorrect decoding
- **oxiarc-brotli**: Kraft inequality tracker changed to `i32` to prevent potential overflow in complex prefix code reading

### Quality
- Zero clippy warnings (strict mode with `-D warnings`)
- Zero rustdoc warnings (strict mode with all lint checks)
- All policies compliant (no unwrap in production code, pure Rust, latest crates, workspace)

### Crates in This Release
All crates published at version 0.2.6:
- oxiarc-core, oxiarc-deflate, oxiarc-lzhuf, oxiarc-lzw, oxiarc-lzma
- oxiarc-bzip2, oxiarc-lz4, oxiarc-zstd, oxiarc-brotli, oxiarc-snappy
- oxiarc-archive, oxiarc-cli

## [0.2.5] - 2026-03-18

### Added
- **oxiarc-brotli**: New crate — Brotli compression (RFC 7932)
  - Quality levels 0-11 with static dictionary support
  - LZ77 and context-dependent Huffman coding
  - Streaming compression/decompression API
- **oxiarc-snappy**: New crate — Snappy compression
  - Block format and framed format with CRC32C checksums
  - Streaming Write/Read API
- **oxiarc-deflate**: Streaming compression/decompression
  - `GzipStreamEncoder`/`GzipStreamDecoder` with flush modes (sync_flush, full_flush, partial_flush)
  - `ZlibStreamEncoder`/`ZlibStreamDecoder` with configurable block sizes
- **oxiarc-lz4**: Acceleration parameter (`compress_block_with_accel`) with adaptive skip scaling
- **oxiarc-lzw**: Streaming encoder/decoder (`LzwStreamEncoder`/`LzwStreamDecoder`, TIFF and GIF modes)
- **oxiarc-core**: `EntryBuilder` pattern with fluent API; Serde serialization for Entry types (optional `serde` feature)
- **oxiarc-archive**: Brotli/Snappy archive integration (`BrotliReader`/`BrotliWriter`, `SnappyReader`/`SnappyWriter` with format detection)
- **oxiarc-cli**: Dry-run mode (`--dry-run`/`-n`), sort by ratio, Brotli/Snappy format support

### Quality
- Zero clippy warnings (strict mode with `-D warnings`)
- Zero rustdoc warnings (strict mode with all lint checks)
- 100% test pass rate (1038 tests)
- All policies compliant (no unwrap in production code, pure Rust, latest crates, workspace)

### Crates in This Release
All crates published at version 0.2.5:
- oxiarc-core, oxiarc-deflate, oxiarc-lzhuf, oxiarc-lzw, oxiarc-lzma
- oxiarc-bzip2, oxiarc-lz4, oxiarc-zstd, oxiarc-brotli, oxiarc-snappy
- oxiarc-archive, oxiarc-cli

## [0.2.4] - 2026-03-16

### Changed
- Updated dependencies: `clap` 4.5→4.6, `clap_complete` 4.5→4.6
- Clippy fixes: collapsible match guards, `sort_by` → `sort_by_key`, removed redundant `.max(0)`

### Quality
- Zero clippy warnings (strict mode with `-D warnings`)
- Zero rustdoc warnings (strict mode with all lint checks)
- 100% test pass rate (799 tests)
- All policies compliant (no unwrap in production code, pure Rust, latest crates, workspace)

### Crates in This Release
All crates published at version 0.2.4:
- oxiarc-core, oxiarc-deflate, oxiarc-lzhuf, oxiarc-lzw, oxiarc-lzma
- oxiarc-bzip2, oxiarc-lz4, oxiarc-zstd, oxiarc-archive, oxiarc-cli

## [0.2.3] - 2026-03-11

### Added
- `oxiarc-archive`: Async ZIP support (`async_zip` module)
- `oxiarc-deflate`: Async deflate support (`async_deflate` module) and GZip module (`gzip`)
- `oxiarc-lzw`: New GIF LZW codec (`gif_lzw` module) and LSB bitstream support (`bitstream_lsb` module)

### Changed
- `oxiarc-deflate`: Various improvements to LZ77 match-finding, deflate engine, and lib interface
- `oxiarc-lz4`: Dictionary and HC (high-compression) improvements
- `oxiarc-lzma`: Encoder optimizations, model refinements, and optimal parsing improvements
- `oxiarc-zstd`: Frame, streaming, and lib improvements
- `oxiarc-lzhuf`: LZSS improvements
- `oxiarc-bzip2`: BWT improvements
- `oxiarc-archive`: ZIP header reader and module-level improvements

### Quality
- Zero clippy warnings (strict mode with `-D warnings`)
- Zero rustdoc warnings
- 100% test pass rate
- All policies compliant (no unwrap in production code, pure Rust, latest crates, workspace)

### Crates in This Release
All crates published at version 0.2.3:
- oxiarc-core, oxiarc-deflate, oxiarc-lzhuf, oxiarc-lzw, oxiarc-lzma
- oxiarc-bzip2, oxiarc-lz4, oxiarc-zstd, oxiarc-archive, oxiarc-cli

## [0.2.2] - 2026-03-10

### Added

#### oxiarc-zstd: Full Zstandard Encoder Implementation

- **`bitwriter` module** — two bitstream writers required by the Zstandard encoding pipeline:
  - `ForwardBitWriter`: LSB-first bit packing; `write_bits(value: u32, num_bits: u8)` (up to 25 bits), `write_bit(bool)`, `finish() -> Vec<u8>`, `bit_position()`, `byte_len()`, `is_empty()`, `as_bytes()`, `with_capacity()`; used for FSE table description headers
  - `BackwardBitWriter`: sentinel-marked reversed bitstream compatible with `FseBitReader`; `write_bits(value: u64, num_bits: u8)`, `finish() -> Vec<u8>` (empty input yields `[0x01]` sentinel), `len()`, `is_empty()`, `with_capacity()`; used for FSE sequence encoding

- **`lz77` module** — LZ77 match-finder for compressed block production:
  - `LevelConfig`: 22 compression levels mapping level index to `hash_log` (17–20 bits), `chain_log` (0–20 bits), `search_depth` (1 to level×32), `lazy_matching` flag, `lazy_min_gain`, and `target_block_size` (128 KB)
  - `Lz77Sequence { literals: Vec<u8>, offset: usize, match_length: usize }` — public parsed-sequence type
  - `MatchFinder`: hash-chain algorithm using multiply-shift hashing (`HASH_PRIME = 0x9E3779B1`); `find_sequences(&[u8], dict: &[u8]) -> Result<Vec<Lz77Sequence>>`; `reset()`; internal `CombinedBuffer` avoids copying dictionary data; 8-byte-at-a-time comparison via `get_u64()` fast path; constants `MIN_MATCH=3`, `MAX_MATCH=65539`
  - New public re-exports: `LevelConfig`, `Lz77Sequence`, `MatchFinder`

- **`huffman_encoder` module** — canonical Huffman coding for Zstandard literals:
  - `HuffmanEncoder::from_frequencies(frequencies: &[u64; 256]) -> Option<Self>` — returns `None` for ≤1 distinct symbol; constructs min-heap tree via `BinaryHeap`
  - `limit_code_lengths(code_lengths: &mut [u8], max_length: u8)` — Kraft inequality rebalancing to enforce `MAX_CODE_LENGTH = 11`
  - `serialize_table() -> Vec<u8>` — header byte = `127 + num_weight_symbols`, 4-bit weights packed two per byte, high nibble first
  - `encode_literals(literals: &[u8]) -> Vec<u8>` — produces backward-compatible sentinel byte stream
  - `get_code(symbol: u8) -> (u32, u8)`, `max_bits()`, `num_symbols()`, `weights()`

- **`fse_encoder` module** — FSE (Finite State Entropy) encoding tables and state machine:
  - `FseEncodeTable::from_frequencies(frequencies: &[u32], accuracy_log: u8) -> Option<Self>` — returns `None` for ≤1 distinct symbol; `normalize_frequencies()` with probability spreading; `spread_remainder()` for residual probability assignment; `serialize() -> Vec<u8>` (4-bit `accuracy_log - 5` header, then variable-length probability encoding); `reset_counters()`, `initial_state_for(symbol: u8) -> u16`, `get_encoding_info()`, `state_symbol()`, `encode_symbol()`
  - `FseStateEncoder<'a>`: `init(table, symbol: u8) -> Self`; `encode(symbol: u8) -> (u8, u32)` (returns bits to flush and their count); `flush() -> (u8, u32)`; `state() -> u16`
  - Standalone functions: `ll_code(literal_length: usize) -> (u8, u8, u32)`, `ml_code(match_length: usize) -> (u8, u8, u32)`, `of_code(offset: usize) -> (u8, u8, u32)` — encode Zstandard literal-length, match-length, and offset codes with baseline/extra-bits; `choose_mode(frequencies: &[u32], total: u32) -> SequenceCompressionMode`, `choose_accuracy_log(total: u32, distinct: usize) -> u8`
  - `pub enum SequenceCompressionMode { Predefined, Rle(u8), Fse(FseEncodeTable) }` — public encoding mode selector

- **`compressed_block` module** — Zstandard compressed block assembly:
  - `pub fn encode_compressed_block(sequences: &[Lz77Sequence]) -> Result<Vec<u8>>` — assembles a complete Zstandard compressed block from LZ77 sequences
  - Internal: literals-section encoding choosing Raw, RLE, or Compressed (Huffman) headers; `encode_sequences_section()` with variable-count encoding (1–3 bytes); `encode_sequences_bitstream()` using `BackwardBitWriter` and backward-order FSE state encoding; `compute_fse_states_backward()` traverses sequences in reverse; predefined FSE table probabilities for LL (accuracy_log=6, 36 symbols), OF (accuracy_log=5, 29 symbols), ML (accuracy_log=6, 53 symbols)

- **`streaming` module (public)** — `std::io` trait adapters for Zstandard:
  - `ZstdStreamEncoder<W: Write>`: `new(writer: W, level: i32)`, `with_dictionary(writer, level, dict: Vec<u8>)`, `finish() -> io::Result<W>` (must be called to flush), `buffered_bytes() -> usize`, `is_finished() -> bool`; implements `Write` buffering data until `finish()`
  - `ZstdStreamDecoder<R: Read>`: `new(reader: R)`, `with_dictionary(reader, _dict: Vec<u8>)`, `decompressed_size() -> usize`, `is_finished() -> bool`; implements `Read` with eager full-decompression on first call
  - New re-exports: `ZstdStreamEncoder`, `ZstdStreamDecoder`

- **`dict` module (public)** — Zstandard dictionary support:
  - `pub const MAX_DICT_SIZE: usize = 1_048_576` (1 MB limit)
  - `ZstdDict`: `new(data: Vec<u8>) -> Result<Self>` (rejects oversized data), `id() -> u32` (lower 32 bits of XXH64 with seed 0), `data() -> &[u8]`, `len()`, `is_empty()`, `into_data() -> Vec<u8>`
  - `pub fn train_dictionary(samples: &[&[u8]], dict_size: usize) -> Result<ZstdDict>` — n-gram extraction (lengths 4–16 bytes, `MIN_FREQUENCY = 2`), frequency×length scoring, descending-score sort, greedy substring deduplication; falls back to raw sample concatenation when no common n-grams exist; output capped at `dict_size` bytes
  - New re-exports: `ZstdDict`, `train_dictionary`

- **`ZstdEncoder` — new public API**:
  - `set_level(level: i32)` and `set_dictionary(dict_data: Vec<u8>)` mutating methods
  - `write_compressed_blocks(&[u8]) -> Result<Vec<u8>>` — dispatches to `MatchFinder` and `encode_compressed_block()`
  - Free functions: `compress_with_level(data: &[u8], level: i32) -> Result<Vec<u8>>`, `encode_all(data: &[u8], level: i32) -> Result<Vec<u8>>`, `decode_all(data: &[u8]) -> Result<Vec<u8>>`
  - Feature-gated `compress_parallel(data: &[u8], level: i32, num_threads: usize) -> Result<Vec<u8>>` (Rayon, `parallel` feature)
  - New re-exports: `compress_with_level`, `encode_all`, `decode_all`, `BackwardBitWriter`, `ForwardBitWriter`; feature-gated `compress_parallel`

#### oxiarc-lz4: Dictionary Frame Support

- **`frame_dict` submodule** — dictionary-aware LZ4 frame encoding and decoding:
  - Free functions: `compress_frame_with_dict(input: &[u8], dict: &Lz4Dict) -> Result<Vec<u8>>` (stores dict ID in FLG byte), `compress_frame_with_dict_options(input, dict, desc: FrameDescriptor) -> Result<Vec<u8>>`, `decompress_frame_with_dict(input, max_output, dict) -> Result<Vec<u8>>` (verifies dict ID matches frame header), `get_frame_dict_id(input: &[u8]) -> Result<Option<u32>>`
  - `Lz4DictFrameEncoder { dict, desc }`: `new(dict: Lz4Dict)`, `with_options(dict, desc)`, `encode(input: &[u8]) -> Result<Vec<u8>>`, `encode_with_size()`, `dict()`, `dict_id() -> u32`
  - `Lz4DictFrameDecoder { dict }`: `new(dict: Lz4Dict)`, `decode(input, max_output)`, `can_decode(input) -> bool` (checks dict ID), `dict()`, `dict_id() -> u32`
  - `Lz4DictCompressor`: implements `Compressor` trait; `new(dict)`, `with_options(dict, desc)`, `dict()`; full `reset()` support
  - `Lz4DictDecompressor`: implements `Decompressor` trait; `new(dict)`, `dict()`; full `reset()` support
- `FrameDescriptor::with_dict_id(id: u32)` — new builder method for setting dictionary ID in frame headers

### Refactored

#### oxiarc-lz4: `frame` Module Split

The monolithic `frame/mod.rs` was split into five dedicated submodules with no public API breakage:

- `frame/types.rs` — `BlockMaxSize` enum, `FrameDescriptor` struct, `LZ4_FRAME_MAGIC`, `LZ4_LEGACY_MAGIC`
- `frame/compress.rs` — `compress()`, `compress_with_options()`, `compress_with_options_parallel()`, `compress_parallel()` (feature-gated)
- `frame/decompress.rs` — `decompress()` supporting both `LZ4_FRAME_MAGIC` and `LZ4_LEGACY_MAGIC`; `decompress_frame()`, `decompress_legacy()`; adds legacy LZ4 format decoding
- `frame/streaming.rs` — `Lz4Compressor` and `Lz4Decompressor` (implementing `Compressor`/`Decompressor` core traits)
- `frame/frame_dict.rs` — new dictionary compression logic (see Added section above)

#### oxiarc-archive: ZIP `header` Module Split

The ZIP `header/mod.rs` was split into three dedicated submodules with no public API breakage:

- `header/types.rs` — enhanced type definitions:
  - `DataDescriptor` struct with `read<R: Read>(reader, is_zip64: bool) -> Result<(Self, usize)>`: handles optional `0x08074B50` signature detection and ZIP64 8-byte size fields
  - `CentralDirEntry`: new methods `needs_zip64() -> bool`, `build_zip64_extra() -> Vec<u8>`, `write<W: Write>()`, `written_size() -> usize`
  - `LocalFileHeader`: added `uncompressed_size_64: Option<u64>` and `compressed_size_64: Option<u64>` fields; new methods `parse_zip64_extra()`, `actual_uncompressed_size() -> u64`, `actual_compressed_size() -> u64`, `has_data_descriptor() -> bool`
  - New constants: `ZIP64_MARKER_16: u16 = 0xFFFF`, `FLAG_DATA_DESCRIPTOR: u16 = 0x0008`, `METHOD_AES_ENCRYPTED: u16 = 99`
  - New free functions: `is_entry_encrypted()`, `get_entry_aes_encryption_info()`, `is_entry_traditional_encrypted()`
- `header/reader.rs` — `ZipReader<R: Read + Seek>`:
  - Primary path `read_from_central_directory()` with ZIP64 EOCD64 locator support (`0x07064B50` signature); fallback `read_from_local_headers()`
  - `extract()`, `extract_with_password()` (ZipCrypto/PKWARE), `extract_with_password_aes()` (WinZip AE-2 with HMAC-SHA1 authentication tag verification), `extract_encrypted()` (auto-detects encryption method)
  - Static helpers: `is_encrypted()`, `get_aes_encryption_info()`, `is_traditional_encrypted()`; `entry_by_name()`
- `header/writer.rs` — `ZipWriter<W: Write>`:
  - `new()`, `set_compression()`, `add_file()`, `add_file_with_options()`, `add_encrypted_file()` (AES-256 CTR + PBKDF2-SHA1 default), `add_encrypted_file_with_options()`, `add_encrypted_file_traditional()`, `add_encrypted_file_traditional_with_options()`, `add_directory()`, `finish()`, `into_inner()`
  - Automatic ZIP64 upgrade: local headers, central directory entries, and EOCD all promote to ZIP64 when `compressed_size`, `uncompressed_size`, or file offset exceeds `0xFFFFFFFF`; extra field ID `0x0001`
  - Implements `Drop` calling `finish()`

### Changed

- **oxiarc-zstd**: `ZstdEncoder` internal structure extended with `level: i32` (0–22) field, `dictionary: Option<Vec<u8>>`, and `dict_id: Option<u32>`; dictionary ID written as 4-byte little-endian field in Zstandard frame header when present; `Single_Segment_flag` always set
- **oxiarc-zstd/fse.rs**: Added `FseTable::from_entries(accuracy_log: u8, entries: Vec<FseTableEntry>) -> Self` constructor; added `test_backward_writer_reader_roundtrip` test verifying `BackwardBitWriter` ↔ `FseBitReader` round-trip correctness
- **Dependencies**: Updated to latest versions
  - clap: 4.5.57 → 4.5.60
  - clap_complete: 4.5.65 → 4.5.66
  - indicatif: 0.18.3 → 0.18.4
  - dialoguer: 0.11.0 → 0.12.0
  - tokio: 1.49.0 → 1.50.0
  - memmap2: 0.9.9 → 0.9.10

### Documentation
- Updated README.md files for all subcrates

### Quality
- Zero clippy warnings (strict mode with `-D warnings`)
- Zero rustdoc warnings
- 100% test pass rate
- All policies compliant (no unwrap in production code, pure Rust, latest crates, workspace)
- Security audit passed

### Crates in This Release
All crates published at version 0.2.2:
- oxiarc-core, oxiarc-deflate, oxiarc-lzhuf, oxiarc-lzw, oxiarc-lzma
- oxiarc-bzip2, oxiarc-lz4, oxiarc-zstd, oxiarc-archive, oxiarc-cli

## [0.2.1] - 2026-02-09

### Added
- **oxiarc-archive**: ZIP encryption support
  - Traditional ZIP encryption (ZipCrypto) implementation
  - Encryption and decryption modules for password-protected archives
  - Comprehensive crypto primitives for secure archive handling
- **oxiarc-core**: Advanced I/O capabilities
  - Async I/O support for non-blocking operations
  - SIMD-accelerated CRC implementations for faster checksums
  - Memory-mapped I/O (mmap) support for efficient large file handling
  - Enhanced CRC benchmarks and performance testing
- **oxiarc-lz4**: Dictionary support for improved compression
  - LZ4 dictionary compression for better ratios on similar data
  - Dictionary API for streaming compression scenarios
- **oxiarc-lzhuf**: Streaming support
  - Streaming compression and decompression API
  - Comprehensive streaming integration tests
- **oxiarc-lzma**: LZMA2 chunking improvements
  - Enhanced LZMA2 chunk handling for better performance
  - Optimal parsing improvements for compression efficiency
- **oxiarc-deflate**: Enhanced compression capabilities
  - Improved LZ77 implementation with better match finding
  - Enhanced zlib support with more compression options
- **oxiarc-cli**: Enhanced utilities and command improvements
  - New utility modules for better file handling
  - Improved list and extract commands

### Changed
- **oxiarc-core**: Enhanced ring buffer implementation
- **oxiarc-deflate**: Optimized Huffman coding
- **CLI**: Improved error handling and user feedback

### Fixed
- Multi-file archive handling edge cases
- DEFLATE compression edge cases in simple scenarios

### Tests
- Added comprehensive ZIP encryption tests
- Added streaming integration tests for LZHUF
- Added multi-file bug regression tests
- Added simple DEFLATE test cases

## [0.2.0] - 2026-02-06

### Added
- **oxiarc-lzw**: Complete LZW compression implementation for TIFF and GIF formats
  - MSB-first and LSB-first bitstream support
  - Variable bit width encoding (9-12 bits for TIFF, 2-12 bits for GIF)
  - Configurable for TIFF and GIF compatibility modes
  - Comprehensive test suite with 427 total tests
- **Documentation**: Added comprehensive README.md files for all codec crates:
  - oxiarc-bzip2: BZip2 compression guide with examples
  - oxiarc-lz4: LZ4 compression guide with parallel compression examples
  - oxiarc-lzw: LZW compression guide for TIFF/GIF formats
  - oxiarc-zstd: Zstandard compression guide with FSE/Huffman details
- **Tests**: Marked resource-intensive stress tests with `#[ignore]` attribute
  - Reduced default test suite runtime from 137s to 32s
  - Stress tests can still be run with `cargo test -- --ignored`

### Changed
- **Dependencies**: Updated to latest versions
  - clap: 4.5.56 → 4.5.57
  - clap_complete: 4.5.56 → 4.5.65
  - criterion: 0.8.1 → 0.8.2
- **Workspace**: Improved workspace dependency management
  - Fixed oxiarc-lzw to use `workspace = true` for oxiarc-core dependency
  - All subcrates now consistently use workspace version references
- **Testing**: Optimized test performance without sacrificing coverage
  - Default `cargo test` now runs in ~32s (76% faster)
  - Parallel stress tests moved to optional ignored tests

### Fixed
- Version synchronization across all 10 workspace crates
- Workspace dependency references in oxiarc-lzw
- Publish script version updated to 0.2.0

### Quality
- ✓ Zero clippy warnings (strict mode with `-D warnings`)
- ✓ Zero rustdoc warnings
- ✓ 100% test pass rate (427/427 tests)
- ✓ All policies compliant (no unwrap, pure Rust, latest crates, workspace)
- ✓ Security audit passed (0 vulnerabilities, 131 dependencies scanned)

### Crates in This Release
All crates published at version 0.2.0:
- oxiarc-core: Core traits and utilities
- oxiarc-deflate: DEFLATE/GZIP compression
- oxiarc-lzhuf: LZHUF compression (LZH format)
- oxiarc-lzw: LZW compression (TIFF/GIF) **[NEW]**
- oxiarc-lzma: LZMA compression
- oxiarc-bzip2: BZip2 compression
- oxiarc-lz4: LZ4/LZ4-HC compression
- oxiarc-zstd: Zstandard compression
- oxiarc-archive: Multi-format archive support
- oxiarc-cli: Command-line interface

## [0.1.0] - 2026-01-17

### Added
- Initial release of OxiArc - Pure Rust Archive/Compression Library
- **oxiarc-core**: Foundation crate with core traits and utilities
  - `Compressor` and `Decompressor` traits
  - CRC32, CRC64, CRC16 implementations
  - Bitstream utilities
- **oxiarc-deflate**: DEFLATE compression implementation
  - RFC 1951 compliant
  - GZIP support (RFC 1952)
  - Huffman coding and LZ77 compression
- **oxiarc-lzhuf**: LZHUF compression
  - LZH archive format support
  - Sliding dictionary with static Huffman
- **oxiarc-lzma**: LZMA compression
  - LZMA1 and LZMA2 support
  - Range coding and LZ dictionary
  - XZ format support
- **oxiarc-bzip2**: BZip2 compression
  - Burrows-Wheeler Transform
  - Parallel compression with Rayon
  - Compression levels 1-9
- **oxiarc-lz4**: LZ4 compression
  - LZ4 frame format
  - LZ4-HC (high compression)
  - XXHash checksum support
  - Parallel compression
- **oxiarc-zstd**: Zstandard compression
  - FSE (Finite State Entropy) coding
  - Huffman coding
  - Parallel compression support
- **oxiarc-archive**: Multi-format archive handling
  - Format detection
  - ZIP, LZH, CAB, GZIP, BZIP2, LZ4, XZ, ZSTD support
- **oxiarc-cli**: Command-line interface
  - Compress/decompress commands
  - Archive extraction and creation
  - Multiple format support

### Quality Standards
- Pure Rust implementation (no C/Fortran dependencies)
- Zero unwrap() in production code
- Comprehensive test coverage
- Full documentation with examples
- Workspace-based dependency management

[0.2.6]: https://github.com/cool-japan/oxiarc/compare/v0.2.5...v0.2.6
[0.2.5]: https://github.com/cool-japan/oxiarc/compare/v0.2.4...v0.2.5
[0.2.4]: https://github.com/cool-japan/oxiarc/compare/v0.2.3...v0.2.4
[0.2.3]: https://github.com/cool-japan/oxiarc/compare/v0.2.2...v0.2.3
[0.2.2]: https://github.com/cool-japan/oxiarc/compare/v0.2.1...v0.2.2
[0.2.1]: https://github.com/cool-japan/oxiarc/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/cool-japan/oxiarc/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/cool-japan/oxiarc/releases/tag/v0.1.0
