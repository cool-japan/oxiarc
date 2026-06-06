
# oxiarc-archive - Development Status (v0.3.3, 2026-06-06)

## Completed Features (COMPLETE)

### Format Detection (~300 lines)
- [x] `ArchiveFormat` enum
- [x] Magic byte detection for ZIP, GZIP, 7z, XZ, BZip2, Zstd, LZH, TAR, LZ4, CAB, Brotli, Snappy
- [x] `detect()` function from reader
- [x] `from_magic()` for direct byte analysis
- [x] Extension and MIME type mappings
- [x] Archive vs. compression-only classification

### ZIP (~3,000 lines)
- [x] Local file header parsing
- [x] Central directory parsing
- [x] End of central directory
- [x] `ZipReader` with entry enumeration
- [x] File extraction with DEFLATE
- [x] File extraction with Stored method
- [x] CRC-32 verification
- [x] UTF-8 filename support (flag bit 11)
- [x] Local file header writing
- [x] Central directory writing
- [x] End of central directory writing
- [x] DEFLATE compression
- [x] CRC-32 computation during write
- [x] Zip64 for large files
- [x] Zip64 read support (>4GB files)
- [x] Data descriptor support
- [x] Archive comments
- [x] Async ZIP I/O support

### GZIP (~800 lines)
- [x] Header parsing (RFC 1952)
- [x] Magic byte validation (0x1F 0x8B)
- [x] Compression method check (CM=8 = DEFLATE)
- [x] Optional fields:
  - [x] FTEXT flag
  - [x] FHCRC (header CRC-16)
  - [x] FEXTRA (extra field)
  - [x] FNAME (original filename)
  - [x] FCOMMENT (comment)
- [x] `GzipReader` with decompression
- [x] Trailer parsing (CRC-32 + ISIZE)
- [x] Header writing
- [x] DEFLATE compression (write)
- [x] Trailer writing

### TAR (~1,500 lines)
- [x] UStar header parsing
- [x] 512-byte block structure
- [x] Header fields: name, mode, uid, gid, size, mtime, chksum, typeflag, linkname
- [x] UStar magic detection
- [x] Prefix field for long names
- [x] `TarReader` with entry enumeration
- [x] File type detection (regular, directory, symlink, etc.)
- [x] File extraction
- [x] PAX extended headers
- [x] GNU long names
- [x] Archive creation

### LZH (~1,000 lines)
- [x] Level 0 header parsing
- [x] Level 1 header parsing
- [x] Level 2 header parsing
- [x] Extension headers:
  - [x] 0x00: Common header
  - [x] 0x01: Filename
  - [x] 0x02: Directory path
  - [x] 0x50-0x54: Unix attributes
- [x] Shift_JIS filename decoding (via encoding_rs)
- [x] CRC-16 verification
- [x] `LzhReader` with entry enumeration
- [x] Path sanitization
- [x] File extraction with all methods
- [x] Archive creation

### XZ (~600 lines)
- [x] XZ (.xz) read support
- [x] Stream header/footer parsing
- [x] Block and index handling
- [x] LZMA2 decompression
- [x] Extraction support

### 7-Zip (~500 lines)
- [x] 7-Zip (.7z) read support
- [x] Signature header parsing
- [x] Entry enumeration
- [x] Extraction support

### CAB (~400 lines)
- [x] CAB (.cab) read support
- [x] Cabinet header parsing
- [x] Folder/file enumeration
- [x] Extraction support

### LZ4 / Zstd / Bzip2 Archive (~500 lines)
- [x] LZ4 frame read/write support
- [x] Zstd frame read/write support
- [x] Bzip2 stream read/write support

### Brotli / Snappy Archive (NEW in v0.2.6)
- [x] BrotliReader for decompression (.br/.brotli)
- [x] BrotliWriter for compression (.br/.brotli)
- [x] SnappyReader for decompression (.sz/.snappy)
- [x] SnappyWriter for compression (.sz/.snappy)
- [x] Format detection for Brotli and Snappy

## Future Enhancements

