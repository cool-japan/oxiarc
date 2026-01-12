# oxiarc-archive - Development Status

## Completed Features

### Format Detection (207 lines)
- [x] `ArchiveFormat` enum
- [x] Magic byte detection for ZIP, GZIP, 7z, XZ, BZip2, Zstd, LZH, TAR
- [x] `detect()` function from reader
- [x] `from_magic()` for direct byte analysis
- [x] Extension and MIME type mappings
- [x] Archive vs. compression-only classification

### ZIP (296 lines)
- [x] Local file header parsing
- [x] Central directory parsing
- [x] End of central directory
- [x] `ZipReader` with entry enumeration
- [x] File extraction with DEFLATE
- [x] File extraction with Stored method
- [x] CRC-32 verification
- [x] UTF-8 filename support (flag bit 11)

### GZIP (223 lines)
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

### TAR (224 lines)
- [x] UStar header parsing
- [x] 512-byte block structure
- [x] Header fields: name, mode, uid, gid, size, mtime, chksum, typeflag, linkname
- [x] UStar magic detection
- [x] Prefix field for long names
- [x] `TarReader` with entry enumeration
- [x] File type detection (regular, directory, symlink, etc.)

### LZH (338 lines)
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

## Future Enhancements

### ZIP Write Support
- [ ] Local file header writing
- [ ] Central directory writing
- [ ] End of central directory
- [ ] DEFLATE compression
- [ ] CRC-32 computation during write
- [ ] Zip64 for large files

### ZIP Improvements
- [ ] Zip64 read support (>4GB files)
- [ ] Data descriptor support
- [ ] ZIP encryption (traditional)
- [ ] ZIP encryption (AES)
- [ ] Split/multi-part archives
- [ ] Archive comments

### GZIP Write Support
- [ ] Header writing
- [ ] DEFLATE compression
- [ ] Trailer writing

### TAR Improvements
- [ ] File extraction
- [ ] PAX extended headers
- [ ] GNU long names
- [ ] Sparse files
- [ ] Archive creation

### LZH Improvements
- [ ] Level 3 headers
- [ ] File extraction with all methods
- [ ] Archive creation
- [ ] More extension headers

### New Formats
- [ ] 7-Zip (.7z) read support
- [ ] XZ (.xz) read support
- [ ] RAR read support (licensing?)
- [ ] CAB (.cab) read support
- [ ] ISO 9660 read support

### General
- [ ] Streaming extraction (without buffering entire file)
- [ ] Async I/O support
- [ ] Progress callbacks
- [ ] Memory-mapped files
- [ ] Archive repair/recovery

## Test Coverage

- detect: 7 tests
- zip: 8 tests
- gzip: 4 tests
- tar: 3 tests
- lzh: 3 tests
- Total: 25 tests

## Code Statistics

| File | Lines |
|------|-------|
| lzh/mod.rs | 338 |
| zip/header.rs | 280 |
| tar/mod.rs | 224 |
| gzip/header.rs | 207 |
| detect.rs | 207 |
| lib.rs | 45 |
| zip/mod.rs | 16 |
| gzip/mod.rs | 16 |
| **Total** | **~1,333** |

## Format Support Matrix

| Feature | ZIP | GZIP | TAR | LZH |
|---------|-----|------|-----|-----|
| Read headers | Yes | Yes | Yes | Yes |
| List entries | Yes | N/A | Yes | Yes |
| Extract files | Yes | Yes | No | No |
| Create archive | No | No | No | No |
| Multi-file | Yes | No | Yes | Yes |

## Known Limitations

1. No write support for any format yet
2. TAR extraction not implemented
3. LZH extraction needs per-method codec integration
4. No support for encrypted archives
5. No Zip64 large file support
