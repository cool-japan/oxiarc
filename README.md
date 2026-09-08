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
- **LZH** - LZSS + Huffman (lh0, lh4, lh5, lh6, lh7) plus lh1/lh2/lh3 (LHarc 1.x/2.x adaptive and block-static Huffman), the LArc methods lzs/lz4/lz5, PMarc pm0, and lhd directory entries; pm1/pm2 are not implemented
- **Bzip2** - BWT + MTF + RLE + Huffman
- **LZ4** - Ultra-fast LZ77 variant with LZ4-HC
- **Zstandard** (RFC 8878) - Full decoder (FSE + 1/4-stream Huffman literals); encoder emits Huffman-compressed literals with predefined/RLE FSE sequence coding
- **LZW** - Lempel-Ziv-Welch for TIFF and GIF compression (MSB/LSB bitstream)
- **Brotli** (RFC 7932) - LZ77 + context-dependent Huffman, complete Appendix A static dictionary (122,784 bytes, all 121 transforms), quality 0-11
- **Snappy** - Ultra-fast LZ77 variant with block and framed formats
- **Store** - No compression
- **AEC/SZIP** (CCSDS-121.0-B-2) - Adaptive entropy coding for scientific datasets

### HTTP Content-Coding and Image Formats (new in 0.4.2)
- **HTTP `Content-Encoding`/`Accept-Encoding`** (`oxiarc-http`) - decode and encode `gzip`, `deflate`, `br`, `zstd`, legacy `compress` (`.Z`), and RFC 9842 shared-dictionary `dcb`/`dcz`; RFC 9110 negotiation, chained codings, bounded-memory decode limits
- **PNG** (`oxiarc-png`, ISO/IEC 15948) - every colour type/depth, Adam7 interlacing, APNG, Apple `CgBI`, `png`-0.18-shaped compat facade
- **JPEG** (`oxiarc-jpeg`, ITU-T T.81) - baseline/progressive/lossless, arithmetic coding (SOF9-11), reduced/enlarged-scale decode, `zune_jpeg`/`jpeg-decoder`/`image`-shaped compat facades
- **TIFF** (`oxiarc-tiff`, TIFF 6.0 + BigTIFF) - LZW/Deflate/ZSTD/LZMA/JPEG/PackBits/CCITT G3/G4 (incl. uncompressed mode), `rayon` parallel strips/tiles, `tiff`-0.11-shaped compat facade
- **Image facade** (`oxiarc-image`) - thin `image`-0.25-crate-shaped `DynamicImage`/`ImageReader`/codec API over PNG/JPEG/TIFF, for drop-in migration off the `image` crate

