# oxiarc-archive - Development Status

## Completed Features

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

### Brotli / Snappy Archive (NEW in v0.2.5)
- [x] BrotliReader for decompression (.br/.brotli)
- [x] BrotliWriter for compression (.br/.brotli)
- [x] SnappyReader for decompression (.sz/.snappy)
- [x] SnappyWriter for compression (.sz/.snappy)
- [x] Format detection for Brotli and Snappy

## Future Enhancements

### ZIP Improvements
- [ ] ZIP encryption (traditional)
- [ ] ZIP encryption (AES)
- [ ] Split/multi-part archives

### TAR Improvements
- [ ] Sparse files

### LZH Improvements
- [ ] Level 3 headers
- [ ] More extension headers

### New Formats
- [ ] RAR read support (licensing?)
- [ ] ISO 9660 read support

### General
- [ ] Streaming extraction (without buffering entire file)
- [ ] Async I/O for more formats (currently ZIP only)
- [ ] Progress callbacks
- [ ] Memory-mapped files
- [ ] Archive repair/recovery

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
- Total: ~172 tests

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

1. No support for encrypted ZIP archives (traditional or AES)
2. No split/multi-part ZIP archive support
3. TAR sparse files not supported
4. LZH level 3 headers not supported
5. No RAR format support
6. 7z and CAB are read-only (no create/write)
7. Async I/O only available for ZIP format