### ZIP Improvements
- [x] ZIP encryption (traditional ZipCrypto) — implemented in 0.2.8
- [x] ZIP encryption (AES-256) — implemented in 0.2.8
- [ ] Split/multi-part archives
- [x] ZIP streaming reader — data-descriptor (flag bit 3) support (planned 2026-04-20) — DEFLATE+bit3 works, Stored+bit3 still requires Seek
- [x] Byte-fidelity raw-preserve in `oxiarc add` for ZIP and LZH (completed 2026-05-06)
  - **Goal:** `oxiarc add` rewrites ZIP and LZH archives preserving existing entries byte-for-byte (compressed payload + method + CRC + sizes), eliminating decompress→recompress round-trip. TAR path extended to preserve symlink/special metadata.
  - **Design:** NEW `ZipWriter::add_file_raw(name, method, crc32, uncompressed_size, compressed_data)` in zip/header/writer.rs — writes LFH with supplied method/CRC/sizes, payload verbatim, updates central dir. NEW `LzhReader::read_raw_method_data(entry) -> Result<(LzhMethod, Vec<u8>, u16)>` in lzh/mod.rs (skip CRC verify). NEW `LzhWriter::add_file_raw(name, method, crc16, original_size, compressed_data, mtime, attrs)` in lzh/mod.rs. Refactor `AddEntry` in oxiarc-cli/src/commands/add.rs (line 23) from tuple to struct/enum with method+crc+sizes variants. ZIP path uses `extract_raw`+`add_file_raw`; LZH path uses new raw read+write; TAR path preserves all metadata. Run `rslines 50` on lzh/mod.rs; invoke `splitrs` if >2000 lines.
  - **Files:** MODIFY `oxiarc-archive/src/zip/header/writer.rs`, MODIFY `oxiarc-archive/src/lzh/mod.rs`, MODIFY `oxiarc-cli/src/commands/add.rs`
  - **Tests:** `test_zip_add_preserves_compressed_bytes`, `test_zip_add_preserves_method_and_crc`, `test_lzh_add_preserves_compressed_bytes`, `test_tar_add_preserves_symlink`, `test_tar_add_preserves_mode_and_uname`
  - **Risk:** LZH header CRC must be recomputed in `add_file_raw`; reuse `add_file_with_metadata` header-CRC path.

### TAR Improvements
- [x] TAR sparse file support (GNU old-format + PAX GNU.sparse.*) (planned 2026-04-20) — materializes logical content with zero-fill; hole-preservation on disk is out of scope

### LZH Improvements
- [x] Level 3 headers (planned 2026-04-20)
  - **Goal:** `LzhWriter` can emit level-3 headers via a builder option; reader already handles level 3. Complete the bidirectional support with a round-trip test.
  - **Design:**
    - `LzhWriter::with_header_level(level: u8) -> Self` builder (level ∈ {0, 1, 2, 3}); default stays at level 1 for compatibility.
    - New `LzhWriter::write_level3_header(name, compressed, original_size, crc16, mtime, method)` method — mirrors `write_level1_header` at line 578 but emits the level-3 structure (4-byte word-size prefix, method-id, 4-byte compressed/original sizes, 4-byte mtime-unix, attributes, level=3, reserved=0x20, filename via extension header 0x01, crc16 via extension header 0x02, etc.).
    - Dispatch in `add_file` / `add_stream` based on configured level.
  - **Files:**
    - MODIFY `oxiarc-archive/src/lzh/mod.rs` (add level-3 writer method + builder; ~150 LoC).
  - **Prerequisites:** none.
  - **Tests:** round-trip — write 3 files at level 3, read back via `LzhReader`, verify names/sizes/crcs match. Extend `test_level3_header_parsing` with a writer-side symmetric test.
  - **Risk:** level-3 extension-header encoding is fiddly (common header, extension header size in 4 bytes). Mitigated by reading the level-3 reader code (already implemented at lines 120-230) as the authoritative reference format — the writer is its inverse.
- [x] LZH extension headers — 0x40/0x41/0x42/0x43/0x44/0x46/0x50 read + write (planned 2026-04-20)
- [x] More extension headers (0x40 OS attr u16, 0x41 Windows timestamps, 0x42/0x43 64-bit sizes, 0x44 comment, 0x46 Unix perms, 0x50 owner names, 0x51 owner IDs, 0x54 Unix mtime)

