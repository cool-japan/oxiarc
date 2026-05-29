
# oxiarc-archive [Stable]

Container format support for OxiArc - parsing and extraction of archive formats.

[![Crates.io](https://img.shields.io/crates/v/oxiarc-archive.svg)](https://crates.io/crates/oxiarc-archive)
![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)
![Status](https://img.shields.io/badge/status-Stable-brightgreen)

**Version: 0.3.1 (2026-05-30) | 332 tests passing**


## Features

- Header parsing and validation
- Entry enumeration
- File extraction
- Format auto-detection
- Async ZIP entry reading (async-io feature)
- Brotli and Snappy compression support
- ISO 9660 read support with PVD and Joliet UCS-2 filename handling (new in 0.2.8)
- Raw-preserve append: `ZipWriter::add_file_raw`, `LzhReader::read_raw_method_data`, `LzhWriter::add_file_raw` (new in 0.2.8)
- Archive repair/recovery: `repair_zip`, `repair_tar` functions; `ZipRepair`, `TarRepair`, `RepairReport` structs for recovering truncated or corrupt archives (new in 0.3.0)

All features are implemented and tested. API is stable.

This crate handles the container/wrapper aspects of archive formats:
- Header parsing and validation
- Entry enumeration
- File extraction
- Format auto-detection
- Async ZIP entry reading (new in 0.2.4, via `async-io` feature)
- Brotli compression support (via `oxiarc-brotli`)
- Snappy compression support (via `oxiarc-snappy`)
- ISO 9660 read support with PVD and Joliet UCS-2 filename handling (new in 0.2.8, via `IsoReader`)
- Raw-preserve append for ZIP and LZH entries (new in 0.2.8)
- Archive repair/recovery: `repair_zip`, `repair_tar`, `ZipRepair`, `TarRepair`, `RepairReport` for recovering truncated or corrupt archives (new in 0.3.0)

The actual compression/decompression is delegated to codec crates (`oxiarc-deflate`, `oxiarc-lzhuf`, `oxiarc-lzma`, `oxiarc-brotli`, `oxiarc-snappy`).

## Supported Formats

| Format | Extension | Read | Write | Notes |
|--------|-----------|------|-------|-------|
| ZIP | .zip | Yes | No | DEFLATE, Stored; async read via `async-io`; raw-preserve append via `add_file_raw` |
| GZIP | .gz | Yes | No | RFC 1952 |
| TAR | .tar | Yes | No | UStar format |
| LZH | .lzh, .lha | Yes | No | Level 0/1/2 headers; raw-preserve append via `add_file_raw` / `read_raw_method_data` |
| ISO 9660 | .iso | Yes | No | PVD + Joliet UCS-2 filenames; magic detection at LBA 16 (new in 0.2.8) |
| Brotli | .br | Yes | No | RFC 7932, via `oxiarc-brotli` |
| Snappy | .sz | Yes | No | Block and framed formats, via `oxiarc-snappy` |

## Quick Start

```rust
use oxiarc_archive::{ArchiveFormat, ZipReader, GzipReader};
use std::fs::File;
use std::io::{BufReader, Seek, SeekFrom};

// Auto-detect format
let mut file = BufReader::new(File::open("archive.zip")?);
let (format, _) = ArchiveFormat::detect(&mut file)?;
println!("Format: {}", format);  // "ZIP"

// Read ZIP archive
file.seek(SeekFrom::Start(0))?;
let mut zip = ZipReader::new(file)?;
for entry in zip.entries() {
    println!("{}: {} bytes", entry.name, entry.size);
}

// Extract a file
let data = zip.extract(&zip.entries()[0])?;
```

## Async ZIP Support (New in 0.2.4)

The `async_zip` module (enabled via `async-io` feature) provides non-blocking ZIP entry reading using Tokio's async I/O primitives:

```rust
use oxiarc_archive::async_zip::{read_zip_entry_async, read_zip_entry_from_reader_async};
use oxiarc_archive::zip::ZipReader;
use std::io::Cursor;

// High-level: read entry from ZipReader asynchronously
let zip_bytes: Vec<u8> = /* ... */;
let cursor = Cursor::new(zip_bytes);
let mut reader = ZipReader::new(cursor)?;
let entries = reader.entries().to_vec();

let data = read_zip_entry_from_reader_async(&mut reader, &entries[0]).await?;

// Low-level: read from any AsyncRead + AsyncSeek
let mut async_reader = tokio::io::BufReader::new(Cursor::new(zip_bytes));
let data = read_zip_entry_async(&mut async_reader, &entries[0]).await?;
```

Supported async compression methods: `Stored`, `Deflate`.

## Features (Cargo)

| Feature | Default | Description |
|---------|---------|-------------|
| `mmap` | no | Memory-mapped file support for efficient large file reading (via `memmap2`) |
| `async-io` | no | Async ZIP entry reading via Tokio (`async_zip` module) |

```toml
[dependencies]
# Default (no optional features)
oxiarc-archive = "0.3.1"

# With memory-mapped I/O
oxiarc-archive = { version = "0.3.1", features = ["mmap"] }

# With async ZIP support
oxiarc-archive = { version = "0.3.1", features = ["async-io"] }

# With all features
oxiarc-archive = { version = "0.3.1", features = ["mmap", "async-io"] }
```

## Format Detection

Automatic format detection based on magic bytes:

```rust
use oxiarc_archive::ArchiveFormat;
use std::fs::File;

let mut file = File::open("unknown_file")?;
let (format, magic_bytes) = ArchiveFormat::detect(&mut file)?;

match format {
    ArchiveFormat::Zip => println!("ZIP archive"),
    ArchiveFormat::Gzip => println!("GZIP compressed"),
    ArchiveFormat::Tar => println!("TAR archive"),
    ArchiveFormat::Lzh => println!("LZH archive"),
    ArchiveFormat::Iso => println!("ISO 9660 image"),
    ArchiveFormat::SevenZip => println!("7-Zip (not supported yet)"),
    ArchiveFormat::Xz => println!("XZ compressed (not supported yet)"),
    ArchiveFormat::Unknown => println!("Unknown format"),
    _ => {}
}

// Format properties
println!("Extension: .{}", format.extension());
println!("MIME type: {}", format.mime_type());
println!("Is archive: {}", format.is_archive());
println!("Is compression only: {}", format.is_compression_only());
```

## ZIP Archives

ZIP format support with DEFLATE and Stored methods:

```rust
use oxiarc_archive::{ZipReader, LocalFileHeader};
use std::io::Cursor;

let data = include_bytes!("test.zip");
let mut zip = ZipReader::new(Cursor::new(data))?;

// List entries
for entry in zip.entries() {
    println!("{}:", entry.name);
    println!("  Size: {} -> {} bytes", entry.size, entry.compressed_size);
    println!("  Method: {}", entry.method.name());
    println!("  CRC-32: {:08X}", entry.crc32.unwrap_or(0));
}

// Extract specific file
if let Some(entry) = zip.entry_by_name("readme.txt")? {
    let content = zip.extract(&entry)?;
    println!("{}", String::from_utf8_lossy(&content));
}
```

## GZIP Files

GZIP single-file compression (RFC 1952):

```rust
use oxiarc_archive::GzipReader;
use std::fs::File;

let file = File::open("data.gz")?;
let mut gzip = GzipReader::new(file)?;

// Read header
let header = gzip.header();
if let Some(name) = &header.filename {
    println!("Original filename: {}", name);
}
println!("Modification time: {}", header.mtime);

// Decompress
let data = gzip.decompress()?;
println!("Decompressed {} bytes", data.len());
```

## TAR Archives

TAR (Tape Archive) format with UStar extensions:

```rust
use oxiarc_archive::TarReader;
use std::fs::File;

let file = File::open("archive.tar")?;
let tar = TarReader::new(file)?;

for entry in tar.entries() {
    println!("{}:", entry.name);
    println!("  Size: {} bytes", entry.size);
    println!("  Type: {:?}", entry.entry_type);
    println!("  Mode: {:o}", entry.attributes.mode());
}
```

## LZH Archives

LZH/LHA archive format with Shift_JIS filename support:

```rust
use oxiarc_archive::LzhReader;
use std::fs::File;

let file = File::open("archive.lzh")?;
let lzh = LzhReader::new(file)?;

for entry in lzh.entries() {
    println!("{}:", entry.name);
    println!("  Method: {}", entry.method.name());
    println!("  Size: {} -> {}", entry.size, entry.compressed_size);
}
```

### LZH Header Levels

| Level | Description |
|-------|-------------|
| 0 | Basic DOS format (obsolete) |
| 1 | Extended with extension headers |
| 2 | Modern with 2-byte header size |

## Modules

| Module | Description |
|--------|-------------|
| `detect` | Format auto-detection |
| `zip` | ZIP archive handling (including `ZipWriter::add_file_raw`) |
| `gzip` | GZIP file handling |
| `tar` | TAR archive handling |
| `lzh` | LZH archive handling (including `LzhWriter::add_file_raw`, `LzhReader::read_raw_method_data`) |
| `iso` | ISO 9660 image reading via `IsoReader` (new in 0.2.8) |
| `repair` | Archive repair/recovery: `repair_zip`, `repair_tar`, `ZipRepair`, `TarRepair`, `RepairReport` (new in 0.3.0) |
| `async_zip` | Async ZIP entry reading (requires `async-io` feature) |

## Security

Path traversal protection is built-in:

```rust
let entry = zip.entries()[0];

// Sanitized name removes:
// - Leading slashes
// - Parent directory references (..)
// - Null bytes
// - Backslashes (converted to forward slashes)
let safe_path = entry.sanitized_name();
```

## Magic Bytes Reference

| Format | Magic Bytes | Offset |
|--------|-------------|--------|
| ZIP | `50 4B 03 04` | 0 |
| GZIP | `1F 8B` | 0 |
| 7-Zip | `37 7A BC AF 27 1C` | 0 |
| XZ | `FD 37 7A 58 5A 00` | 0 |
| BZip2 | `42 5A 68` | 0 |
| Zstd | `28 B5 2F FD` | 0 |
| LZH | `-lh?-` or `-lz?-` | 2 |
| TAR | `ustar` | 257 |
| ISO 9660 | `CD001` (Primary Volume Descriptor) | LBA 16 (byte 32768) |

## License

Apache-2.0
