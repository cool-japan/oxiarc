
# OxiArc - Development Roadmap (v0.4.1, 2026-07-30)

## Version History

- **v0.4.0** (2026-07-30) — DEFLATE/zlib decoder performance rewrite. Motivation: throughput — the inflate path read the bitstream one `Read::read` call at a time and wrote every decoded byte twice (once into a separate LZ77 ring buffer, once into the output). Rewritten: a buffered `BitReader` (`BitReader::buffered`/`with_buffer_capacity`) that refills via bulk 64-bit little-endian loads instead of per-bit reads, paired with a register-resident `BitCache` (`BitReader::detach`/`reattach`/`refill_cache`) a decoder's inner loop can decode many symbols against without the store/load-forwarding stall of a memory-resident accumulator (`BitReader::new`/exact mode is unchanged and still required wherever the reader must not advance past the bits actually consumed, e.g. ZIP's byte-aligned data descriptor immediately following a DEFLATE member); `HuffmanTree` now decodes through a two-level root+sub-table layout (root widened from a single-level 9-bit table to a 10-bit root, in the style of zlib's `inflate_table`/libdeflate) instead of one flat table; the LZ77 history is now the output buffer itself (`InflateWindow`, backed by `Vec::extend_from_within`) rather than a separate ring buffer that required writing every decoded byte twice; `Adler32::update` folds 32-byte groups through a closed-form reduction instead of one add-pair per byte, letting the compiler auto-vectorize it like zlib's `DO16` unrolling (output stays bit-identical). New opt-in zero-copy entry points: `oxiarc_deflate::inflate_into(src, dst)` and `zlib::zlib_decompress_into`, decompressing directly into a caller-supplied buffer with no intermediate `Vec` and no output-size guessing (a stream that would overflow `dst` is rejected with `BufferTooSmall` rather than truncated), plus `Inflater::with_output_capacity`/`MAX_OUTPUT_CAPACITY_HINT` to pre-size the decoder's output buffer from an untrusted size hint clamped to 64 MiB (GZIP decoding now seeds this automatically from the trailing ISIZE field). Non-breaking: no archive/stream wire format changed and no public API was removed — existing callers of `inflate`, `Inflater::new`, `zlib_decompress`, etc. see only a speed-up. New `oxiarc-deflate/tests/inflate_differential.rs` proves the buffered fast path, the exact-mode path, and the new `_into` APIs all agree byte-for-byte across stored/fixed/dynamic blocks, maximum-distance (32 KiB) back-references, and hostile/truncated/corrupted input (plus an optional CPython `zlib` oracle comparison behind the pre-existing `zlib-oracle` feature), and a new `fuzz_inflate_into` target cross-checks the growable-`Vec` and slice-sink decode paths byte-for-byte. Only oxiarc-core and oxiarc-deflate changed source this cycle; the other 11 crates are unchanged from 0.3.6. Final state: **2,468 tests passing, 0 failed** (2,329 via nextest across 101 binaries + 139 doctests, 13/13 crates green); zero clippy warnings (`--all-features --all-targets -D warnings`); zero rustdoc warnings; `cargo fmt --all --check` clean; release build clean; `cargo audit` clean; `cargo deny check bans` clean; ~116,354 Rust lines across 339 files (tokei).
- **v0.3.6** (2026-07-13) — reference-interop production-hardening campaign. Root problem: the test suites only exercised oxiarc→oxiarc round-trips, which **masked total interop failure** — several codecs were self-consistent *private dialects* that passed their own tests while failing ~100% against the reference implementation in both directions. A 22-auditor differential+audit investigation produced a 75-item remediation plan (P0–P3); **all 75 items were implemented**, and every codec is now validated by reference-tool differential testing, both directions, with permanent regression gates (new opt-in oracle Cargo features that self-skip when the tool is absent, so CI stays hermetic: `zstd-oracle`, `brotli-oracle`, `xz-oracle` in oxiarc-lzma and oxiarc-archive, `bzip2-oracle`, `lz4-oracle`, `snappy-oracle`, `zlib-oracle`, `tiff-oracle`, `libaec-oracle`, `zip-oracle` — joining 0.3.5's `lha-oracle`). Measured results: **zstd** (flagship; resolves Known Issue #1) — the true root cause was the FSE backward bitstream being read FIFO/LSB-first instead of RFC 8878 LIFO/MSB-first, mirrored in the writer (hence self-round-trips passed), compounded by the 4-stream Huffman jump table read as offsets instead of sizes and unvalidated FSE table/state indexing (the reported `fse.rs` index-OOB panic); baseline 0/64 real frames decoded (62 panics, 2 silent corruptions) → **64/64 corpus + 101/101 wide reference frames decode byte-identical; 85/85 oxiarc frames accepted by `zstd -d`; 9/9 dictionary frames; 60,000 fuzz cases, 0 panics**; the Huffman literals encoder is wired in (sequence FSE stays predefined/RLE — RFC-valid, a ratio limitation only, see Known Issues). **brotli** — full RFC 7932 rewrite of decoder AND encoder (window-bits tree, 704-symbol insert-and-copy table, block-type switching, code-length VLC, context maps + §7.1 LUTs, distance ring rules, metadata blocks, two-level Huffman tables) including the byte-exact 122,784-byte Appendix A static dictionary with all 121 transforms; baseline 141/172 reference-stream decode failures + 21 silently wrong and `brotli -d` rejecting 441/441 oxiarc outputs → **608/608 reference streams decode byte-identical (zero silent mismatches); 588/588 oxiarc streams accepted by `brotli -d`** (real compressed meta-blocks). **lzma/xz** — the LZMA2 decoder reset the uncompressed position per chunk, desyncing `pos_state`, so standard multi-chunk `.xz` was undecodable; now **60/60 `.xz` decode + 8/8 encode byte-identical vs `xz 5.8.3`** (multi-block OK), plus a stateful chunked encoder (cross-chunk matching) and `LzmaProperties` validation (`new(20,20,4)` previously SIGABRT'd via a multi-TiB allocation). **bzip2** — multi-stream/concatenated `.bz2` (pbzip2/lbzip2/`cat`) was silently truncated to the first stream; now full multi-stream decode, **324/324 both directions vs `bzip2 1.0.8`**, legacy randomised-block de-randomisation, a multi-Huffman-table encoder (libbz2 `sendMTFValues`) reaching **99.9% of the reference compression ratio** (was up to +50% larger), and `decompress_with_limit`. **szip** — AEC/CCSDS-121 framing put the RSI reference sample outside the block option-ID field, breaking libaec interop both ways incl. silent wrong output; rewritten per CCSDS-121.0-B-2 §5.2, validated against **live libaec 1.1.4: 2450/2450 decode + 4900/4900 encode byte-identical**. **lzw** — TIFF LZW had no Clear Code support (100% incompatible with real TIFF from libtiff/Pillow/GDAL); now **125/125 both directions vs Pillow/libtiff, with oxiarc's encoded output byte-identical to libtiff's**. **deflate** — the core codec was already correct (bit-exact vs CPython zlib/gzip + the gzip CLI), but the streaming/trait wrappers silently truncated output past 32 KiB, broke DEFLATE bit-continuity across calls, couldn't decode multi-member gzip (incl. oxiarc's own parallel output), and had an O(n²) bomb-amplifiable zlib concatenation path hanging >60s — all fixed and reference-verified. **lz4** — decompression bomb (per-sequence `max_output` bypass: 64 B → 1,020,020 B), LASTLITERALS(5) violation (reference lz4 rejected the output), and the ignored block-independence flag (`lz4 -BD` undecodable) all fixed; **11/11 encode + 44/44 decode + 3/3 linked-block vs `lz4 1.10.0`**. **snappy** — the 64 KiB per-chunk cap is now enforced (was 21x amplification) + bounded APIs. **lzhuf** — lh4–lh7 decoders returned `Ok` with silently truncated output on truncated streams (**4,442/4,442 truncation trials now return `Err`**; was 4,440 silent short-`Ok`); streaming decoder hardened; the 64-bit size extension header (0x42) is honored in the stream reader. **archive** — externally-encrypted ZIPs (`zip -e`, 7-Zip, WinRAR) were reported UNENCRYPTED and silently mis-extracted (detection used a homegrown 0xEE,0xEE marker instead of general-purpose bit 0) — fixed, plus DOS-date month=0 underflow panic, AE-2 CRC=0 spec compliance, the Info-ZIP data-descriptor check byte, and correct civil-date DOS timestamps; XZ: unbounded block-header allocation (28-byte crafted file → SIGABRT), filter-props OOB panic, and block-header CRC32 now validated; 7z: unbounded stream counts (capacity-overflow panic / TiB reservation) and the kCrc aggregate (175 bytes → 256 MB) bounded; CAB: MSZIP LZ77 window now carried across CFDATA blocks, CFDATA checksums validated, per-folder decode cache (was O(files × folder_size)), unknown methods now Err instead of silent stored; ISO: directory-record OOB panic on small LEN_DR fixed; TAR: GNU sparse ('S') and PAX 0.1 sparse entries fully supported in the streaming reader (bsdtar-verified byte-identical; previously silent mis-decode); also fixed a pre-existing flaky shared-temp-path race in the async_lzh/async_tar tests. **CLI** — `--memory-limit` (the advertised decompression-bomb defense) was silently unenforced for file-based gzip/xz/bzip2; now enforced **during** decode for all formats (gzip ISIZE, xz stream index, lz4/zstd declared sizes, bzip2/brotli/snappy bounded decoders): a brotli bomb under `--memory-limit 1M` peaks at **3.3 MB RSS vs 72.9 MB** and exits non-zero; `.br` files (no magic bytes) gained an extension fallback so file-path commands work; end-to-end matrix **303/303 pass** (13 formats × 7 subcommands, oxiarc- and reference-produced inputs, byte-identical); corrupt/truncated inputs across all formats: 0 panics, 0 exit-101; path traversal blocked. **API stability** — 18 public enums marked `#[non_exhaustive]` (formats/methods/status/errors), 106 `#[must_use]` on consuming builders, internal types no longer leaked in public signatures. Wire-format note: szip and the crate-private LZW stream framing changed from private dialects to the standard formats (old oxiarc-only streams of those two codecs are not readable — intended). Final state: **2,426 tests passing, 0 failed, 0 ignored** (2,289 via nextest across 100 binaries + 137 doctests; 112 suites); zero clippy warnings (`--all-features --all-targets`, also clean with `--no-default-features`); `cargo build --workspace --no-default-features` green (Pure Rust default preserved); `cargo fmt --all --check` clean; rustdoc clean; ~114,394 lines across 336 Rust files (tokei). Acid test: the 64 real-world zstd frames (OxiGDAL Zarr v3 chunks) that triggered this campaign decode **64/64 byte-identical through the `oxiarc` CLI, 0 panics**.
- **v0.3.6** (2026-07-08): Production-readiness hardening release. A structured 11-dimension audit (security, panic-freedom, API stability, tests/fuzz, docs, packaging, CLI UX, stubs/fabrications, cross-platform, format completeness) drove ~30 fixes across the workspace. **Security / untrusted-input**: fixed a Zip-Slip path-traversal in the 7z/CAB/ISO 9660 extract paths (they bypassed the `..`-stripping sanitizer the ZIP/TAR/LZH paths used; the sanitizer now also drops `..`/root/drive-prefix components and `resolve_output_path` adds a canonicalized-root containment check); bounded every header-driven allocation against the real remaining input length via `try_reserve`/`try_reserve_exact` (ZIP central-dir/entry counts + compressed sizes, LZH, 7z, TAR, XZ/LZMA dict up to a 1.5 GiB cap, zstd `content_size`), closing decompression-bomb / capacity-overflow-panic vectors that `--memory-limit` did not cover; fixed an integer-underflow panic in the encrypted-ZIP length computation (`checked_sub`) and a DOS-date-field underflow panic in `LocalFileHeader::modified_time`; made WinZip-AES tag / password-verification comparisons constant-time; replaced the AES-256-only cipher that silently zero-padded shorter keys with a genuine key-length-dependent AES-128/192/256; rejected spanned/multi-volume ZIP instead of misreading it. **Correctness (found by the new fuzz/example work)**: fixed a genuine LZMA2 multi-chunk decode bug that corrupted any varied (non-repeated-byte) input crossing more than one chunk boundary — this affected the DEFAULT `encode_lzma2`/`decode_lzma2` path for inputs above the 2 MiB chunk size (root cause: only the first chunk set the dictionary-reset flag, so the decoder's literal context diverged and the range coder desynced; every fresh-encoder chunk now resets the dict, spec-conformant LZMA2 reset field 3, liblzma-decodable, zero ratio loss); fixed an lh1 (LZHUF) decode DoS where a tiny malformed stream with a huge declared size looped producing zero-padding output until OOM (BitReader now signals exhaustion and decode errors promptly); corrected the CRC-32 runtime-dispatch diagnostics so `implementation_name()`/`is_simd_available()` report the path actually selected (the aarch64 PMULL constants were verified correct by exact software simulation of the clmul/fold/Barrett arithmetic and kept enabled; x86 PCLMULQDQ stays disabled pending validation). **API stability for 1.0**: marked status/mode enums (`FlushMode`, `DecompressStatus`, zstd `BlockType`/`LiteralsBlockType`, and the codec error enums) `#[non_exhaustive]`, demoted zstd advanced internal re-exports to `pub(crate)`, and made zstd encoder docs honest about its literals (Raw/RLE) + Predefined-FSE reality. **Tooling / tests**: proptest round-trip + no-panic suites across all 10 codecs, 18 cargo-fuzz harnesses for the decoder/parser entry points, runnable `examples/` across the crates, CAB/ISO/corrupt-input/CLI integration tests, regenerated shell completions + man pages (relocated under `oxiarc-cli/`), `deny.toml`, `SECURITY.md`/`CONTRIBUTING.md`, per-crate `LICENSE` symlinks + `[package.metadata.docs.rs]`, and `cargo package` excludes so oracle-corpus fixtures / `TODO.md` / stale `.bak` files no longer ship. 2,004 tests passing (all features, 1 skipped); zero clippy / check / rustdoc warnings; ~100,081 lines across 195 files. CI/CD was intentionally left out of scope for this release.
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
- [x] Zstandard (RFC 8878: full decoder — FSE incl. custom/repeat tables + 1/4-stream Huffman literals; encoder — LZ77 + Huffman-compressed literals + predefined/RLE FSE sequences; XXHash64, streaming, dict modules; reference-interop verified 2026-07-13, see Known Issues #2 for the remaining sequence-table ratio limitation)
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

1. **[RESOLVED 2026-07-13] oxiarc-zstd: FSE decoder panics on valid zstd frames** (confirmed 2026-07-13 via OxiGDAL 0.1.7 release testing; the `fse.rs` `index out of bounds: the len is 32 but the index is 65526` panic on valid frames that reference `zstd` decoded fine). Fixed in the production-hardening campaign (ZSTD-01..08). The root cause ran deeper than the originally-suspected normalized-count reconstruction: the FSE backward bitstream was read FIFO/LSB-first instead of RFC 8878 LIFO/MSB-first — mirrored in the writer, which is why oxiarc↔oxiarc round-trips passed while 0/64 real frames decoded — compounded by the 4-stream Huffman jump table being read as offsets instead of sizes, missing Huffman weight validation/implied-last-weight deduction, and unguarded `entries[state]` indexing (the reported panic site). All fixed, with probability-sum validation and bounds-checked state indexing added to `FseTable`. Verified: 64/64 OxiGDAL Zarr v3 corpus frames + 101/101 wide reference-frame matrix decode byte-identical (each corpus chunk decodes to the expected 16384 bytes); 85/85 oxiarc-encoded frames accepted by `zstd -d`; 60,000 fuzz cases, 0 panics; permanent regression gate behind the `zstd-oracle` feature plus an always-run embedded corpus. Remaining checklist (external to this repo):
   - [x] repair the FSE bitstream/table construction (root cause fixed; counts validated; state indexing bounds-checked)
   - [x] add the failing frames as regression tests (embedded corpus + `zstd-oracle` differential gate)
   - [x] **STALE, superseded — not tracked further in this repo.** This line originally meant "release the still-unpublished 0.3.6 that carries the FSE fix." The branch has since moved through 0.3.6 → 0.4.0 → 0.4.1 (see the version history above), i.e. oxiarc-zstd has been released multiple times since the FSE fix landed; the fix has been out via crates.io for several release cycles. Left unchecked before only because nobody had re-read this line since 0.3.6.
   - [ ] **Out of this repo's scope (downstream/external).** OxiGDAL is a separate project in this workspace; bumping its oxiarc-zstd dependency and removing its `#[ignore]` on `test_compute_zarr_stats_demo_fixture` is OxiGDAL's own backlog item, not oxiarc's. Tracked here only for historical context — do not resurface this as oxiarc remaining work in future audits.
2. **[RESOLVED 2026-08-04] oxiarc-zstd encoder: custom block-optimal FSE sequence tables.** Sequence sections used to be restricted to the RFC 8878 predefined/RLE FSE tables — RFC-valid, but a ratio limitation on inputs whose symbol distribution differs from the RFC's fixed one. `FSE_Compressed_Mode` is now emitted: `oxiarc-zstd/src/fse_encoder.rs` carries reference-faithful ports of `FSE_normalizeCount` (including the `FSE_normalizeM2` fallback) and `FSE_writeNCount`, plus a `ZSTD_fseBitCost`-style model, and `compressed_block.rs` picks per category (literal length / offset / match length) whichever of RLE, predefined and custom costs fewest bits *including* the table description. Custom tables also lift the predefined offset table's 0..=28 code ceiling to the format's full 0..=31. Measured on a 1.5 MB structured-record corpus: 173,521 → 109,359 bytes at level 1 (-37%). Gated by the `zstd-oracle` differential suite, which now walks the produced frame, asserts a block really used mode 2, and requires reference `zstd -d` to reproduce the input byte for byte. Huffman literal compression remains wired in as before.
3. **[RESOLVED 2026-08-04] oxiarc-brotli encoder: block splitting and context modeling.** The encoder used to hardcode `NBLTYPESL = NBLTYPESI = NBLTYPESD = 1`, so it could never use the block-type switching and context maps the decoder has always supported. Literal block splitting now ships (`oxiarc-brotli/src/block_split.rs`): the literal stream is segmented, segment histograms are clustered by merge cost, adjacent equal labels collapse into runs, and the meta-block declares up to 8 literal block types with a per-type prefix code bound through a move-to-front + zero-run-length-coded context map. The encoder measures the meta-block **both** ways and keeps the smaller, so a split can cost encode time but never compression ratio. Active at quality 10–11 only; quality 0–9 emits byte-identical output to the previously reference-verified path, which is asserted by a test. Measured on a 180 KB two-population input: 145,638 → 125,264 bytes (-14%). Gated by the `brotli-oracle` suite, which walks the stream header, asserts `NBLTYPESL > 1`, and requires reference `brotli -d` to reproduce the input. **[COMPLETED 2026-08-04]** The remainder now ships too: insert-and-copy (`NBLTYPESI`) and distance (`NBLTYPESD`) block splitting reuse the same clustering machinery over their own alphabets, and per-context histogram assignment is live for both context-mapped categories (`NTREESL`/`NTREESD` may exceed their block-type counts, i.e. two contexts of one block type get different prefix codes — the `LSB6` mode is now exploited, not merely declared). The three categories are searched by *coordinate ascent*: each is tried against the current best plan for the other two, and a change is kept only when the fully-written meta-block actually gets smaller, so no category has to buy another's header cost to be usable. Measured on a two-regime 162 KB corpus: 62,719 → 60,659 bytes; on 240 KB of context-dependent text: 146,769 → 142,703. Gated by five new `brotli-oracle` tests that decode the produced stream through this crate's own decoder to read back the real `NBLTYPESI`/`NBLTYPESD`/`NTREESL`/`NTREESD` (a new `decompress_reporting_shapes` API), assert each feature actually reached the wire, and then require reference `brotli -d` to reproduce the input — so none of them can pass vacuously. Quality 0-9 remains byte-frozen for every category, asserted directly.
4. **CAB**: Quantum and LZX compression are not implemented — entries using them are listed, and extraction returns a clean `unsupported_method` error (never a silent raw-copy fallback). None/MSZIP folders are fully supported (window carried across CFDATA blocks, CFDATA checksums validated).
5. **TAR**: GNU old-format ('S'), PAX 0.1 (`GNU.sparse.*` attributes), and PAX 1.0 (`GNU.sparse.major=1`/`GNU.sparse.minor=0`, in-data-stream decimal-ASCII preamble map) sparse entries are all fully supported for *reading* in both the seekable and streaming readers. Writer-side sparse emission (for any of the three variants) remains out of scope — `TarWriter` writes sparse-source files as regular dense entries; see `oxiarc-archive/TODO.md`'s "TAR Improvements" note.
6. BWT has O(n² log n) worst-case for highly repetitive data (mitigated by 900KB block size limit).
7. Previously-tracked issues that remain RESOLVED: bzip2 parallel compression bit-alignment (pre-0.3.4); LZMA/LZMA2 liblzma interop (v0.3.4: distance-slot probability layout, state mapping, LZMA2 control-byte reset field, embedded LZMA2 EOS marker).
8. **`cargo bench` / `--all-targets` requires a C compiler (dev-only, does not affect the shipped libraries).** `criterion` 0.8+ (the workspace's benchmark harness, used only as a `dev-dependencies` entry) has a mandatory `[target."cfg(any(windows, unix))".dependencies.alloca]` dependency, and `alloca` itself depends on `cc`. Every member crate's *default* features remain 100% Pure Rust with zero C/C++/Fortran dependencies — verified by walking the full `Cargo.lock` crate list, which contains no other build-time C dependency — but `cargo build --workspace --all-targets` and `cargo bench` specifically need a working C toolchain on `PATH`, which breaks the hermetic-build property on minimal CI images for that surface only. Deliberately not downgrading `criterion` to a pre-0.8 version without this dependency: CLAUDE.md's "Latest Crates" policy requires using the latest available version, and criterion 0.5.x/0.7.x predate a number of upstream fixes. Documented in CONTRIBUTING.md's "Getting Started" and "Benchmarks" sections; revisit if/when `alloca` becomes optional upstream.
9. **`fuzz/corpus/` carries roughly 50 MB of committed seed inputs across 18 fuzz targets**, so every clone of this repository pays that cost even though the corpora are excluded from the crates.io package (`cargo package` excludes, added in the v0.3.6 campaign). Not resolved in the 0.4.1 hygiene pass: shrinking a *tracked* directory requires either `git rm --cached` (removes future clones' cost but leaves the ~50 MB in every existing clone's history and every prior commit) or a full history rewrite (`git filter-repo`/BFG, which rewrites every commit hash on this branch) — both are state-changing git operations outside an automated hygiene pass's remit, and picking a relocation target (release-artifact upload, Git LFS, a separate corpus-only repo) is a maintainer decision, not a mechanical fix. `fuzz/artifacts/` remains empty across all 20 targets (no retained crash reproducers). Revisit with an explicit maintainer decision on where the corpora should live.

## Test Coverage

Per-crate counts measured 2026-07-30 (`cargo nextest run --workspace --all-features` + `cargo test --doc`; nextest tests + doctests):

- oxiarc-core: 196 tests (170 + 26 doctests)
  - CRC-32/64 slicing-by-8, DualCrc, SIMD CRC32 (aarch64 PMULL), bitstream (incl. the short-read `fill_buffer` fix), msb_bitstream, ringbuffer (bounded back-reference copy lengths), EntryBuilder, Serde serialization
- oxiarc-deflate: 293 tests (273 + 20 doctests)
  - Dynamic Huffman, Zlib wrapper, Adler-32, compression levels, async deflate, gzip module (multi-member decode), streaming (true incremental Compressor/Decompressor trait semantics, flush modes, bit-continuity across `deflate(_, false)` calls), optimal parsing, parallel GZIP, LZ77 tuning, memory pool, `with_max_output` bomb cap on ZlibStreamDecoder, CPython zlib/gzip + gzip-CLI differential suite (`zlib-oracle`)
- oxiarc-lzhuf: 188 tests (183 + 5 doctests)
  - LH1/LH4-7 roundtrips (incl. beyond-window), truncation-guard regressions (lh4-7 short streams now Err), streaming decoder hardening, LZSS, Huffman trees, optimal parser, custom dictionaries, real-LHA corpus + `lha-oracle`
- oxiarc-bzip2: 108 tests (104 + 4 doctests)
  - MSB-first bit I/O, non-reflected CRC-32, BWT, MTF/symbol-map Huffman, multi-stream (concatenated) decode, de-randomisation (legacy blocks), multi-Huffman-table encoder (sendMTFValues), `decompress_with_limit`, reference `bzip2` differential suite (`bzip2-oracle`)
- oxiarc-lz4: 166 tests (151 + 15 doctests)
  - Frame format, XXHash32, LZ4-HC, per-sequence `max_output` enforcement (bomb regression), LASTLITERALS(5) invariant, block-dependent (linked) frames (`FrameDescriptor::with_block_independence`), parallel compression, dictionaries, `lz4` CLI differential suite (`lz4-oracle`)
- oxiarc-zstd: 208 tests (205 + 3 doctests)
  - RFC 8878 LIFO/MSB backward bitstream (FSE reader AND writer), FSE table probability-sum validation + bounds-checked states, Huffman weight validation + implied-last-weight, 4-stream jump-table sizes, Huffman literals encoder, frame parsing, dictionaries, multi-frame, OxiGDAL Zarr corpus regression, reference `zstd` differential suite (`zstd-oracle`)
- oxiarc-archive: 524 tests (495 + 29 doctests)
  - ZIP (GP-bit-0 encryption detection, AE-2 CRC=0, Info-ZIP check byte, civil-date DOS timestamps, name encoding, Zip64, data descriptors), TAR (GNU 'S' + PAX 0.1 sparse in both readers, PAX long names), XZ (multi-chunk LZMA2, block-header CRC validation, bounded header allocs), 7z (bounded stream counts/kCrc), CAB (MSZIP window carry, CFDATA checksum, folder cache, unknown-method Err), ISO 9660 (LEN_DR bounds), LZH (0x42 64-bit size in stream reader), archive repair, async readers (de-flaked temp paths), `zip-oracle`/`xz-oracle`/`lha-oracle` differential suites
- oxiarc-lzma: 187 tests (178 + 9 doctests)
  - LZMA/LZMA2 (persistent cross-chunk uncompressed position, stateful chunked encoder, `LzmaProperties` validation), optimal parsing, range coder, BT4 match finder, parallel LZMA2, dictionaries, memory pool, liblzma golden vectors, live `xz` differential suite (`xz-oracle`)
- oxiarc-lzw: 100 tests (93 + 7 doctests)
  - GIF/TIFF configurations (TIFF Clear Code per TIFF 6.0/libtiff semantics), gif_lzw, bitstream_lsb, fallible `LzwConfig::new`, length-framed stream format, Pillow/libtiff differential suite (`tiff-oracle`)
- oxiarc-brotli: 219 tests (207 + 12 doctests)
  - RFC 7932 decoder/encoder (block-type switching, context maps + §7.1 LUTs, code-length VLC, distance ring, metadata blocks, two-level Huffman), byte-exact Appendix A dictionary (CRC-verified) + 121 transforms, strict trailing-garbage/padding rejection, reference `brotli` differential suite (`brotli-oracle`)
- oxiarc-snappy: 140 tests (132 + 8 doctests)
  - Block + framed formats, 64 KiB per-chunk cap enforcement, bounded-decode APIs, CRC32C, memory pool, dictionaries, async I/O, cramjam/python-snappy differential suite (`snappy-oracle`)
- oxiarc-szip: 47 tests (46 + 1 doctest)
  - CCSDS-121.0-B-2 §5.2 framing (option ID before all RSI first-block samples), zero-block/ROS/second-extension options, typed validation errors, embedded libaec fixtures + live libaec 1.1.4 differential suite (`libaec-oracle`)
- oxiarc-cli: 92 tests
  - End-to-end format×subcommand matrix, `--memory-limit` enforcement during decode (all formats), `.br` extension fallback, corrupt-input no-panic sweeps, path-traversal, exit codes
- Total: **2,468 tests passing, 0 failed, 0 ignored** (2,329 via `cargo nextest run --workspace --all-features` across 101 test binaries + 139 doctests; 113 suites — 101 binaries + 12 crates with nonzero doctests, oxiarc-cli has none). Zero clippy warnings (`--all-features --all-targets`, also with `--no-default-features`); `cargo fmt --all --check` and rustdoc clean. Oracle suites self-skip when the reference tool is absent, so the counts are reproducible on a hermetic machine.

## Code Statistics (measured 2026-07-30; tokei Rust code lines per crate incl. tests/examples)

| Crate | Lines of Code |
|-------|---------------|
| oxiarc-core | ~6,043 (CRC-32/64 slicing-by-8, DualCrc, bitstream/msb_bitstream — now with buffered `BitReader`/`BitCache` — bounded ringbuffer copies, EntryBuilder, Serde) |
| oxiarc-deflate | ~9,893 (Zlib wrapper, Adler-32, async_deflate, gzip multi-member, true streaming Compressor/Decompressor, zlib-oracle suite, new `inflate_into`/`InflateWindow`/two-level Huffman table, `inflate_differential` suite) |
| oxiarc-lzhuf | ~6,606 (canonical MSB-first LZSS + Huffman: lh0/1/4/5/6/7/lhd, truncation guards, custom dictionaries, lha-oracle) |
| oxiarc-bzip2 | ~3,303 (MSB-first bit I/O, multi-stream decode, de-randomisation, multi-table sendMTFValues encoder, decompress_with_limit, bzip2-oracle) |
| oxiarc-lz4 | ~5,971 (frame format, XXHash32, LZ4-HC, per-sequence output caps, linked-block frames, lz4-oracle) |
| oxiarc-zstd | ~7,336 (RFC 8878 LIFO/MSB FSE bitstream both directions, validated FSE/Huffman tables, Huffman literals encoder, dict, streaming, zstd-oracle) |
| oxiarc-brotli | ~7,153 (full RFC 7932 decoder+encoder, tables.rs, 122,784-byte Appendix A dict_data.bin + 121 transforms, two-level Huffman, brotli-oracle) |
| oxiarc-snappy | ~4,304 (block + framed formats, 64 KiB chunk-cap enforcement, bounded decode, CRC32C, snappy-oracle) |
| oxiarc-szip | ~1,902 (CCSDS-121.0-B-2 §5.2 framing, zero-block/ROS/second-extension, typed validation errors, libaec-oracle) |
| oxiarc-archive | ~22,153 (ZIP GP-bit encryption detection + AE-2/timestamps, TAR sparse, XZ header CRC + bounded allocs, 7z count caps, CAB window/checksum/cache, ISO bounds, zip/xz/lha oracles) |
| oxiarc-cli | ~6,897 (enforced --memory-limit during decode, .br fallback, e2e format×subcommand matrix) |
| oxiarc-lzma | ~7,957 (persistent-position LZMA2 decode, stateful chunked encoder, validated LzmaProperties, xz-oracle) |
| oxiarc-lzw | ~2,775 (TIFF Clear Code per libtiff, gif_lzw, bitstream_lsb, fallible LzwConfig, tiff-oracle) |
| **Total** | **~90,686 code lines** (317 Rust files across the 13 crates; workspace-wide incl. fuzz harnesses: 114,394 total lines / 90,935 code lines, 336 Rust files) |

## Stubs to implement (added 2026-06-22 by /cooljapan-stub-check)

- [x] LZH compression (lh5) encoder now spec-compatible (completed 2026-07-07) — MSB-first canonical rewrite of the lh4/lh5/lh6/lh7 codec (new `oxiarc-core::msb_bitstream`; `oxiarc-lzhuf/src/{encode,huffman,decode,lzss,optimal,methods}.rs` + `streaming/{decoder,huffman}.rs`) producing genuine LHA-wire-format archives; `oxiarc-archive/src/lzh/{writer,header}.rs` needed **no** changes — the default level-2 writer already emits headers real `lha` reads. Validated byte-exact against 6 genuine real-LHA `.lzh` fixtures (levels 0/1/2, incl. a 1.24 MB multi-block file, cross-tool) AND a live `lha` (Lhasa 0.6.0) oracle at **both** the codec level (`oxiarc-lzhuf` `lha-oracle`: `lha t`/`x` round-trips) and the **archive** level (new `oxiarc-archive` `lha-oracle`: `lha l`/`t`/`x`/`pq` over small, ~150 KB multi-block, multi-file, Shift_JIS-named, empty and stored archives); reverse direction (real archive → `LzhReader`) added as always-run `tests/lzh_corpus_reader.rs`. Suites green (oxiarc-archive 389, oxiarc-lzhuf 163), clippy clean, oracle self-skips without `lha`. Residual (out of scope, non-blocking, pre-existing): the opt-in level-1/0 writer stores Unix time where canonical LHA uses DOS FAT time (cosmetic date only — `lha` still CRC-tests + extracts fine), and Lhasa does not read level-3 headers at all; the default level-2 path is fully clean. Prior fix (v0.3.4, 2026-07-06): an earlier, already-shipped beyond-window corruption bug (LZSS clobbering history past the window, a 16-bit per-block size field overflowing past 65535 bytes, wrong p-tree count-field width for np=16/17) was fixed under this same item with 8/16/64/100 KB CRC-16 regression tests; that fix stands independently — this canonical-format rewrite was the separate, now-closed real-tool-readability work.