### New Formats
- [ ] RAR read support (licensing?)
- [x] ISO 9660 read support — PVD + Joliet UCS-2 filenames (completed 2026-05-06)
  - **Goal:** `oxiarc list/extract/info/detect` work on ISO 9660 images. PVD + Joliet SVD support, fallback to 8.3 names. No Rock Ridge / El Torito / UDF / write / multi-session.
  - **Design:** NEW `oxiarc-archive/src/iso9660/` with: `mod.rs` (`IsoReader<R: Read+Seek>`, new/entries/extract), `volume_descriptor.rs` (PVD type-1 + Joliet SVD type-2, 2048-byte sector walk from LBA 16), `directory_record.rs` (ECMA-119 §9.1, recursive walker), `joliet.rs` (UCS-2 BE→UTF-8). Register in lib.rs. `ArchiveFormat::Iso9660` detected at offset 32768 by magic `CD001`. CLI match arms in list/extract/info/detect. Test fixture: `build_minimal_iso() -> Vec<u8>` hand-crafted 32-KiB ISO byte literal (system area 0-15, PVD at 16, Joliet SVD at 17, terminator at 18, path table at 19, root dir at 20, file data at 21-22) with field-by-field ECMA-119 section comments.
  - **Files:** NEW `oxiarc-archive/src/iso9660/{mod,volume_descriptor,directory_record,joliet}.rs`, MODIFY `oxiarc-archive/src/{lib,format}.rs`, MODIFY `oxiarc-cli/src/commands/{list,extract,info,detect}.rs`
  - **Tests:** `test_iso_detect_magic_at_lba_16`, `test_iso_pvd_parses`, `test_iso_joliet_filename_decode`, `test_iso_directory_record_walk`, `test_iso_extract_file_content`, `test_iso_level1_fallback`, CLI `test_cli_list_iso`
  - **Risk:** Fixture self-consistency risk — field-by-field ECMA-119 comments in builder function mitigate.

### General
- [x] Streaming extraction (without buffering entire file) (completed 2026-04-20)
  - **Goal:** Three archive formats expose a streaming-extraction API that reads one entry at a time without holding the full archive in memory and without requiring `Seek`. TAR: formalize the in-flight `TarStreamReader`. ZIP: new `ZipStreamReader`. LZH: new `LzhStreamReader`. All three return per-entry readers that implement `std::io::Read`, drop-safely skip unread data, and report progress / honor cancellation via the new core primitives.
  - **Design:**
    - **TAR** — move the *already-written* `TarStreamReader` code from `oxiarc-archive/src/tar/mod.rs` into a new `oxiarc-archive/src/tar/stream.rs` file. No API change — `pub use` from `tar/mod.rs` preserved, `lib.rs` re-export unchanged. This frees the mainline `tar/mod.rs` (1365 lines after the recent +284 addition) from further growth.
    - **ZIP** — new `oxiarc-archive/src/zip/stream.rs`. `ZipStreamReader<R: Read>` walks local file headers inline (signature `PK\x03\x04`). For each entry: parse LFH, detect general-purpose flag bit 3 (data-descriptor), construct the appropriate decompressor (STORE pass-through or DEFLATE via `oxiarc_deflate::streaming::Inflater`), yield a `ZipStreamEntry<'_, R>: Read`. **Critical: data-descriptor path must be codec-EOF-driven.** When bit 3 is set the LFH size fields are zero; decompress until the codec reports end-of-stream, then read the trailing `PK\x07\x08` descriptor (12 or 16 bytes depending on Zip64 via extra-field inspection). Do NOT scan for the `PK\x07\x08` signature in the compressed stream — the byte sequence can legitimately appear inside DEFLATE output and will false-match. Spec ref: PKWARE APPNOTE.TXT §4.3.9. Initial methods: STORE (method 0) + DEFLATE (method 8); others return `OxiarcError::UnsupportedMethod`.
    - **LZH** — new `oxiarc-archive/src/lzh/stream.rs`. `LzhStreamReader<R: Read>` walks LZH level-0/1/2 headers sequentially. Per entry: parse header, read declared packed size bytes, hand back to decompressor per method (-lh0- passthrough, -lh5-/-lh6-/-lh7- via `oxiarc_lzhuf`). Yield `LzhStreamEntry<'_, R>: Read`. Level-3 headers return `UnsupportedHeader`.
    - **Plumbing (all three):** per-entry reader tracks `pending_skip` remaining bytes; `Drop` impl consumes them. Optional `Arc<dyn ProgressSink>` and `CancellationToken` from oxiarc-core.
  - **Files:** `tar/mod.rs` (MODIFY), `tar/stream.rs` (NEW), `zip/stream.rs` (NEW), `zip/mod.rs` (MODIFY), `lzh/stream.rs` (NEW), `lzh/mod.rs` (MODIFY), `src/lib.rs` (MODIFY)
  - **Prerequisites:** `ProgressSink` trait in `oxiarc-core::progress`; `CancellationToken` in `oxiarc-core::cancel`
  - **Tests:** TAR (3 existing + 4 new), ZIP (7 new), LZH (6 new), cross-format round-trip for each format
  - **Risk:** ZIP data-descriptor path is subtle (Zip64 may use 16-byte form); mitigated by explicit Zip64 extra-field detection. LZH level-3 returns error, never panics.
