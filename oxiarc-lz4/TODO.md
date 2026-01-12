# oxiarc-lz4 - Development Status

## Completed Features

### Block Format
- [x] Block compression with hash chain matching
- [x] Block decompression with overlapping match support
- [x] Variable-length literal encoding (15 + 255...)
- [x] Variable-length match encoding (15 + 255...)
- [x] 4-byte minimum match length
- [x] 16-bit match offset (up to 65KB)

### Frame Format
- [x] Simple framing with magic number and size
- [x] Magic number: 0x184D2204
- [x] Stored original size for decompression

### Traits
- [x] Compressor trait implementation
- [x] Decompressor trait implementation
- [x] FlushMode support

## Completed Features (New)

### Official LZ4 Frame Format (RFC Compatible)
- [x] Frame magic number and version validation
- [x] Frame descriptor (FLG, BD bytes)
- [x] Block independence flag
- [x] Configurable block maximum sizes (64KB, 256KB, 1MB, 4MB)
- [x] Content size in header (optional)
- [x] Header checksum (XXH32)
- [x] Block checksum (optional XXH32)
- [x] Content checksum (XXH32)
- [x] End marker

### XXHash32 Implementation
- [x] One-shot xxhash32() function
- [x] Seeded xxhash32_with_seed()
- [x] Incremental XxHash32 hasher
- [x] All prime constants and avalanche functions

### LZ4-HC High Compression Mode
- [x] Compression levels 1-12
- [x] Larger hash table (64K entries)
- [x] Chain table for multiple matches
- [x] Configurable match attempt limits
- [x] Optimal parsing for level 12 (dynamic programming)

## Future Enhancements

### Performance
- [ ] SIMD-accelerated matching
- [ ] Parallel compression
- [ ] Streaming with bounded memory
- [ ] Dictionary support

## Test Coverage

- block: 7 tests
- frame: 15 tests (official format, checksums, options)
- xxhash: 8 tests (one-shot, incremental, seeded)
- hc: 9 tests (levels, compression ratios, optimal)
- lib: 6 tests
- Total: 44 tests

## Code Statistics

| File | Lines |
|------|-------|
| block.rs | ~402 |
| frame.rs | ~786 |
| xxhash.rs | ~230 |
| hc.rs | ~450 |
| lib.rs | ~103 |
| **Total** | **~1,350** |

## LZ4 Format Summary

### Token Format
```
+-------+-------+
| LLLL  | MMMM  |  <- Token byte
+-------+-------+
  4 bit   4 bit

LLLL: Literal length (0-14, or 15 for extended)
MMMM: Match length - 4 (0-14, or 15 for extended)
```

### Sequence Layout
```
[Token] [Lit Ext*] [Literals] [Offset:2] [Match Ext*]
```

### Extended Length Encoding
- If base length = 15, read additional bytes
- Add 255 for each 0xFF byte, stop at first non-0xFF

## Known Limitations

1. Single-threaded only
2. No streaming API yet
3. No dictionary support yet
