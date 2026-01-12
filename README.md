# OxiArc - The Oxidized Archiver

Pure Rust implementation of archive and compression formats with core algorithms implemented from scratch.

## Overview

OxiArc is a comprehensive archive/compression library written in pure Rust. It provides support for multiple archive formats and compression algorithms, all implemented without relying on C bindings or external compression libraries.

## Features

- **Pure Rust** - No C dependencies, safe and portable
- **10 Archive Formats** - ZIP, TAR, GZIP, LZH, XZ, 7z, CAB, LZ4, Zstd, Bzip2 with auto-detection
- **8 Compression Algorithms** - DEFLATE, LZMA/LZMA2, LZH, Bzip2, LZ4, Zstd with multiple levels
- **Optimized CRC** - Hardware-independent slicing-by-8 (3-5x faster CRC-32/64)
- **Modern CLI** - Progress bars, verbose output, pattern filtering
- **Streaming API** - Memory-efficient processing of large files

## Architecture

```
+----------------------------------------------------------+
| L4: Unified API (oxiarc-cli)                             |
|     CLI with progress bars, verbose mode, filters        |
+----------------------------------------------------------+
| L3: Container (oxiarc-archive)                           |
|     ZIP, TAR, GZIP, LZH, XZ, 7z, CAB, LZ4, Zstd, Bzip2   |
+----------------------------------------------------------+
| L2: Codecs                                               |
|     oxiarc-deflate: DEFLATE (RFC 1951)                   |
|     oxiarc-lzma: LZMA/LZMA2                              |
|     oxiarc-lzhuf: LZH (lh0-lh7)                          |
|     oxiarc-bzip2: BWT + MTF + Huffman                    |
|     oxiarc-lz4: LZ4 block/frame                          |
|     oxiarc-zstd: Zstandard (FSE + Huffman)               |
+----------------------------------------------------------+
| L1: Core (oxiarc-core)                                   |
|     BitReader/Writer, RingBuffer, CRC-16/32/64 (simd-8)  |
+----------------------------------------------------------+
```

## Workspace Structure

| Crate | Description | Lines | Tests |
|-------|-------------|-------|-------|
| `oxiarc-core` | Core primitives: BitStream, RingBuffer, CRC-16/32/64 (slicing-by-8) | ~1,800 | 56 |
| `oxiarc-deflate` | DEFLATE compression (RFC 1951) with LZ77 + Huffman + Zlib | ~2,100 | 57 |
| `oxiarc-lzhuf` | LZH compression (lh0-lh7) with LZSS + Huffman | ~1,400 | 20 |
| `oxiarc-bzip2` | Bzip2 with BWT + MTF + RLE + Huffman | ~1,200 | 29 |
| `oxiarc-lz4` | LZ4 block/frame + LZ4-HC with XXHash32 | ~1,600 | 45 |
| `oxiarc-zstd` | Zstandard with FSE + Huffman + XXHash64 | ~2,100 | 43 |
| `oxiarc-lzma` | LZMA/LZMA2 with range coding + hash chains | ~2,000 | 30 |
| `oxiarc-archive` | 10 container formats (ZIP, TAR, GZIP, LZH, XZ, 7z, CAB, etc.) | ~5,600 | 91 |
| `oxiarc-cli` | CLI tool with progress bars, filters, JSON output | ~1,800 | - |
| **Total** | **Pure Rust archive/compression library** | **~19,600** | **371** |

## Quick Start

### Using the CLI

```bash
# List archive contents
oxiarc list archive.zip
oxiarc list archive.7z

# Extract with progress bar and verbose output
oxiarc extract archive.zip -o output_dir/ -v
oxiarc extract data.xz --progress

# Filter files during extraction
oxiarc extract archive.tar --include "*.txt" --exclude "test/*"

# Create archives
oxiarc create archive.zip file1.txt file2.txt
oxiarc create data.xz large_file.bin
oxiarc create backup.tar.zst folder/

# Convert between formats
oxiarc convert old.lzh new.zip
oxiarc convert data.7z data.tar

# Test archive integrity
oxiarc test archive.zip

# Show archive info
oxiarc info archive.lzh

# Detect archive format
oxiarc detect file.bin
```