### Core Features
- **Pure Rust** - No C/Fortran dependencies, 100% safe Rust
- **Reference-Interop Verified** - Every codec is validated by differential tests against the reference implementation, in both directions (see [Reference-Implementation Differential Testing](#reference-implementation-differential-testing-oracles))
- **Optimized CRC** - Slicing-by-8 implementation (3-5x faster than table lookup)
- **SIMD CRC32** - Hardware-accelerated CRC32 via aarch64 PMULL (Apple Silicon) and x86_64 PCLMULQDQ + SSE4.1
- **Modern CLI** - Progress bars, verbose output, JSON support, shell completions
- **Streaming API** - Memory-efficient processing with stdin/stdout support
- **Async I/O** - Async ZIP and async deflate support (async-io feature flag)
- **Streaming API** - GzipStream/ZlibStream encoders/decoders with flush modes
- **LZW Streaming** - `LzwStreamEncoder` writes incrementally with flush modes; `LzwStreamDecoder` currently buffers the whole input before decoding (own length-prefixed framing, not a bare TIFF/GIF bitstream) - not yet true streaming
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
|     CLI with progress bars, verbose mode, filters,       |
|     PNG/JPEG/TIFF detect+info fallback                   |
+----------------------------------------------------------+
| L3.5: Image + HTTP (new in 0.4.2)                         |
|     oxiarc-image: image-0.25-shaped facade over PNG/JPEG/TIFF |
|     oxiarc-http: Content-Encoding gzip/deflate/br/zstd/compress/dcb/dcz |
|     oxiarc-png:  PNG (ISO/IEC 15948) + APNG               |
|     oxiarc-jpeg: JPEG (ITU-T T.81) + arithmetic + OJPEG   |
|     oxiarc-tiff: TIFF 6.0 + BigTIFF, all baseline codecs  |
+----------------------------------------------------------+
| L3: Container (oxiarc-archive)                           |
|     ZIP, TAR, GZIP, LZH, XZ, 7z, CAB, LZ4, Zstd, Bzip2, Brotli, Snappy, ISO 9660 |
+----------------------------------------------------------+
| L2: Codecs                                               |
|     oxiarc-deflate: DEFLATE (RFC 1951) + async + GZip + resumable inflate |
|     oxiarc-lzma: LZMA/LZMA2 + reusable .xz decoder        |
|     oxiarc-lzhuf: LZH (lh0, lh1, lh4, lh5, lh6, lh7, lhd) |
|     oxiarc-bzip2: BWT + MTF + Huffman                    |
|     oxiarc-lz4: LZ4 block/frame                          |
|     oxiarc-zstd: Zstandard (RFC 8878 FSE + Huffman), push decoder |
|     oxiarc-lzw: LZW (GIF/TIFF/.Z, MSB/LSB bitstream, 9-16 bit) |
|     oxiarc-brotli: Brotli (RFC 7932), push decoder, shared dict |
|     oxiarc-snappy: Snappy (block + framed)                |
|     oxiarc-szip: AEC/SZIP (CCSDS-121.0-B-2 adaptive entropy coding)    |
+----------------------------------------------------------+
| L1: Core (oxiarc-core)                                   |
|     BitReader/Writer, RingBuffer, CRC-16/32/64 (simd-8), SHA-256 |
+----------------------------------------------------------+
```

## Workspace Structure

| Crate | Description | Lines | Tests |
|-------|-------------|-------|-------|
| `oxiarc-core` | Core primitives: BitStream (LSB + MSB), RingBuffer, CRC-16/32/64 (slicing-by-8), SHA-256, EntryBuilder, Serde | ~7,086 | 200 |
| `oxiarc-deflate` | DEFLATE (RFC 1951), zlib-faithful encoder, resumable `InflateStream`/`WrappedInflate`, async deflate, GZip (multi-member) | ~23,129 | 455 |
| `oxiarc-lzhuf` | LZH compression (lh0, lh1, lh4, lh5, lh6, lh7, lhd) with LZSS + Huffman + custom dictionaries | ~9,782 | 269 |
| `oxiarc-bzip2` | Bzip2 with BWT + MTF + RLE + multi-table Huffman, multi-stream decode, de-randomisation | ~3,303 | 104 |
| `oxiarc-lz4` | LZ4 block/frame + LZ4-HC with XXHash32, linked (block-dependent) frames, acceleration parameter | ~6,229 | 158 |
| `oxiarc-zstd` | Zstandard (RFC 8878) with FSE + Huffman + XXHash64, `ZstdStream` push decoder, dictionary support, multi-frame | ~15,031 | 341 |
| `oxiarc-lzma` | LZMA/LZMA2 with range coding + hash chains + memory pool, reusable `xz::XzDecoder`, multi-block `.xz` writer | ~13,417 | 277 |
| `oxiarc-archive` | 13 container formats (ZIP, TAR, GZIP, LZH, XZ, 7z, CAB, LZ4, Zstd, Bzip2, Brotli, Snappy, ISO 9660) + async ZIP + archive repair | ~24,395 | 583 |
| `oxiarc-lzw` | LZW compression (GIF/TIFF/`.Z`, TIFF 6.0 Clear Code, 9-16 bit codes) with MSB/LSB bitstream, streaming encoder/decoder | ~9,638 | 233 |
| `oxiarc-brotli` | Brotli compression (RFC 7932) with the full Appendix A static dictionary, quality 0-11, shared dictionaries, `dcb` framing, streaming | ~15,232 | 349 |
| `oxiarc-snappy` | Snappy compression (block + framed format) with CRC32C, memory pool, dictionaries, async I/O | ~4,306 | 132 |
| `oxiarc-szip` | AEC/SZIP (CCSDS-121.0-B-2): encode/decode/encode_bytes, SzipParams, libaec-interoperable | ~1,902 | 46 |
| `oxiarc-http` | HTTP `Content-Encoding`/`Accept-Encoding`: gzip/deflate/br/zstd/compress/dcb/dcz, negotiation, bounded decode limits | ~9,094 | 310 |
| `oxiarc-png` | PNG (ISO/IEC 15948) decoder/encoder: every colour type/depth, Adam7, APNG, `CgBI`, `png`-0.18-shaped compat | ~15,144 | 303 |
| `oxiarc-jpeg` | JPEG (ITU-T T.81) decoder/encoder: baseline/progressive/lossless/arithmetic, scaled decode, compat facades | ~28,077 | 591 |
| `oxiarc-tiff` | TIFF 6.0 + BigTIFF decoder/encoder: LZW/Deflate/ZSTD/LZMA/JPEG/PackBits/CCITT, `rayon`, `tiff`-0.11-shaped compat | ~26,601 | 535 |
| `oxiarc-image` | Thin `image`-0.25-crate-shaped facade over PNG/JPEG/TIFF (`DynamicImage`, `ImageReader`, no image processing) | ~4,301 | 145 |
| `oxiarc-cli` | CLI tool with progress bars, filters, JSON output, dry-run mode, enforced `--memory-limit`, PNG/JPEG/TIFF detect/info, man pages | ~7,844 | 124 |
| **Total** | **Pure Rust archive/compression library** | **~224,511 code lines (678 Rust files; 714 workspace-wide incl. fuzz)** | **5,155** |

Lines are tokei Rust code lines per crate (src + tests + examples); the Tests column is nextest (all-features, per-crate); the workspace additionally has 346 doctests (not attributed per crate), for **5,501 tests total**. Measured 2026-09-08.

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
oxiarc-archive = "0.4.2"  # For archive format support
oxiarc-deflate = "0.4.2"  # For DEFLATE compression
oxiarc-lzma = "0.4.2"     # For LZMA/LZMA2 compression
oxiarc-bzip2 = "0.4.2"    # For Bzip2 compression
oxiarc-lz4 = "0.4.2"      # For LZ4 compression
oxiarc-zstd = "0.4.2"     # For Zstandard compression
oxiarc-brotli = "0.4.2"   # For Brotli compression
oxiarc-snappy = "0.4.2"   # For Snappy compression
oxiarc-szip = "0.4.2"      # For AEC/SZIP (CCSDS-121.0-B-2) compression
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
- Compression levels 0-9, encoder byte-identical to CPython `zlib.compress` at every level 1-9 (a faithful port of zlib's `deflate.c`/`trees.c`; `Deflater::with_strategy` exposes `Z_DEFAULT_STRATEGY`/`Z_FILTERED`/`Z_HUFFMAN_ONLY`/`Z_RLE`/`Z_FIXED`). Level 0's stored blocks are cut at 65535 bytes where zlib cuts at its pending-buffer bound, so the bytes differ there — valid, round-tripping, and never larger than CPython's
- Resumable, genuinely incremental decode core (`InflateStream`, `WrappedInflate` for zlib/gzip framing, `InflateReader`/`AsyncInflateReader`) shared by `oxiarc-png`'s `IDAT` chain, `oxiarc-http`'s response bodies and `oxiarc-tiff`'s strips — no `read_to_end`-then-decode

### LZH (lh0, lh1, lh4, lh5, lh6, lh7, lhd)

Japanese archive format compression:
- LZSS with configurable window sizes (4KB-64KB)
- Static Huffman coding with dual trees (codes + offsets)
- Methods: lh0 (stored), lh1 (4KB window + adaptive Huffman), lh2 (8KB + adaptive Huffman), lh3 (8KB + block-static Huffman), lh4, lh5, lh6, lh7, lhd (directory), lzs/lz5 (LArc LZSS), lz4/pm0 (stored); pm1/pm2 are not implemented; unknown methods are listed and skipped per entry
- Shift_JIS filenames and level-2 headers (LHA 2.x standard) on write

### LZMA/LZMA2

Advanced compression used in 7z and XZ:
- LZ77-style dictionary compression
- Range coding for entropy encoding
- Context-dependent probability models
- 11-bit probability model (2048 states)
- Reusable `xz::XzDecoder` (keeps its dictionary/probability-model/coder-state allocation across many same-dictionary-size `.xz` streams — built for TIFF's thousands-of-strips-per-image case) and multi-block `XzWriter` (`with_block_size`, per-block `CheckType`)

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
- Encoder: Huffman-compressed literal sections (chosen when they beat Raw/RLE, self-verified per section); sequences now also emit `FSE_Compressed_Mode` custom tables (predefined/RLE/custom, whichever costs fewest bits including the table description, chosen per literal-length/offset/match-length category) alongside the RFC-valid predefined/RLE tables
- `ZstdStream` push decoder — genuinely incremental (no `read_to_end`), a real sliding-window ring, memory ceilings enforced before a block is decoded, `with_max_output`/`with_max_window`/`with_multi_frame`/`with_dictionary`
- Decode throughput rebuilt around the reference decoder's data layout (bulk bit-container refills, four-stream lockstep Huffman literals, pattern-doubling overlapping-match copies) — 2-8x faster on the shapes that were per-byte-work-bound, at parity with `zstd -b -d` (0.5x-4x) on 11 of 12 measured shapes
- XXHash64 checksums
- Dictionary support

### LZW

Lempel-Ziv-Welch compression:
- GIF LZW codec with configurable initial code size, differentially verified against Pillow's own decoder in both directions
- LSB-first bitstream packing (GIF standard); explicit `LzwBitOrder` on `LzwConfig` lets any generic entry point use either order
- MSB-first bitstream packing (TIFF standard) with TIFF 6.0 Clear Code semantics — interoperable with libtiff/Pillow/GDAL in both directions (oxiarc's encoded output is byte-identical to libtiff's); `TIFF_COMPAT_LSB` for libtiff's pre-1993 `LZWDecodeCompat` strips
- Variable bit widths, now **9-16 bits** (was 12-bit-capped before 0.4.2), with clear/EOI codes
- The legacy UNIX `compress(1)`/`.Z` container (`oxiarc_lzw::z`), both directions, byte-identical to `compress -b N -c` and readable by/producing output readable by `gzip -dc`/`uncompress -c`
- Decoder rebuilt to libtiff's own `LZWDecode` algorithm shape: 1.25x-2.6x faster, now within 1.25x-1.37x of libtiff 4.7.1's own decode time (re-measured 2026-09-08 on a machine at load 56-66, where the band widens to 1.00x-1.56x — load scatter, not a slower decoder; see `oxiarc-lzw/README.md`)

### Brotli (RFC 7932)

Modern compression format:
- Full RFC 7932 decoder: block-type switching, context maps with the exact §7.1 context tables, the complete distance code space, metadata meta-blocks, shared (custom LZ77) dictionaries — differentially verified byte-identical against the reference `brotli` CLI (qualities 0-11, windows 10-24)
- The complete, byte-exact 122,784-byte Appendix A static dictionary with all 121 word transforms (UTF-8-aware ferment casing)
- Encoder does literal/insert-copy/distance block-type splitting and per-context histogram assignment at quality 10-11 (coordinate-ascent search, keeps the smaller of split-vs-unsplit), plus shared-dictionary-aware matching — RFC-conformant, accepted by `brotli -d`, and a shared dictionary can never cost ratio (every meta-block is tried both ways)
- Quality levels 0-11 (fast to best compression)
- `Content-Encoding: dcb` (RFC 9842) framing for HTTP compression-dictionary transport (`oxiarc-brotli::dcb`, wired through `oxiarc-http`)
- Streaming compression/decompression API (`BrotliStream` push decoder)

### AEC/SZIP (CCSDS-121.0-B-2)

Adaptive entropy coding for scientific data:
- CCSDS-121.0-B-2 standard implementation, differentially verified byte-identical against live libaec 1.1.4 in both directions
- Used in HDF5 and NetCDF scientific datasets
- `BitReader`/`BitWriter` for efficient bit manipulation
- `SzipParams` struct for encoding/decoding configuration
- `encode` / `decode` / `encode_bytes` entry points

## HTTP Content-Coding and Image Formats (new in 0.4.2)

Five new crates close the two routes an ecosystem-wide dependency audit found
pulling `flate2` into 39 of 93 `~/work` project lockfiles: `ureq`'s default
`gzip` feature (`oxiarc-http`), and the `image` crate's `png`/`tiff` codecs
(`oxiarc-png`, `oxiarc-jpeg`, `oxiarc-tiff`, and the `oxiarc-image` facade
over all three). All five ship `#![forbid(unsafe_code)]`. Every snippet below
is lifted verbatim (module-level rustdoc hidden lines removed) from a
doctest that passes in this crate as of this release.

### `oxiarc-http` — `Content-Encoding`/`Accept-Encoding`

```rust
use oxiarc_http::{AcceptEncoding, ContentCoding, EncodeOptions, encode_body, negotiate};

fn main() {
    // Client side: advertise every coding this build can decode.
    let accept = AcceptEncoding::all_supported();
    let header_value = accept.to_header_value(); // None => send no header at all

    // Server side: negotiate against what it received, and only what this
    // build can actually produce. Filter by `is_encodable` so the list tracks
    // this crate's own compiled-in features rather than being hardcoded.
    let available: Vec<ContentCoding> = [
        ContentCoding::Zstd,
        ContentCoding::Brotli,
        ContentCoding::Gzip,
        ContentCoding::Deflate,
    ]
    .into_iter()
    .filter(ContentCoding::is_encodable)
    .collect();
    let chosen = negotiate(header_value.as_deref(), &available)
        .expect("identity is always acceptable here, so this never fails");

    let body = b"hello, world! hello, world! hello, world!";
    if let Some(coding) = chosen {
        let compressed = encode_body(&coding, body, EncodeOptions::default())
            .expect("`available` only ever contains codings this build can encode");
        assert!(compressed.len() < body.len());
        // ... set Content-Encoding: coding.as_str(), Content-Length, Vary: Accept-Encoding
    }
}
```

`oxiarc-http`'s default features are `gzip` + `deflate` only. `br`, `zstd`,
the legacy `compress`, and the RFC 9842 `dcb`/`dcz` codings each need their
own Cargo feature (`brotli`, `zstd`, `compress`, `dcb`, `dcz`), so a
default-feature client never advertises a coding it cannot decode:
`AcceptEncoding::all_supported()` returns exactly `"gzip, deflate"` there,
and `is_decodable(Brotli)` is `false`. See the feature matrix in
`oxiarc-http/README.md`.

Recipes for `ureq` 3, `reqwest` and `oxihttp` ship as runnable
`oxiarc-http/examples/`.

### `oxiarc-png`

```rust
use std::fs::File;

fn main() -> Result<(), oxiarc_png::DecodingError> {
    let decoder = oxiarc_png::Decoder::new(File::open("image.png")?);
    let mut reader = decoder.read_info()?;
    while let Some(row) = reader.next_row()? {
        let _pixels: &[u8] = row.data();
    }
    Ok(())
}
```

### `oxiarc-jpeg`

```rust
use oxiarc_jpeg::{Decoder, InputColor, Subsampling, EncodeOptions,
                  encode_to_vec_with_options};

fn main() -> Result<(), oxiarc_jpeg::JpegError> {
    let pixels = vec![90u8; 16 * 16 * 3];
    let options = EncodeOptions {
        quality: 92,
        subsampling: Subsampling::S444,
        ..Default::default()
    };
    let jpeg = encode_to_vec_with_options(&pixels, 16, 16, InputColor::Rgb, &options)?;

    let decoded = Decoder::new(&jpeg[..]).decode()?;
    assert_eq!(decoded.len(), 16 * 16 * 3);
    assert!(decoded.iter().all(|&v| v.abs_diff(90) <= 2));
    Ok(())
}
```

### `oxiarc-tiff`

```rust
use oxiarc_tiff::{ColorType, Decoder, Encoder, ImageSpec, Samples};
use std::io::Cursor;

fn main() -> Result<(), oxiarc_tiff::TiffError> {
    // Build a 4x2 greyscale image so the example is self-contained.
    let pixels: Vec<u8> = vec![0, 40, 80, 120, 160, 200, 240, 255];
    let mut buffer = Cursor::new(Vec::new());
    let mut encoder = Encoder::new(&mut buffer)?;
    encoder.write_image(&ImageSpec::new(4, 2, ColorType::Gray(8)), &pixels)?;
    encoder.finish()?;

    let mut decoder = Decoder::new(Cursor::new(buffer.into_inner()))?;
    assert_eq!(decoder.dimensions()?, (4, 2));
    assert_eq!(decoder.color_type()?, ColorType::Gray(8));
    match decoder.read_image()? {
        Samples::U8(data) => assert_eq!(data, pixels),
        other => panic!("unexpected sample type: {:?}", other.sample_type()),
    }
    Ok(())
}
```

### `oxiarc-image` — the `image`-crate-shaped facade

```toml
[dependencies]
image = { package = "oxiarc-image", version = "0.4" }
```

```rust
use oxiarc_image::{ImageBuffer, Rgb};

fn main() {
    let buf: ImageBuffer<Rgb<u8>> = ImageBuffer::from_fn(4, 4, |x, _y| Rgb::new(x as u8 * 60, 0, 0));
    let image = oxiarc_image::DynamicImage::ImageRgb8(buf);

    let path = std::env::temp_dir().join("example_encode.png");
    image.save(&path).expect("save");
    let reread = oxiarc_image::open(&path).expect("reopen");
    assert_eq!(reread.dimensions(), (4, 4));
}
```

### CLI: detecting and inspecting images

```bash
oxiarc detect photo.jpg      # -> "PNG image (not an archive)" / JPEG / TIFF
oxiarc info image.png        # dimensions, colour type, bit depth, compression,
                              # chunk/segment/IFD summary
```

## Status

| Crate           | Status  | Public API | Tests Passing |
|-----------------|---------|------------|---------------|
| oxiarc-core     | Stable  | 306        | 200           |
| oxiarc-deflate  | Stable  | 294        | 455           |
| oxiarc-lzhuf    | Stable  | 138        | 269           |
| oxiarc-bzip2    | Stable  | 46         | 104           |
| oxiarc-lz4      | Stable  | 133        | 158           |
| oxiarc-zstd     | Stable  | 197        | 341           |
| oxiarc-lzma     | Stable  | 290        | 277           |
| oxiarc-archive  | Stable  | 474        | 583           |
| oxiarc-lzw      | Stable  | 104        | 233           |
| oxiarc-brotli   | Stable  | 195        | 349           |
| oxiarc-snappy   | Stable  | 48         | 132           |
| oxiarc-szip     | Stable  | 29         | 46            |
| oxiarc-http     | Stable  | 99         | 310           |
| oxiarc-png      | Stable  | 505        | 303           |
| oxiarc-jpeg     | Stable  | 244        | 591           |
| oxiarc-tiff     | Stable  | 654        | 535           |
| oxiarc-image    | Stable  | 143        | 145           |
| oxiarc-cli      | Stable  | 71         | 124           |
| **Total**       |         | **3,969**  | **5,155**     |

Test counts measured 2026-09-08 (nextest, all features, per-crate, 0 failed, 0 skipped; the workspace additionally has 346 doctests not attributed per crate, for **5,501 tests total**). Public-API item counts are a `rg '^\s*pub (fn|struct|enum|trait|const|static|type|mod)'` sweep over each crate's `src/`, measured the same day — a coarser proxy than a rustdoc item count, and not directly comparable to any pre-0.4.2 snapshot that used a different methodology, but current and consistent across all 18 crates. All crates are feature-complete and, as of the 2026-07-13 production-hardening campaign, validated against the reference implementation of every format in both directions (the five new 0.4.2 crates — `oxiarc-http`, `oxiarc-png`, `oxiarc-jpeg`, `oxiarc-tiff`, `oxiarc-image` — each carry their own live reference-oracle suite from day one; see the table below). Ahead of a 1.0 release, 18+ public format/method/status/error enums (`FlushMode`, `CompressStatus`/`DecompressStatus`, `CompressionMethod`, `EntryType`, `ArchiveFormat`, zstd `BlockType`/`LiteralsBlockType`, `Lz4Level`, `ContentCoding`, `UnsupportedReason`, `ColorSpace`, `ImageFormat`, `DynamicImage`, the codec error enums, and more) are marked `#[non_exhaustive]` for forward-compatible matching.
Streaming compression/decompression support in `oxiarc-deflate`:
- `GzipStreamEncoder`/`GzipStreamDecoder` with configurable block sizes
- `ZlibStreamEncoder`/`ZlibStreamDecoder` with flush modes
- Flush modes: `sync_flush`, `full_flush`, `partial_flush`

## Format Support Matrix

| Format | Read | Write | Compression | Checksums | Notes |
|--------|------|-------|-------------|-----------|-------|
| **ZIP** | ✅ | ✅ | DEFLATE, Store | CRC-32 | Zip64 support, data descriptors, async ZIP (async-io feature), AES-128/192/256 + ZipCrypto encryption (external encrypted archives detected via general-purpose bit 0); spanned/multi-volume ZIP unsupported (rejected) |
| **TAR** | ✅ | ✅ | N/A (container only) | None | UStar, PAX, GNU long names, GNU sparse read support for all three variants — old-format 'S', PAX 0.1, and PAX 1.0 — in both the seekable and streaming readers (writer-side sparse emission unsupported; sparse-source files are written as regular dense entries) |
| **GZIP** | ✅ | ✅ | DEFLATE | CRC-32 | RFC 1952 compliant |
| **LZH** | ✅ | ✅ | lh0-lh7, lzs, lz4, lz5, pm0 | CRC-16 | Shift_JIS support, all header levels; pm1/pm2 not implemented |
| **XZ** | ✅ | ✅ | LZMA2 | CRC-64 | Block checksums |
| **7z** | ✅ | ❌ | LZMA/LZMA2 | CRC-32 | Read-only, partial support |
| **CAB** | ✅ | ❌ | None, MSZIP | CFDATA checksums | Microsoft Cabinet, read-only; MSZIP window carried across CFDATA blocks, per-block checksums validated; Quantum/LZX unsupported (clean error, never silent raw copy) |
| **LZ4** | ✅ | ✅ | LZ4, LZ4-HC | XXHash32 | Frame format, block/content checksums |
| **Zstd** | ✅ | ✅ | Zstandard | XXHash64 | RFC 8878 frame format; full decoder (FSE + 1/4-stream Huffman); encoder: Huffman literals + predefined/RLE FSE sequences (custom sequence tables not emitted — ratio, not correctness) |
| **Bzip2** | ✅ | ✅ | BWT + Huffman | CRC-32 | Block-sorting compression |
| **Brotli** | ✅ | ✅ | Brotli (RFC 7932) | None | Quality levels 0-11, full Appendix A static dictionary; `.br` file-path CLI support via extension fallback (raw Brotli has no magic bytes) |
| **Snappy** | ✅ | ✅ | Snappy | CRC32C | Block and framed formats |
| **ISO 9660** | ✅ | ❌ | Store | None | Read-only; list/extract/info/detect support |
| **PNG** | ✅ | ✅ | DEFLATE (zlib) | CRC-32 (per chunk) | ISO/IEC 15948; every colour type/depth, Adam7, APNG, Apple `CgBI` (lenient default, strict rejects); `oxiarc-cli detect`/`info` recognise it (not an archive format) |
| **JPEG** | ✅ | ✅ | DCT + Huffman/arithmetic | None (entropy-coded) | ITU-T T.81; baseline/extended/progressive/lossless, arithmetic (SOF9-11, default-on), OJPEG; scaled decode (`Scale` 1-16/8); `oxiarc-cli detect`/`info` recognise it |
| **TIFF** | ✅ | ✅ | None/PackBits/CCITT/LZW/Deflate/ZSTD/LZMA/JPEG | None (per-strip codec checksums where applicable) | TIFF 6.0 + BigTIFF; classic + BigTIFF, both byte orders, `rayon` parallel strips/tiles; `oxiarc-cli detect`/`info` recognise it |

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
| `oxiarc-lzw` | `z-oracle` | `compress`/`uncompress`/`gzip` (`.Z` container) |
| `oxiarc-lzw` | `gif-oracle` | `python3` + Pillow (GIF) |
| `oxiarc-szip` | `libaec-oracle` | libaec (compiled harness; `LIBAEC_PREFIX` env var) |
| `oxiarc-lzhuf` | `lha-oracle` | `lha` (Lhasa) CLI |
| `oxiarc-archive` | `zip-oracle`, `xz-oracle`, `lha-oracle` | Info-ZIP `zip`/`unzip` + Python `zipfile`; `xz`; `lha` |
| `oxiarc-http` | `http-oracle` | `python3` (zlib/gzip) + `brotli`/`zstd`/`compress`/`uncompress`/`gzip` CLIs |
| `oxiarc-png` | `png-oracle` | `python3` + Pillow, both directions |
| `oxiarc-jpeg` | `jpeg-oracle` | `cjpeg`/`djpeg`/`tjbench` (libjpeg-turbo), Pillow tolerance cross-check |
| `oxiarc-tiff` | `tiff-oracle` (own, per-crate feature — unrelated to `oxiarc-lzw`'s feature of the same name; Cargo features are crate-namespaced) | `tiffcp`/`tiffinfo` (libtiff), `python3` + Pillow/`tifffile` |

```bash
# Run one codec's live oracle against the reference tool
cargo nextest run -p oxiarc-zstd --features zstd-oracle
cargo nextest run -p oxiarc-brotli --features brotli-oracle

# Archive-level oracles
cargo nextest run -p oxiarc-archive --features zip-oracle,xz-oracle,lha-oracle

# Everything, everywhere (oracle suites self-skip for any missing tool)
cargo nextest run --workspace --all-features
```

Verified interop snapshot (last full run 2026-07-13, live tools; unchanged in 0.4.0 — this release's DEFLATE/zlib decoder rewrite changed no wire format or output, so the DEFLATE/zlib/gzip result below still holds): zstd 64/64 corpus + 101/101 wide frames decode byte-identical, 85/85 oxiarc frames accepted by `zstd -d`; brotli 608/608 decode / 588/588 accepted; xz 5.8.3 60/60 decode / 8/8 encode; bzip2 1.0.8 324/324 both directions; lz4 1.10.0 11/11 + 44/44 + 3/3 linked; TIFF-LZW 125/125 vs Pillow/libtiff (encoder byte-identical to libtiff); libaec 1.1.4 2450/2450 decode + 4900/4900 encode; DEFLATE/zlib/gzip bit-exact vs CPython + gzip CLI.

**0.4.2 additions** (each new format crate ships its own live oracle from its first wave, not bolted on afterwards): `oxiarc-brotli` shared dictionaries **72/72** reference `-D` streams decode byte-identical, **96/96** oxiarc streams accepted by `brotli -d -D`; `oxiarc-lzw`'s new `.Z` container **64/64** reference `compress` streams decode byte-identical, **64/64** oxiarc streams byte-identical to `compress -b N -c`, **160/160** decode via `gzip -dc`/`uncompress -c`, and GIF (previously only ever round-tripped against itself) is now **20/20** Pillow-written GIFs decode byte-identical + **20/20** oxiarc GIFs read back correctly in Pillow; `oxiarc-jpeg` baseline/progressive output is byte-identical to `cjpeg`/`djpeg -dct int` including 12-bit, optimized Huffman and arithmetic coding (SOF9/10), with scaled decode (`M` in `{1,2,4,8}`) also byte-identical and the other twelve `M` values within a measured numeric tolerance (peak error 3); `oxiarc-png` decodes and encodes both directions against Pillow across every colour type/depth/interlacing combination; `oxiarc-tiff` round-trips every codec (LZW/Deflate/ZSTD/LZMA/JPEG/PackBits/CCITT G3/G4, incl. the new uncompressed mode, which libtiff can *parse* but not decode — `tiffinfo` reports it correctly, `tiffcp` reports "not supported") against `tiffcp`/`tiffinfo`/Pillow/`tifffile`; `oxiarc-zstd`'s legacy one-shot decode path is now hardened to match the streaming `ZstdStream` decoder on every RFC-forbidden frame shape a 983,701-execution differential fuzz campaign could find. Per-crate READMEs carry the exact counts and dates.

## Performance

### Benchmark Results

Real-world performance measured on various data types.

> **Every codec figure in this repository was re-measured on 2026-09-08**
> (interleaved arms, medians, three independent runs wherever a row straddles
> its gate, machine load average quoted next to every table — it ranged from
> 110 down to 18 on 8 cores during that window, so **the ratios are the
> result and the absolute times are not portable**). Headline movements, all
> against the reference tool for that codec:
>
> | matrix | before | 2026-09-08 |
> |---|---|---|
> | `oxiarc-tiff` whole-image vs `tiffcp -c none`, ZSTD | 5.07x-6.24x | **0.96x-1.65x** |
> | same, LZW | 2.13x-2.41x | **1.00x-1.07x** |
> | same, Deflate | 1.39x-1.58x | **1.14x-1.33x** |
> | same, LZMA | 1.15x-1.16x | 1.31x-1.32x (moved the wrong way) |
> | `oxiarc-zstd` vs `zstd -b -d`, 50 MB text L3 | 0.43x-0.55x | 0.43x-0.54x (confirmed) |
> | `oxiarc-deflate` vs Apple's tuned libz, PNG rows @ 16 MiB | 0.51x-0.60x | 0.47x-0.67x (confirmed) |
> | `oxiarc-brotli` push vs one-shot, copy-dense @ lgwin 22 | 0.80x | 0.71x-0.76x |
> | `oxiarc-lzw` vs libtiff `LZWDecode` | 1.25x-1.37x | 1.00x-1.56x (load scatter) |
>
> Four rows of the TIFF matrix crossed the `<= 1.25x` gate into "met" (both
> LZW rows, RGB8 Deflate, Gray16 ZSTD) and one crossed out of it (LZMA). The
> per-crate READMEs carry the full tables, the fixtures and the method; root
> `TODO.md` Known Issue 11 carries the gate bookkeeping.

The figures below predate that pass and cover the compression *encoders*,
which it did not re-measure:

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

# Run all tests (5,155 via nextest + 346 doctests = 5,501)
cargo nextest run --workspace --all-features
cargo test --doc --workspace --all-features

# Build CLI only
cargo build --release -p oxiarc-cli

# Install CLI
cargo install --path oxiarc-cli
```

## Requirements

- Rust 1.85+ (Edition 2024)
- No external C libraries or compression dependencies for the shipped libraries (default features are 100% Pure Rust — see Known Issue #8 in `TODO.md` for the one dev-only exception, `criterion`'s benchmark harness)
- Optional: `indicatif` for progress bars (CLI only)
- Optional, for the live oracle suites only (never required to build or use the library — every oracle feature self-skips when its tool is absent): `zstd`, `brotli`, `xz`, `bzip2`, `lz4`, `lha` (Lhasa) CLIs; Info-ZIP `zip`/`unzip`; libaec; `python3` with `zlib`/`gzip` (stdlib), Pillow 12.1, `numpy`, `tifffile`, and `cramjam`; libtiff's `tiffcp`/`tiffinfo`; libjpeg-turbo's `cjpeg`/`djpeg`/`tjbench`

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
