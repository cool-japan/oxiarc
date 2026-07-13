# OxiArc - The Oxidized Archiver

[![Crates.io](https://img.shields.io/crates/v/oxiarc-cli.svg)](https://crates.io/crates/oxiarc-cli)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](README.md#license)

Pure Rust implementation of archive and compression formats with core algorithms implemented from scratch.

## Overview

OxiArc is a comprehensive archive/compression library and CLI tool written in pure Rust. It provides support for multiple archive formats and compression algorithms, all implemented without relying on C bindings or external compression libraries. Built from the ground up with performance and safety in mind.

## Features

### Archive Formats (13 supported)
- **ZIP** - PKZIP format with DEFLATE and Store methods, Zip64 support
- **TAR** - POSIX tar with UStar and PAX extended headers
- **GZIP** - GNU zip single-file compression (RFC 1952)
- **LZH/LHA** - Japanese archive format with lh0, lh1, lh4, lh5, lh6, lh7, lhd methods
- **XZ** - Modern LZMA2 compression format
- **7z** - 7-Zip archive format (read-only)
- **CAB** - Microsoft Cabinet format (read-only)
- **LZ4** - Fast LZ4 frame format
- **Zstandard** - Facebook's fast compression format
- **Bzip2** - Block-sorting compression
- **Brotli** - Brotli compression (RFC 7932)
- **Snappy** - Google's fast compression format
- **ISO 9660** - CD/DVD disc image format (read-only)

### Compression Algorithms (11 implemented)
- **DEFLATE** (RFC 1951) - LZ77 + Huffman, levels 0-9, async deflate support
- **LZMA/LZMA2** - Range coding with context modeling
- **LZH** - LZSS + Huffman (lh0, lh4, lh5, lh6, lh7) plus lh1 (LZHUF adaptive Huffman) and lhd directory entries; lh2/lh3 are not implemented
- **Bzip2** - BWT + MTF + RLE + Huffman
- **LZ4** - Ultra-fast LZ77 variant with LZ4-HC
- **Zstandard** (RFC 8878) - Full decoder (FSE + 1/4-stream Huffman literals); encoder emits Huffman-compressed literals with predefined/RLE FSE sequence coding
- **LZW** - Lempel-Ziv-Welch for TIFF and GIF compression (MSB/LSB bitstream)
- **Brotli** (RFC 7932) - LZ77 + context-dependent Huffman, complete Appendix A static dictionary (122,784 bytes, all 121 transforms), quality 0-11
- **Snappy** - Ultra-fast LZ77 variant with block and framed formats
- **Store** - No compression
- **AEC/SZIP** (CCSDS-121.0-B-2) - Adaptive entropy coding for scientific datasets

### Core Features
- **Pure Rust** - No C/Fortran dependencies, 100% safe Rust
- **Reference-Interop Verified** - Every codec is validated by differential tests against the reference implementation, in both directions (see [Reference-Implementation Differential Testing](#reference-implementation-differential-testing-oracles))
- **Optimized CRC** - Slicing-by-8 implementation (3-5x faster than table lookup)
- **SIMD CRC32** - Hardware-accelerated CRC32 via aarch64 PMULL (Apple Silicon) and x86_64 PCLMULQDQ + SSE4.1
- **Modern CLI** - Progress bars, verbose output, JSON support, shell completions
- **Streaming API** - Memory-efficient processing with stdin/stdout support
- **Async I/O** - Async ZIP and async deflate support (async-io feature flag)
- **Streaming API** - GzipStream/ZlibStream/LzwStream encoders/decoders with flush modes
- **Dry-Run Mode** - Preview operations without writing files
- **EntryBuilder** - Fluent API for building archive entries
- **Pattern Filtering** - Include/exclude patterns with glob syntax
- **Metadata Preservation** - Timestamps, permissions, extended attributes
- **Auto-detection** - Automatic format detection from magic bytes
- **Flexible Overwrite** - Overwrite, skip, or prompt modes
- **Progress/Cancel** - `with_progress` and `with_cancel` builders on lz4, zstd, and lzma2 codecs
- **Optimal DEFLATE** - Zopfli-style graph-based optimal parsing via `Deflater::with_optimal_parsing(level)`
- **Bounded-Memory LZ4** - True streaming LZ4 with configurable memory budget via `with_memory_budget(usize)`
- **Snappy Parallel** - Rayon-based parallel frame compression via `parallel` feature
- **Memory-Mapped Files** - Zero-copy `MappedFile` primitive in oxiarc-core (`mmap` feature)
- **LZ4 Dict Blocks** - Block-layer prefix dictionary support via `Lz4DictBlockEncoder`/`Lz4DictBlockDecoder`, `compress_block_with_dict`, `decompress_block_dict`
- **Parallel GZIP** - pigz-style multi-member parallel GZIP via `gzip_compress_parallel`/`ParallelGzipEncoder` (`parallel` feature in oxiarc-deflate)
- **LZ77 Tuning API** - Fine-grained LZ77 heuristics via `Lz77Params` and `Lz77Preset` (nice_match + chain tuning)
- **DEFLATE Memory Pool** - Thread-safe buffer reuse via `DeflatePool`/`PooledBuf` for high-throughput workloads
- **Parallel LZMA2** - Multi-threaded LZMA2 compression via `lzma2_compress_parallel`/`ParallelLzma2Encoder` (`parallel` feature in oxiarc-lzma)
- **Raw-Preserve Append** - `oxiarc add` preserves ZIP/LZH entries byte-for-byte (no re-compression)
- **ISO 9660 Read** - `oxiarc list/extract/info/detect` support for `.iso` disc images
- **Memory Limit** - `--memory-limit <BYTES>` option for `extract` and `list` (e.g. `--memory-limit 100M`)
- **LZH/LZMA Dictionaries** - Prefix dictionary support for LZH (`LzhEncoder::with_dictionary`, `LzhDecoder::with_dictionary`) and LZMA (`LzmaEncoder::with_dictionary`, `LzmaDecoder::with_dictionary`)
- **LZMA Memory Pool** - Thread-safe buffer reuse for LZMA decoders via `LzmaPool`, `PooledBuf`, `LzmaDecoderPooled` (`parallel` feature in oxiarc-lzma)
- **Archive Repair** - Repair truncated/corrupted ZIP and TAR archives via `repair_zip`, `repair_tar`, `ZipRepair`, `TarRepair`, `RepairReport`
- **Snappy Memory Pool** - Thread-safe buffer reuse for Snappy FrameEncoder/FrameDecoder via `SnappyPool`, `PoolStats`, `compress_frame_pooled`
- **Snappy Dictionaries** - Block and frame level dictionary support (`compress_block_with_dict`, `compress_frame_with_dict`, `decompress_block_with_dict`, `decompress_frame_with_dict`)
- **Snappy Async I/O** - Async compression/decompression via `AsyncSnappyCompressor`, `AsyncSnappyDecompressor` (`async-io` feature in oxiarc-snappy)
- **Zstd Multi-Frame** - Multi-frame decompression via `decompress_multi_frame`, `decompress_multi_frame_with_dict`; streaming dict multi-frame fix
- **CLI Man Pages** - Full set of troff `.1` man pages for all CLI subcommands in `man/` directory
- **Snappy/Brotli Interop Tests** - 35 new integration tests against wire-format golden vectors (16 Snappy, 19 Brotli) validating spec compliance
- **AEC/SZIP Codec** - CCSDS-121.0-B-2 compliant adaptive entropy coding via `oxiarc-szip` with `BitReader`/`BitWriter`, `encode`/`decode`/`encode_bytes` entry points, `SzipParams` configuration, `SzipError` error type
- **Non-Panicking Constructors** - `RingBuffer::try_new`/`OutputRingBuffer::try_new` fallible alternatives to the panicking constructors, for untrusted/arbitrary window sizes (oxiarc-core)
- **CLI Quiet Mode & Stdin Everywhere** - Global `--quiet`/`-q` flag; `list`/`test`/`info`/`detect` now accept `-` for stdin (previously only `extract`/`create` did)
- **Symlink-Aware Extraction** - TAR entries that declare a symlink are recreated as real symlinks on extraction instead of being silently followed/overwritten

## Architecture

```
+----------------------------------------------------------+
| L4: Unified API (oxiarc-cli)                             |
|     CLI with progress bars, verbose mode, filters        |
+----------------------------------------------------------+
| L3: Container (oxiarc-archive)                           |
|     ZIP, TAR, GZIP, LZH, XZ, 7z, CAB, LZ4, Zstd, Bzip2, Brotli, Snappy, ISO 9660 |
+----------------------------------------------------------+
| L2: Codecs                                               |
|     oxiarc-deflate: DEFLATE (RFC 1951) + async + GZip    |
|     oxiarc-lzma: LZMA/LZMA2                              |
|     oxiarc-lzhuf: LZH (lh0, lh1, lh4, lh5, lh6, lh7, lhd) |
|     oxiarc-bzip2: BWT + MTF + Huffman                    |
|     oxiarc-lz4: LZ4 block/frame                          |
|     oxiarc-zstd: Zstandard (RFC 8878 FSE + Huffman)      |
|     oxiarc-lzw: LZW (GIF/TIFF, MSB/LSB bitstream)       |
|     oxiarc-brotli: Brotli (RFC 7932)                     |
|     oxiarc-snappy: Snappy (block + framed)                |
|     oxiarc-szip: AEC/SZIP (CCSDS-121.0-B-2 adaptive entropy coding)    |
+----------------------------------------------------------+
| L1: Core (oxiarc-core)                                   |
|     BitReader/Writer, RingBuffer, CRC-16/32/64 (simd-8)  |
+----------------------------------------------------------+
```

## Workspace Structure

| Crate | Description | Lines | Tests |
|-------|-------------|-------|-------|
| `oxiarc-core` | Core primitives: BitStream (LSB + MSB), RingBuffer, CRC-16/32/64 (slicing-by-8), EntryBuilder, Serde | ~5,573 | 187 |
| `oxiarc-deflate` | DEFLATE (RFC 1951) + async deflate + GZip (multi-member) + true streaming (GzipStream/ZlibStream) | ~8,756 | 260 |
| `oxiarc-lzhuf` | LZH compression (lh0, lh1, lh4, lh5, lh6, lh7, lhd) with LZSS + Huffman + custom dictionaries | ~6,606 | 188 |
| `oxiarc-bzip2` | Bzip2 with BWT + MTF + RLE + multi-table Huffman, multi-stream decode, de-randomisation | ~3,303 | 108 |
| `oxiarc-lz4` | LZ4 block/frame + LZ4-HC with XXHash32, linked (block-dependent) frames, acceleration parameter | ~5,971 | 166 |
| `oxiarc-zstd` | Zstandard (RFC 8878) with FSE + Huffman + XXHash64, dictionary support, multi-frame | ~7,336 | 208 |
| `oxiarc-lzma` | LZMA/LZMA2 with range coding + hash chains + memory pool, multi-chunk `.xz` | ~7,957 | 186 |
| `oxiarc-archive` | 13 container formats (ZIP, TAR, GZIP, LZH, XZ, 7z, CAB, LZ4, Zstd, Bzip2, Brotli, Snappy, ISO 9660) + async ZIP + archive repair | ~22,153 | 524 |
| `oxiarc-lzw` | LZW compression (GIF/TIFF incl. TIFF 6.0 Clear Code) with MSB/LSB bitstream, streaming encoder/decoder | ~2,775 | 100 |
| `oxiarc-brotli` | Brotli compression (RFC 7932) with the full Appendix A static dictionary, quality 0-11, streaming | ~7,153 | 219 |
| `oxiarc-snappy` | Snappy compression (block + framed format) with CRC32C, memory pool, dictionaries, async I/O | ~4,304 | 140 |
| `oxiarc-szip` | AEC/SZIP (CCSDS-121.0-B-2): encode/decode/encode_bytes, SzipParams, libaec-interoperable | ~1,902 | 47 |
| `oxiarc-cli` | CLI tool with progress bars, filters, JSON output, dry-run mode, enforced `--memory-limit`, man pages | ~6,897 | 92 |
| **Total** | **Pure Rust archive/compression library** | **~90,686 code lines (317 Rust files; 336 workspace-wide incl. fuzz)** | **2,425** |

Lines are tokei Rust code lines per crate (src + tests + examples); tests are nextest tests + doctests, measured 2026-07-13.

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
oxiarc-archive = "0.3.6"  # For archive format support
oxiarc-deflate = "0.3.6"  # For DEFLATE compression
oxiarc-lzma = "0.3.6"     # For LZMA/LZMA2 compression
oxiarc-bzip2 = "0.3.6"    # For Bzip2 compression
oxiarc-lz4 = "0.3.6"      # For LZ4 compression
oxiarc-zstd = "0.3.6"     # For Zstandard compression
oxiarc-brotli = "0.3.6"   # For Brotli compression
oxiarc-snappy = "0.3.6"   # For Snappy compression
oxiarc-szip = "0.3.6"      # For AEC/SZIP (CCSDS-121.0-B-2) compression
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

### LZH (lh0, lh1, lh4, lh5, lh6, lh7, lhd)

Japanese archive format compression:
- LZSS with configurable window sizes (4KB-64KB)
- Static Huffman coding with dual trees (codes + offsets)
- Methods: lh0 (stored), lh1 (4KB window + adaptive Huffman), lh4, lh5, lh6, lh7, lhd (directory); lh2/lh3 are not implemented; unknown methods (including lh2/lh3) are listed and skipped per entry
- Shift_JIS filenames and level-2 headers (LHA 2.x standard) on write

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

Modern fast compression (RFC 8878):
- Full decoder: FSE (predefined, RLE, custom `FSE_Compressed`, and repeat modes) plus 1- and 4-stream Huffman literals — differentially verified byte-identical against reference `zstd` (incl. dictionary frames), with the RFC 8878 LIFO/MSB-first backward bitstream
- Encoder: Huffman-compressed literal sections (chosen when they beat Raw/RLE, self-verified per section); sequences use the RFC 8878 predefined/RLE FSE tables — RFC-valid and accepted by `zstd -d`, but custom block-optimal sequence tables are not emitted yet, so ratio on some inputs trails the reference encoder
- XXHash64 checksums
- Dictionary support

### LZW

Lempel-Ziv-Welch compression:
- GIF LZW codec with configurable initial code size
- LSB-first bitstream packing (GIF standard)
- MSB-first bitstream packing (TIFF standard) with TIFF 6.0 Clear Code semantics — interoperable with libtiff/Pillow/GDAL in both directions (oxiarc's encoded output is byte-identical to libtiff's)
- Variable bit widths (2-12 bits) with clear/EOI codes

### Brotli (RFC 7932)

Modern compression format:
- Full RFC 7932 decoder: block-type switching, context maps with the exact §7.1 context tables, the complete distance code space, metadata meta-blocks — differentially verified byte-identical against the reference `brotli` CLI (qualities 0-11, windows 10-24)
- The complete, byte-exact 122,784-byte Appendix A static dictionary with all 121 word transforms (UTF-8-aware ferment casing)
- RFC-conformant encoder accepted by `brotli -d`; ratio trails the reference encoder at quality 10-11 and on structured binary data (no encode-side block-splitting/context modeling — a ratio limitation, not a correctness one)
- Quality levels 0-11 (fast to best compression)
- Streaming compression/decompression API

### AEC/SZIP (CCSDS-121.0-B-2)

Adaptive entropy coding for scientific data:
- CCSDS-121.0-B-2 standard implementation, differentially verified byte-identical against live libaec 1.1.4 in both directions
- Used in HDF5 and NetCDF scientific datasets
- `BitReader`/`BitWriter` for efficient bit manipulation
- `SzipParams` struct for encoding/decoding configuration
- `encode` / `decode` / `encode_bytes` entry points


## Status

| Crate           | Status  | Public API | Tests Passing |
|-----------------|---------|------------|---------------|
| oxiarc-core     | Stable  | 228        | 187           |
| oxiarc-deflate  | Stable  | 168        | 260           |
| oxiarc-lzhuf    | Stable  | 106        | 188           |
| oxiarc-bzip2    | Stable  | 56         | 108           |
| oxiarc-lz4      | Stable  | 126        | 166           |
| oxiarc-zstd     | Stable  | 161        | 208           |
| oxiarc-lzma     | Stable  | 188        | 186           |
| oxiarc-archive  | Stable  | 438        | 524           |
| oxiarc-lzw      | Stable  | 67         | 100           |
| oxiarc-brotli   | Stable  | 101        | 219           |
| oxiarc-snappy   | Stable  | 35         | 140           |
| oxiarc-szip     | Stable  | 27         | 47            |
| oxiarc-cli      | Stable  | 45         | 92            |
| **Total**       |         | **1,746**  | **2,425**     |

Test counts measured 2026-07-13 (nextest tests + doctests, all features, 0 failed, 0 ignored); public-API item counts are the v0.3.6 snapshot. All crates are feature-complete and, as of the 2026-07-13 production-hardening campaign, validated against the reference implementation of every format in both directions. Ahead of a 1.0 release, 18 public format/method/status/error enums (`FlushMode`, `CompressStatus`/`DecompressStatus`, `CompressionMethod`, `EntryType`, `ArchiveFormat`, zstd `BlockType`/`LiteralsBlockType`, `Lz4Level`, the codec error enums, and more) are marked `#[non_exhaustive]` for forward-compatible matching.
Streaming compression/decompression support in `oxiarc-deflate`:
- `GzipStreamEncoder`/`GzipStreamDecoder` with configurable block sizes
- `ZlibStreamEncoder`/`ZlibStreamDecoder` with flush modes
- Flush modes: `sync_flush`, `full_flush`, `partial_flush`

## Format Support Matrix

| Format | Read | Write | Compression | Checksums | Notes |
|--------|------|-------|-------------|-----------|-------|
| **ZIP** | ✅ | ✅ | DEFLATE, Store | CRC-32 | Zip64 support, data descriptors, async ZIP (async-io feature), AES-128/192/256 + ZipCrypto encryption (external encrypted archives detected via general-purpose bit 0); spanned/multi-volume ZIP unsupported (rejected) |
| **TAR** | ✅ | ✅ | N/A (container only) | None | UStar, PAX, GNU long names, GNU sparse (old-format 'S' + PAX 0.1, both readers; PAX 1.0 sparse unsupported) |
| **GZIP** | ✅ | ✅ | DEFLATE | CRC-32 | RFC 1952 compliant |
| **LZH** | ✅ | ✅ | lh0, lh1, lh4, lh5, lh6, lh7 | CRC-16 | Shift_JIS support, all header levels; lh2/lh3 not implemented |
| **XZ** | ✅ | ✅ | LZMA2 | CRC-64 | Block checksums |
| **7z** | ✅ | ❌ | LZMA/LZMA2 | CRC-32 | Read-only, partial support |
| **CAB** | ✅ | ❌ | None, MSZIP | CFDATA checksums | Microsoft Cabinet, read-only; MSZIP window carried across CFDATA blocks, per-block checksums validated; Quantum/LZX unsupported (clean error, never silent raw copy) |
| **LZ4** | ✅ | ✅ | LZ4, LZ4-HC | XXHash32 | Frame format, block/content checksums |
| **Zstd** | ✅ | ✅ | Zstandard | XXHash64 | RFC 8878 frame format; full decoder (FSE + 1/4-stream Huffman); encoder: Huffman literals + predefined/RLE FSE sequences (custom sequence tables not emitted — ratio, not correctness) |
| **Bzip2** | ✅ | ✅ | BWT + Huffman | CRC-32 | Block-sorting compression |
| **Brotli** | ✅ | ✅ | Brotli (RFC 7932) | None | Quality levels 0-11, full Appendix A static dictionary; `.br` file-path CLI support via extension fallback (raw Brotli has no magic bytes) |
| **Snappy** | ✅ | ✅ | Snappy | CRC32C | Block and framed formats |
| **ISO 9660** | ✅ | ❌ | Store | None | Read-only; list/extract/info/detect support |

### ZIP Encryption

- **AES-128 / AES-192 / AES-256** (WinZip AE-2) via a genuine, FIPS-197-compliant AES cipher (key schedule/round count derived from key length), CTR mode, HMAC-SHA1 authentication tag verified in constant time, and OS-CSPRNG-sourced salts. AE-2 entries write CRC=0 per the WinZip AES spec.
- **Traditional ZipCrypto** encryption/decryption, with a CSPRNG-sourced header. Info-ZIP (`zip -e`) streamed archives — which derive the password-check byte from the DOS mtime rather than the CRC — decrypt correctly.
- **Encryption detection uses the ZIP general-purpose bit 0** (plus method 99 for AES), so archives encrypted by external tools (`zip -e`, 7-Zip, WinRAR, Python) are correctly reported as encrypted and require a password — they are never silently extracted as garbage.
- **Spanned/multi-volume ZIP archives are not supported** — both classic and Zip64 end-of-central-directory records that declare more than one disk are rejected with an explicit error rather than silently misread.

### `--memory-limit`

`extract`/`list --memory-limit <BYTES>` bounds memory use **during decompression for every supported format**. Container entries (ZIP/TAR/LZH/7z/CAB/ISO) are checked against their declared sizes before allocating. Single-file formats are enforced during decode: gzip via the trailing ISIZE field, xz via the stream index's declared uncompressed size, lz4/zstd via the frame content-size fields, and bzip2/brotli/snappy via bounded decoders (`decompress_with_limit`) that return an error as soon as output would exceed the limit — no pre-flight size field is required. Measured: a brotli decompression bomb extracted under `--memory-limit 1M` peaks at 3.3 MB RSS (vs 72.9 MB unbounded) and exits non-zero.

Independently of `--memory-limit`, every header-driven allocation across the readers (ZIP central directory/AES payloads, TAR PAX/extension data, LZH, 7z, ISO 9660 directory extents, zstd frame content-size, LZMA/LZMA2 dictionaries) validates the declared length against the bytes actually available and allocates via `try_reserve`/`try_reserve_exact` rather than an unconditional `Vec::with_capacity`/`vec![0; n]`. A crafted, wildly-oversized header therefore surfaces as a clean error instead of an allocator abort/OOM even with no `--memory-limit` set at all.

## Reference-Implementation Differential Testing (Oracles)

Self round-trips alone cannot prove interoperability — an encoder and decoder that share the same deviation from a spec will round-trip perfectly while being incompatible with everything else. Every OxiArc codec is therefore validated by **differential tests against the reference implementation, in both directions**: reference-produced streams must decode byte-identically, and oxiarc-produced streams must be accepted (and decode byte-identically) by the reference tool.

Two layers keep this permanent:

1. **Always-run embedded corpora** — golden byte vectors generated by the reference tools are committed and checked on every `cargo nextest run`, with no external dependencies.
2. **Live oracle suites** — opt-in Cargo features that shell out to the real reference tool. They **self-skip with a printed note (never fail) when the tool is absent**, so enabling them is always safe and CI stays hermetic.

| Crate | Feature | Reference oracle |
|-------|---------|------------------|
| `oxiarc-zstd` | `zstd-oracle` | `zstd` CLI |
| `oxiarc-brotli` | `brotli-oracle` | `brotli` CLI |
| `oxiarc-lzma` | `xz-oracle` | `xz` (XZ Utils) CLI |
| `oxiarc-bzip2` | `bzip2-oracle` | `bzip2` CLI |
| `oxiarc-lz4` | `lz4-oracle` | `lz4` CLI |
| `oxiarc-snappy` | `snappy-oracle` | `python3` + `cramjam` |
| `oxiarc-deflate` | `zlib-oracle` | `python3` (zlib/gzip) + `gzip` CLI |
| `oxiarc-lzw` | `tiff-oracle` | `python3` + Pillow (libtiff), `tiffcp` when present |
| `oxiarc-szip` | `libaec-oracle` | libaec (compiled harness; `LIBAEC_PREFIX` env var) |
| `oxiarc-lzhuf` | `lha-oracle` | `lha` (Lhasa) CLI |
| `oxiarc-archive` | `zip-oracle`, `xz-oracle`, `lha-oracle` | Info-ZIP `zip`/`unzip` + Python `zipfile`; `xz`; `lha` |

```bash
# Run one codec's live oracle against the reference tool
cargo nextest run -p oxiarc-zstd --features zstd-oracle
cargo nextest run -p oxiarc-brotli --features brotli-oracle

# Archive-level oracles
cargo nextest run -p oxiarc-archive --features zip-oracle,xz-oracle,lha-oracle

# Everything, everywhere (oracle suites self-skip for any missing tool)
cargo nextest run --workspace --all-features
```

Verified interop snapshot (2026-07-13, live tools): zstd 64/64 corpus + 101/101 wide frames decode byte-identical, 85/85 oxiarc frames accepted by `zstd -d`; brotli 608/608 decode / 588/588 accepted; xz 5.8.3 60/60 decode / 8/8 encode; bzip2 1.0.8 324/324 both directions; lz4 1.10.0 11/11 + 44/44 + 3/3 linked; TIFF-LZW 125/125 vs Pillow/libtiff (encoder byte-identical to libtiff); libaec 1.1.4 2450/2450 decode + 4900/4900 encode; DEFLATE/zlib/gzip bit-exact vs CPython + gzip CLI.

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

# Run all tests (2,288 via nextest + 137 doctests = 2,425)
cargo nextest run --workspace --all-features
cargo test --doc --workspace --all-features

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
   - Ensure `cargo clippy --workspace --all-features --all-targets` and
     `cargo nextest run --workspace --all-features` pass locally (this
     project has no CI pipeline yet, so these checks are not automated)
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

## Sponsorship

OxiARC is developed and maintained by **COOLJAPAN OU (Team Kitasan)**.

If you find OxiARC useful, please consider sponsoring the project to support continued development of the Pure Rust ecosystem.

[![Sponsor](https://img.shields.io/badge/Sponsor-%E2%9D%A4-red?logo=github)](https://github.com/sponsors/cool-japan)

**[https://github.com/sponsors/cool-japan](https://github.com/sponsors/cool-japan)**

Your sponsorship helps us:
- Maintain and improve the COOLJAPAN ecosystem
- Keep the entire ecosystem (OxiGDAL, OxiMedia, OxiBLAS, OxiFFT, SciRS2, etc.) 100% Pure Rust
- Provide long-term support and security updates

## License

Licensed under the Apache License, Version 2.0 ([LICENSE](LICENSE) or http://www.apache.org/licenses/LICENSE-2.0).

## Repository

https://github.com/cool-japan/oxiarc

## Authors

COOLJAPAN OU <contact@cooljapan.tech>