- [x] Async I/O support for more formats (planned 2026-04-20)
  - **Goal:** Replicate the `async_zip` pattern (`read_zip_entry_async`, `decompress_zip_entry_async`, `read_zip_entry_from_reader_async`) for TAR and LZH under `oxiarc-archive/src/async_tar.rs` and `async_lzh.rs`, gated on `async-io` feature.
  - **Design:**
    - `async_tar.rs` — `read_tar_entry_async(path, index) -> Future<Result<Vec<u8>>>`, `read_tar_entries_async(path) -> Future<Result<Vec<TarEntry>>>` via `tokio::task::spawn_blocking` wrapping sync reads. Mirrors `async_zip` structure.
    - `async_lzh.rs` — same pattern.
    - Re-export from `lib.rs` under `#[cfg(feature = "async-io")]`.
  - **Files:**
    - NEW `oxiarc-archive/src/async_tar.rs` (~120 lines).
    - NEW `oxiarc-archive/src/async_lzh.rs` (~120 lines).
    - MODIFY `oxiarc-archive/src/lib.rs` — add conditional `pub mod`s + re-exports.
    - MODIFY `oxiarc-archive/Cargo.toml` — confirm `tokio` is a workspace dep gated by `async-io` feature.
  - **Prerequisites:** none; async_zip pattern is the reference.
  - **Tests:** one round-trip per format — write archive via sync API, read asynchronously via new APIs, compare contents.
  - **Risk:** `spawn_blocking` is the pragmatic choice (full async streams would require tokio-aware codec internals, which is out of scope here). Document this trade-off in module docs.
- [x] Progress callbacks (planned 2026-04-20)
  - **Goal:** **Core container-format** readers/writers in `oxiarc-archive` accept an optional `ProgressHandle` and emit `on_progress` per read/write chunk, plus `on_entry(name, index)` at entry boundaries. Scope narrowed to five formats: **ZIP, TAR, LZH, gzip, CAB**. Streaming readers already expose `.with_progress()`; non-streaming readers/writers gain the same builder method.
  - **Out of scope (deferred):** thin codec wrappers in `oxiarc-archive` around `oxiarc-bzip2` / `oxiarc-xz` / `oxiarc-zstd` / `oxiarc-lz4` / `oxiarc-brotli` / `oxiarc-snappy`, and the 7z reader. The underlying codec crates get their own progress wiring via items 2-5; propagating that into the archive-crate wrappers is a cleanup follow-up that should land in a dedicated pass once all codec-side APIs settle.
  - **Design:**
    - Add `.with_progress(handle: ProgressHandle)` + internal `Option<ProgressHandle>` field to `ZipReader`, `ZipWriter`, `TarReader`, `TarWriter`, `LzhReader`, `LzhWriter`, `GzipReader`, `CabReader`.
    - Progress sites: for readers, at each entry extract / per read chunk; for writers, at each entry add / per write chunk.
    - `on_entry(name, entry_index)` fires once per entry (zero-indexed) for multi-entry formats (ZIP, TAR, LZH, CAB). Gzip is single-stream — emit `on_entry(filename_from_gz_header_or_"<stream>", 0)` once.
    - `on_progress(processed, total)` fires at least once per 16 KiB chunk.
  - **Files:**
    - MODIFY `oxiarc-archive/src/zip/header/reader.rs` (add `progress: Option<ProgressHandle>` field, `.with_progress()` builder, emit hooks in `extract`/`extract_to`)
    - MODIFY `oxiarc-archive/src/zip/header/writer.rs` (similar)
    - MODIFY `oxiarc-archive/src/tar/mod.rs` (TarReader + TarWriter; streaming reader already wired)
    - MODIFY `oxiarc-archive/src/lzh/mod.rs` (LzhReader + LzhWriter; streaming reader already wired)
    - MODIFY `oxiarc-archive/src/gzip/mod.rs`, `cab/mod.rs` (reader side only — CAB has no writer)
  - **Prerequisites:** `ProgressSink` already in `oxiarc-core` (landed previous run).
  - **Tests:** counting-sink fixture verifies monotonic `processed`, exactly-once `on_entry` per entry, and matching entry names; one test per format that has progress wiring (5 formats × read, 4 formats × write = 9 tests).
  - **Risk:** API addition only; existing tests keep passing because `Option<ProgressHandle>` defaults to `None`. Mitigation: run full archive crate test suite.
