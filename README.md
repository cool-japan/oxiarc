# OxiArc - The Oxidized Archiver

[![Crates.io](https://img.shields.io/crates/v/oxiarc-cli.svg)](https://crates.io/crates/oxiarc-cli)
[![License](https://img.shields.io/badge/license-MIT%2FApache--2.0-blue.svg)](README.md#license)

Pure Rust implementation of archive and compression formats with core algorithms implemented from scratch.

## Overview

OxiArc is a comprehensive archive/compression library and CLI tool written in pure Rust. It provides support for multiple archive formats and compression algorithms, all implemented without relying on C bindings or external compression libraries. Built from the ground up with performance and safety in mind.

## Features

### Archive Formats (10 supported)
- **ZIP** - PKZIP format with DEFLATE and Store methods, Zip64 support
- **TAR** - POSIX tar with UStar and PAX extended headers
- **GZIP** - GNU zip single-file compression (RFC 1952)
- **LZH/LHA** - Japanese archive format with lh0-lh7 methods
- **XZ** - Modern LZMA2 compression format
- **7z** - 7-Zip archive format (read-only)
- **CAB** - Microsoft Cabinet format (read-only)
- **LZ4** - Fast LZ4 frame format
- **Zstandard** - Facebook's fast compression format
- **Bzip2** - Block-sorting compression

### Compression Algorithms (8 implemented)
- **DEFLATE** (RFC 1951) - LZ77 + Huffman, levels 0-9
- **LZMA/LZMA2** - Range coding with context modeling
- **LZH** - LZSS + Huffman (lh0, lh4-lh7)
- **Bzip2** - BWT + MTF + RLE + Huffman
- **LZ4** - Ultra-fast LZ77 variant with LZ4-HC
- **Zstandard** - FSE + Huffman entropy coding
- **LZW** - Lempel-Ziv-Welch for TIFF compression
- **Store** - No compression

### Core Features
- **Pure Rust** - No C/Fortran dependencies, 100% safe Rust
- **Optimized CRC** - Slicing-by-8 implementation (3-5x faster than table lookup)
- **Modern CLI** - Progress bars, verbose output, JSON support, shell completions
- **Streaming API** - Memory-efficient processing with stdin/stdout support
- **Pattern Filtering** - Include/exclude patterns with glob syntax
- **Metadata Preservation** - Timestamps, permissions, extended attributes
- **Auto-detection** - Automatic format detection from magic bytes
- **Flexible Overwrite** - Overwrite, skip, or prompt modes

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

## Installation

### Install from crates.io

```bash
cargo install oxiarc-cli
```

### Build from source

```bash
git clone https://github.com/cool-japan/oxiarc
cd oxiarc
cargo build --release
cargo install --path oxiarc-cli
```

### Add as library dependency

```toml
[dependencies]
oxiarc-archive = "0.1.0"  # For archive format support
oxiarc-deflate = "0.1.0"  # For DEFLATE compression
oxiarc-lzma = "0.1.0"     # For LZMA/LZMA2 compression
oxiarc-bzip2 = "0.1.0"    # For Bzip2 compression
oxiarc-lz4 = "0.1.0"      # For LZ4 compression
oxiarc-zstd = "0.1.0"     # For Zstandard compression
```

## Quick Start

### CLI Usage - Common Operations

```bash
# List archive contents
oxiarc list archive.zip
oxiarc list archive.7z --verbose

# Extract archives
oxiarc extract archive.zip
oxiarc extract data.tar.gz -o output/
oxiarc extract files.7z --progress

# Create archives
oxiarc create backup.zip file1.txt file2.txt folder/
oxiarc create data.tar dir1/ dir2/
oxiarc create compressed.xz large_file.bin

# Test integrity
oxiarc test archive.zip
oxiarc test data.lzh --verbose

# Show detailed information
oxiarc info archive.7z
oxiarc info data.cab

# Detect format
oxiarc detect unknown_file.bin

# Convert between formats
oxiarc convert old.lzh new.zip
oxiarc convert data.7z backup.tar
```

### Library Usage - Basic Examples

```rust
use oxiarc_deflate::{deflate, inflate};
use oxiarc_archive::ZipReader;
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

## Format Support Matrix

| Format | Read | Write | Compression | Checksums | Notes |
|--------|------|-------|-------------|-----------|-------|
| **ZIP** | ✅ | ✅ | DEFLATE, Store | CRC-32 | Zip64 support, data descriptors |
| **TAR** | ✅ | ✅ | N/A (container only) | None | UStar, PAX, GNU long names |
| **GZIP** | ✅ | ✅ | DEFLATE | CRC-32 | RFC 1952 compliant |
| **LZH** | ✅ | ✅ | lh0-lh7 | CRC-16 | Shift_JIS support, all header levels |
| **XZ** | ✅ | ✅ | LZMA2 | CRC-64 | Block checksums |
| **7z** | ✅ | ❌ | LZMA/LZMA2 | CRC-32 | Read-only, partial support |
| **CAB** | ✅ | ❌ | None, MSZIP | CRC-32 | Microsoft Cabinet, read-only |
| **LZ4** | ✅ | ✅ | LZ4, LZ4-HC | XXHash32 | Frame format, block/content checksums |
| **Zstd** | ✅ | ✅ | Zstandard | XXHash64 | Frame format with FSE+Huffman |
| **Bzip2** | ✅ | ✅ | BWT + Huffman | CRC-32 | Block-sorting compression |

## Performance

### Benchmark Results

Real-world performance measured on various data types:

#### LZ77 (DEFLATE) Compression Throughput
| Level | Uniform Data | Text Data | Binary Data |
|-------|-------------|-----------|-------------|
| Level 1 (Fast) | 400 MB/s | 85 MB/s | 48 MB/s |
| Level 5 (Normal) | 275 MB/s | 42 MB/s | 13 MB/s |
| Level 9 (Best) | 253 MB/s | 15 MB/s | 0.3 MB/s |

#### BWT (Bzip2) Throughput
| Operation | Speed Range |
|-----------|-------------|
| Forward Transform | 2-11 MB/s |
| Inverse Transform | 60-320 MB/s |

#### CRC Performance
| Algorithm | Naive | Slicing-by-8 | Speedup |
|-----------|-------|--------------|---------|
| CRC-32 | ~150 MB/s | ~500 MB/s | 3.3x |
| CRC-64 | ~100 MB/s | ~450 MB/s | 4.5x |

### Optimizations

OxiArc implements several performance optimizations:

- **CRC Slicing-by-8**: Hardware-independent 3-5x speedup over table lookup
- **Optimized Hash Chains**: Improved LZ77 pattern matching with multiplication-based hashing
- **Lazy Matching**: Better compression ratios in DEFLATE with minimal speed impact
- **BWT Key-Based Sorting**: 4-byte prefix keys for faster block sorting
- **Zero-Copy Streaming**: Minimizes allocations and memory copies
- **Early Rejection**: Fast-path optimizations for match finding

## Examples

### Creating Archives

#### ZIP Archives
```bash
# Create a ZIP archive from files and directories
oxiarc create backup.zip file1.txt file2.pdf documents/

# Create with compression level (store, fast, normal, best)
oxiarc create -l best archive.zip src/ tests/

# Verbose output
oxiarc create -v data.zip folder/
```

#### TAR Archives
```bash
# Create a TAR archive
oxiarc create backup.tar project/

# Combine with compression (tar.gz, tar.xz, tar.bz2, tar.zst)
gzip backup.tar    # or use GZIP directly
oxiarc create backup.tar.gz folder/  # Auto-detects .gz extension
```

#### Single-File Compression
```bash
# GZIP compression
oxiarc create data.txt.gz large_file.txt

# XZ (LZMA2) compression
oxiarc create database.sql.xz database.sql
oxiarc create -l best archive.xz bigdata.bin

# LZ4 (fast compression)
oxiarc create temp.lz4 file.bin
oxiarc create -l fast logs.lz4 access.log

# Zstandard compression
oxiarc create data.zst large_dataset.csv

# Bzip2 compression
oxiarc create text.bz2 document.txt
```

#### LZH Archives
```bash
# Create LZH archive (Japanese format)
oxiarc create archive.lzh file1.txt file2.txt folder/
```

### Extracting Archives

#### Basic Extraction
```bash
# Extract to current directory
oxiarc extract archive.zip
oxiarc extract data.tar.gz
oxiarc extract files.7z

# Extract to specific directory
oxiarc extract archive.zip -o extracted/
oxiarc extract backup.tar.xz -o /tmp/restore/

# Extract with progress bar
oxiarc extract large_archive.zip --progress

# Verbose output (show each file being extracted)
oxiarc extract data.lzh -v
```

#### Selective Extraction
```bash
# Extract specific files
oxiarc extract archive.zip file1.txt readme.md

# Extract only files matching patterns (glob syntax)
oxiarc extract backup.zip --include "*.txt"
oxiarc extract data.tar --include "src/**/*.rs"

# Exclude files from extraction
oxiarc extract archive.zip --exclude "test/*" --exclude "*.tmp"

# Combine include and exclude
oxiarc extract backup.zip --include "docs/**" --exclude "*.draft"
```

#### Metadata Preservation
```bash
# Preserve modification timestamps
oxiarc extract archive.zip -t

# Preserve Unix file permissions
oxiarc extract backup.tar --preserve-permissions

# Preserve all metadata (timestamps + permissions)
oxiarc extract data.tar.gz -p
```

#### Overwrite Control
```bash
# Always overwrite (default)
oxiarc extract archive.zip --overwrite

# Skip existing files without prompting
oxiarc extract backup.zip --skip-existing

# Prompt before overwriting each file
oxiarc extract data.zip --prompt
```

### Streaming with stdin/stdout

#### Extract from stdin
```bash
# Decompress from stdin to stdout
cat data.gz | oxiarc extract - -o - > output.txt
curl https://example.com/data.xz | oxiarc extract - --format xz > data.txt

# Extract specific format from stdin
oxiarc extract - --format gzip < compressed.gz > original.txt
```

#### Create to stdout
```bash
# Compress to stdout
oxiarc create - --format gzip < input.txt > output.gz
cat large_file.bin | oxiarc create - --format xz > compressed.xz

# Pipe compression
find . -name "*.log" | tar -cf - -T - | oxiarc create - --format zst > logs.tar.zst
```

### Listing Contents

#### Basic Listing
```bash
# List files in archive
oxiarc list archive.zip
oxiarc list backup.tar.gz
oxiarc list data.7z

# Verbose listing (show size, date, permissions)
oxiarc list archive.zip -v

# JSON output (machine-readable)
oxiarc list data.lzh --json
```

#### Filtered Listing
```bash
# List only matching files
oxiarc list backup.zip --include "*.txt"
oxiarc list archive.tar --include "src/**/*.rs"

# Exclude patterns
oxiarc list data.zip --exclude "test/*"
```

### Testing Integrity

```bash
# Test archive integrity
oxiarc test archive.zip
oxiarc test backup.tar.gz
oxiarc test data.lzh

# Verbose testing (show each file being tested)
oxiarc test archive.7z -v
```

### Getting Archive Information

```bash
# Show archive metadata
oxiarc info archive.zip
oxiarc info data.7z
oxiarc info backup.lzh

# Example output:
# Format: ZIP
# Files: 42
# Compressed size: 1.2 MB
# Uncompressed size: 5.4 MB
# Compression ratio: 77.8%
```

### Format Detection

```bash
# Detect archive format
oxiarc detect unknown_file.bin
oxiarc detect downloaded_archive

# Useful for files without extensions
oxiarc detect mystery_file
```

### Converting Between Formats

```bash
# Convert archive formats
oxiarc convert old.lzh new.zip
oxiarc convert data.7z backup.tar
oxiarc convert legacy.cab modern.zip

# Convert with compression level
oxiarc convert source.zip dest.tar -l best

# Verbose conversion
oxiarc convert old.lzh new.zip -v
```

### Using Filters and Patterns

Pattern syntax supports glob-style wildcards:
- `*` matches any characters except `/`
- `**` matches any characters including `/` (recursive)
- `?` matches a single character
- `[abc]` matches one character from the set

```bash
# Include only specific file types
oxiarc extract archive.zip --include "*.txt" --include "*.md"

# Recursive pattern matching
oxiarc list backup.tar --include "src/**/*.rs"
oxiarc extract data.zip --include "docs/**/*.pdf"

# Complex filtering
oxiarc extract backup.zip \
  --include "src/**" \
  --exclude "src/test/**" \
  --exclude "**/*.tmp"
```

## API Usage

### Basic Compression/Decompression

```rust
use oxiarc_deflate::{deflate, inflate};
use oxiarc_core::error::Result;

fn main() -> Result<()> {
    // DEFLATE compression
    let data = b"Hello, World! This is a test.";
    let compressed = deflate(data, 6)?;  // Level 6 compression
    let decompressed = inflate(&compressed)?;
    assert_eq!(data, &decompressed[..]);
    Ok(())
}
```

### Working with ZIP Archives

```rust
use oxiarc_archive::ZipReader;
use std::fs::File;
use std::io::Read;

fn read_zip() -> oxiarc_core::error::Result<()> {
    // Open ZIP archive
    let file = File::open("archive.zip")?;
    let mut zip = ZipReader::new(file)?;

    // List entries
    for entry in zip.entries() {
        println!("{}: {} bytes (compressed: {})",
            entry.name,
            entry.size,
            entry.compressed_size
        );
    }

    // Extract specific file
    let mut data = Vec::new();
    zip.extract_by_name("readme.txt", &mut data)?;
    println!("Content: {}", String::from_utf8_lossy(&data));

    Ok(())
}
```

### Creating ZIP Archives

```rust
use oxiarc_archive::zip::{ZipWriter, ZipCompressionLevel};
use std::fs::File;

fn create_zip() -> oxiarc_core::error::Result<()> {
    let file = File::create("output.zip")?;
    let mut zip = ZipWriter::new(file);

    // Add file with compression
    zip.add_file(
        "hello.txt",
        b"Hello, World!",
        ZipCompressionLevel::Normal
    )?;

    // Add directory
    zip.add_directory("docs/")?;

    // Finalize archive
    zip.finish()?;
    Ok(())
}
```

### LZMA Compression

```rust
use oxiarc_lzma::{compress, decompress, LzmaLevel};

fn lzma_example() -> oxiarc_core::error::Result<()> {
    let data = b"This is test data for LZMA compression";

    // Compress with LZMA
    let compressed = compress(data, LzmaLevel::DEFAULT)?;

    // Decompress
    let decompressed = decompress(&compressed)?;
    assert_eq!(data, &decompressed[..]);

    Ok(())
}
```

### Bzip2 Compression

```rust
use oxiarc_bzip2::{compress, decompress, CompressionLevel};

fn bzip2_example() -> oxiarc_core::error::Result<()> {
    let data = b"Data to compress with Bzip2";

    // Compress (levels 1-9)
    let compressed = compress(data, CompressionLevel::Best)?;

    // Decompress
    let decompressed = decompress(&compressed)?;
    assert_eq!(data, &decompressed[..]);

    Ok(())
}
```

### LZ4 Fast Compression

```rust
use oxiarc_lz4::{compress_frame, decompress_frame};

fn lz4_example() -> oxiarc_core::error::Result<()> {
    let data = b"Fast compression with LZ4";

    // Compress (very fast)
    let compressed = compress_frame(data)?;

    // Decompress
    let decompressed = decompress_frame(&compressed)?;
    assert_eq!(data, &decompressed[..]);

    Ok(())
}
```

### Format Detection

```rust
use oxiarc_archive::ArchiveFormat;
use std::fs::File;

fn detect_format() -> oxiarc_core::error::Result<()> {
    let mut file = File::open("unknown.bin")?;
    let (format, magic) = ArchiveFormat::detect(&mut file)?;

    println!("Detected format: {}", format);
    println!("Magic bytes: {:02X?}", magic);

    if format.is_archive() {
        println!("This is a multi-file archive");
    } else if format.is_compression_only() {
        println!("This is single-file compression");
    }

    Ok(())
}
```

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

## Contributing

We welcome contributions to OxiArc! Please follow these guidelines:

### COOLJAPAN Policies

OxiArc is part of the COOLJAPAN ecosystem and follows strict development policies:

#### 1. Pure Rust Policy
- **No C/Fortran dependencies** - All code must be pure Rust
- If C/Fortran bindings are absolutely necessary, they must be feature-gated
- Default features must be 100% pure Rust

#### 2. No Warnings Policy
- Code must compile with zero warnings
- Run `cargo clippy` and fix all warnings before submitting
- Use `cargo nextest run --all-features` to verify

#### 3. No Unwrap Policy
- Avoid using `.unwrap()`, `.expect()`, or panicking code in production
- Use proper error handling with `Result<T, E>`
- Provide meaningful error messages

#### 4. Workspace Policy
- Use workspace-level dependency management
- Set `*.workspace = true` in crate `Cargo.toml` files
- No version specifications in individual crates (except keywords/categories)

#### 5. Latest Crates Policy
- Always use the latest stable versions from crates.io
- Keep dependencies up to date

#### 6. Refactoring Policy
- Keep individual source files under 2000 lines
- Use `splitrs` tool for refactoring large files
- Check with `rslines 50` to find refactoring targets

### Development Workflow

1. **Fork and Clone**
   ```bash
   git clone https://github.com/YOUR_USERNAME/oxiarc
   cd oxiarc
   ```

2. **Create a Branch**
   ```bash
   git checkout -b feature/your-feature-name
   ```

3. **Make Changes**
   - Follow Rust naming conventions (snake_case for variables/functions)
   - Add tests for new functionality
   - Update documentation and examples
   - Run tests: `cargo nextest run --all-features`
   - Check code: `cargo clippy --all-features`

4. **Test Thoroughly**
   ```bash
   # Run all tests
   cargo nextest run --all-features

   # Check for warnings
   cargo clippy --all-features

   # Check formatting
   cargo fmt --check

   # Run benchmarks (if applicable)
   cargo bench
   ```

5. **Commit Changes**
   - Write clear, descriptive commit messages
   - Reference issue numbers if applicable
   - **DO NOT commit unless explicitly ready**
   - **NEVER use `cargo publish` without permission**

6. **Submit Pull Request**
   - Describe your changes clearly
   - Reference related issues
   - Ensure CI passes
   - Wait for review from maintainers

### Code Style

- Follow standard Rust conventions
- Use `rustfmt` for formatting: `cargo fmt`
- Document public APIs with doc comments (`///`)
- Include examples in documentation where helpful
- Prefer explicit over implicit
- Think deeply about implementations (ultrathink mode)

### Testing

- Write unit tests for new functionality
- Add integration tests for complex features
- Include edge case testing
- Use temporary directories for file operations: `std::env::temp_dir()`
- Aim for high test coverage

### Documentation

- Update README.md for user-facing changes
- Update TODO.md for development progress
- Add API documentation for public items
- Include usage examples
- Keep documentation accurate and up-to-date

### Benchmark Contributions

- Use `criterion` for benchmarks
- Place benchmarks in `benches/` directory
- Document benchmark methodology
- Include various data patterns (uniform, random, text, binary)

### Issue Reporting

When reporting issues, please include:
- Rust version (`rustc --version`)
- OxiArc version
- Operating system and architecture
- Minimal reproduction example
- Expected vs actual behavior
- Any relevant error messages

### Feature Requests

- Describe the use case clearly
- Explain why the feature would be useful
- Provide examples of how it would be used
- Consider implementation complexity

### Architecture Contributions

When adding new formats or algorithms:
- Follow the existing layered architecture
- Core algorithms go in appropriate codec crates
- Format support goes in `oxiarc-archive`
- CLI features go in `oxiarc-cli`
- Share common code through `oxiarc-core`

### Community

- Be respectful and constructive
- Help others in issues and discussions
- Share knowledge and expertise
- Follow the Rust Code of Conduct

## License

Licensed under either of:

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or http://www.apache.org/licenses/LICENSE-2.0)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or http://opensource.org/licenses/MIT)

at your option.

## Repository

https://github.com/cool-japan/oxiarc

## Authors

COOLJAPAN OU <contact@cooljapan.tech>