### Using as a Library

```rust
use oxiarc_deflate::{deflate, inflate};
use oxiarc_archive::{ArchiveFormat, ZipReader};
use std::fs::File;

// Compress data with DEFLATE
let compressed = deflate(b"Hello, World!", 6)?;
let decompressed = inflate(&compressed)?;

// Read a ZIP archive
let file = File::open("archive.zip")?;
let mut zip = ZipReader::new(file)?;
for entry in zip.entries() {
    println!("{}: {} bytes", entry.name, entry.size);
}
```

## Compression Algorithms

### DEFLATE (RFC 1951)

The standard compression used in ZIP, GZIP, and PNG:
- LZ77 dictionary compression with 32KB sliding window
- Canonical Huffman coding
- Supports stored, fixed, and dynamic blocks
- Compression levels 0-9

### LZH (lh0-lh7)

Japanese archive format compression:
- LZSS with configurable window sizes (4KB-64KB)
- Static Huffman coding with dual trees (codes + offsets)
- Methods: lh0 (stored), lh4, lh5, lh6, lh7

### LZMA/LZMA2

Advanced compression used in 7z and XZ:
- LZ77-style dictionary compression
- Range coding for entropy encoding
- Context-dependent probability models
- 11-bit probability model (2048 states)

### Bzip2

Block-sorting compression:
- Burrows-Wheeler Transform (BWT)
- Move-To-Front (MTF) coding
- Run-Length Encoding (RLE)
- Huffman coding

### LZ4

Ultra-fast compression:
- Simple LZ77 variant
- Block and frame formats
- Minimal CPU overhead

### Zstandard

Modern fast compression:
- Finite State Entropy (FSE)
- Huffman coding
- XXHash64 checksums
- Dictionary support

## Container Formats

| Format | Read | Write | Compression Support | Notes |
|--------|------|-------|---------------------|-------|
| ZIP | ✅ | ✅ | DEFLATE, Store | Zip64 partial, data descriptors |
| TAR | ✅ | ✅ | N/A | UStar, PAX extended headers |
| GZIP | ✅ | ✅ | DEFLATE | RFC 1952, CRC-32 |
| LZH/LHA | ✅ | ✅ | lh0-lh7 | Level 0/1/2/3 headers, Shift_JIS |
| XZ | ✅ | ✅ | LZMA2 | CRC-64, block checksums |
| 7z | ✅ | ❌ | LZMA/LZMA2 | Read-only |
| CAB | ✅ | ❌ | None, MSZIP | Microsoft Cabinet, read-only |
| LZ4 | ✅ | ✅ | LZ4 | Frame format, XXHash32 |
| Zstd | ✅ | ✅ | Zstandard | Frame format, XXHash64 |
| Bzip2 | ✅ | ✅ | Bzip2 | Block CRC-32 |

## Performance

OxiArc implements several optimizations for high performance:

- **CRC-32/64 Slicing-by-8**: 3-5x faster than naive table lookup
- **Optimized DualCRC**: Single-pass computation for LZH
- **Zero-copy where possible**: Minimizes allocations
- **Streaming decompression**: Memory-efficient for large files

## Building

```bash
# Build all crates
cargo build --release

# Run all 293 tests
cargo nextest run --all-features

# Build CLI only
cargo build --release -p oxiarc-cli

# Install CLI
cargo install --path oxiarc-cli
```

## Requirements

- Rust 1.85+ (Edition 2024)
- No external C libraries or compression dependencies
- Optional: `indicatif` for progress bars (CLI only)

## License

Licensed under either of:

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or http://www.apache.org/licenses/LICENSE-2.0)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or http://opensource.org/licenses/MIT)

at your option.

## Repository

https://github.com/cool-japan/oxiarc

## Authors

COOLJAPAN OU <contact@cooljapan.tech>