- [x] Memory-mapped files (planned 2026-04-20) — `open_zip_mmap`, `open_tar_mmap`, `open_lzh_mmap` live
- [x] Archive-crate codec wrappers forward progress/cancellation (planned 2026-04-20) — `BrotliReader`/`BrotliWriter`, `Bzip2Reader`/`Bzip2Writer`, `SnappyReader`/`SnappyWriter`, `Lz4Reader`/`Lz4Writer`, `XzReader`/`XzWriter`, `ZstdReader`/`ZstdWriter` all grew `.with_progress(ProgressHandle)` / `.with_cancel(CancellationToken)` builders. Brotli/Snappy forward to streaming builders on `oxiarc-brotli::BrotliCompressor/Decompressor` and `oxiarc-snappy::FrameEncoder/Decoder`. Bzip2 forwards to new `BzEncoder`/`BzDecoder` builders added to `oxiarc-bzip2` in the same pass (reference implementation; per-block progress). LZ4/XZ/Zstd use wrapper self-emission (one-shot `on_progress`+`on_finish`, cancel-check before the compress/decompress call) because `oxiarc-lz4`, `oxiarc-lzma::Lzma2Encoder/Decoder`, and `oxiarc-zstd::ZstdEncoder` still lack builder hooks — upgrading those crates to per-chunk builders is deferred follow-up. Tests: `{brotli,bzip2,lz4,snappy,xz,zstd}::tests::test_*_progress_forwarding` and matching `*_cancel_forwarding`.
- [x] Archive repair/recovery — `ZipRepair` / `TarRepair` structs + `repair_zip` / `repair_tar` convenience functions; rolling LFH scanner for ZIP, 512-byte UStar block scanner for TAR; `RepairReport` with recovered entries, skipped byte ranges, and per-entry `RecoveryStatus` (Verified/Recovered/RawOnly) (done 2026-05-16)
- [x] Lenient-mode corruption recovery — ZIP/TAR/LZH readers .lenient(bool) + CLI --lenient flag on extract/list (planned 2026-04-20)

## Test Coverage

- detect: ~15 tests
- zip: ~40 tests (including Zip64, data descriptors, async)
- gzip: ~15 tests
- tar: ~20 tests (PAX, GNU long names)
- lzh: ~10 tests
- xz: ~10 tests
- 7z: ~5 tests
- cab: ~5 tests
- lz4/zstd/bzip2 archive: ~15 tests
- integration: ~5 tests
- Total: ~332 tests

## Code Statistics

| Module | Lines |
|--------|-------|
| zip/ | ~3,000 (header, reader, writer, types, async_zip) |
| tar/ | ~1,500 |
| lzh/ | ~1,000 |
| gzip/ | ~800 |
| xz/ | ~600 |
| sevenz/ | ~500 |
| cab/ | ~400 |
| detect.rs | ~300 |
| lz4/zstd/bzip2 | ~500 |
| lib.rs | ~200 |
| **Total** | **~7,897** |

## Format Support Matrix

| Feature | ZIP | GZIP | TAR | LZH | XZ | 7z | CAB | LZ4 | Zstd | Bzip2 | Brotli | Snappy |
|---------|-----|------|-----|-----|----|----|-----|-----|------|-------|--------|--------|
| Read | Yes | Yes | Yes | Yes | Yes | Yes | Yes | Yes | Yes | Yes | Yes | Yes |
| List entries | Yes | N/A | Yes | Yes | N/A | Yes | Yes | N/A | N/A | N/A | N/A | N/A |
| Extract | Yes | Yes | Yes | Yes | Yes | Yes | Yes | Yes | Yes | Yes | Yes | Yes |
| Create | Yes | Yes | Yes | Yes | Yes | No | No | Yes | Yes | Yes | Yes | Yes |
| Async | Yes | No | No | No | No | No | No | No | No | No | No | No |

## Known Limitations

1. No split/multi-part ZIP archive support
2. TAR sparse: read support lands hole-preserving extraction via in-memory
   materialization (GNU old-format + PAX `GNU.sparse.*`); writer-side sparse
   emission and on-disk hole-punching during extraction remain out of scope.
3. LZH level 3 headers not supported
4. No RAR format support
5. 7z and CAB are read-only (no create/write)
6. Async I/O only available for ZIP format
