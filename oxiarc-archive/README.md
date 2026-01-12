# oxiarc-archive

Container format support for OxiArc - parsing and extraction of archive formats.

## Overview

This crate handles the container/wrapper aspects of archive formats:
- Header parsing and validation
- Entry enumeration
- File extraction
- Format auto-detection

The actual compression/decompression is delegated to codec crates (`oxiarc-deflate`, `oxiarc-lzhuf`, `oxiarc-lzma`).

## Supported Formats

| Format | Extension | Read | Write | Notes |
|--------|-----------|------|-------|-------|
| ZIP | .zip | Yes | No | DEFLATE, Stored |
| GZIP | .gz | Yes | No | RFC 1952 |
| TAR | .tar | Yes | No | UStar format |
| LZH | .lzh, .lha | Yes | No | Level 0/1/2 headers |

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
| `zip` | ZIP archive handling |
| `gzip` | GZIP file handling |
| `tar` | TAR archive handling |
| `lzh` | LZH archive handling |

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

## License

MIT OR Apache-2.0
