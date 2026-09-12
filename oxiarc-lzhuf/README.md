# oxiarc-lzhuf [Stable]

Pure Rust implementation of LZH (LZSS + Huffman) compression.

![Version](https://img.shields.io/badge/version-0.4.3-blue)
![License](https://img.shields.io/badge/license-Apache--2.0-green)
![Status](https://img.shields.io/badge/status-Stable-brightgreen)

**Version 0.4.3** (2026-09-08) — 274 tests passing (269 via nextest + 5 doctests, `--all-features`).

**What's new in 0.3.0**: 4-byte multiplicative hash for better avalanche and fewer collisions; `LzssOptimalParser` — two-pass optimal LZSS parser with Huffman-cost retraining; `LzhEncoder::with_optimal()` builder; custom dictionary support via `LzhEncoder::with_dictionary`, `LzhDecoder::with_dictionary`, `LzssEncoder::preload_dictionary`, and `LzssDecoder::preload_dictionary`.

**What's new in 0.3.1**: Custom dictionary support — `LzhEncoder::with_dictionary(method, dict)` and `set_dictionary(&mut self, dict)` allow seeding the encoder with a known prefix corpus; `LzhDecoder::with_dictionary(method, size, dict)` and `set_dictionary` mirror the interface for the decoder; `LzssEncoder::preload_dictionary` and `LzssDecoder::preload_dictionary` seed hash chains and ring buffer from the dict tail, improving compression ratio when encoder and decoder share a known corpus prefix.

**What's new in 0.3.5**: The `-lh4-`/`-lh5-`/`-lh6-`/`-lh7-` codec now speaks genuine canonical LHA wire format instead of the previous private, self-consistent-only bitstream — MSB-first bit order (via the new `oxiarc-core::msb_bitstream` module), corrected code-table length encoding, pt-tree zero-run handling, and position/offset encoding. Validated against 6 real third-party `.lzh` archives (`tests/data/`, sourced from the `fragglet/lhasa` corpus) and a live `lha` (Lhasa) CLI oracle behind the new opt-in `lha-oracle` feature. The streaming decoder was rewritten to match, and a `parallel` header-size bug that made real LHA tools report zero entries in parallel-built archives is fixed.

**What's new in 0.3.6**: Fixed `LzssDecoder::new` panicking on a non-power-of-two or zero window size — it now rounds up to the next power of two (minimum 16), mirroring `LzssEncoder::new`'s normalization. Fixed a decompression-bomb DoS in the `-lh1-` decoder: a malformed/truncated stream paired with a large declared output size could previously loop indefinitely, fabricating zero-padding output; the bit reader now flags end-of-input exhaustion and `decode_lh1` returns an error as soon as decoding would read past the real compressed data. `#[must_use]` added to `ParallelLzhBuilder::with_num_threads`. New corrupt-input (`tests/corrupt_input.rs`: bit-flip and heavy multi-byte-corruption fuzzing) and `proptest`-based round-trip (`tests/proptest_roundtrip.rs`) test suites.

## Overview

LZH is the compression algorithm used in LHA/LZH archives. It was particularly popular in Japan during the BBS era and is still used in some embedded systems and legacy applications.

This crate implements the core compression algorithm, separate from the archive container format (handled by `oxiarc-archive`).

## Features

- **Pure Rust** - No C bindings or unsafe code
- **Multiple methods** - lh0, lh1, lh4, lh5, lh6, lh7
- **Real LHA interoperability** - lh4/lh5/lh6/lh7 speak genuine canonical LHA wire format (MSB-first), validated against real-world `.lzh` archives and a live `lha` CLI oracle
- **Dual Huffman trees** - Codes + Offsets
- **Configurable window sizes** - 4KB to 64KB
- **Streaming and one-shot APIs**
- **4-byte multiplicative hash** - Better avalanche effect, fewer collisions
- **Optimal LZSS parser** - `LzssOptimalParser` with two-pass parsing and Huffman-cost retraining
- **Optimal encoder builder** - `LzhEncoder::with_optimal()` for maximum compression
- **Custom dictionary support** - `LzhEncoder::with_dictionary`, `LzhDecoder::with_dictionary`, `LzssEncoder::preload_dictionary`, `LzssDecoder::preload_dictionary`

All features are implemented and tested. API is stable.

## Quick Start

```rust
use oxiarc_lzhuf::{LzhMethod, LzhEncoder, LzhDecoder, encode_lzh, decode_lzh};

// One-shot compression
let original = b"Hello, World! Hello, World!";
let compressed = encode_lzh(original, LzhMethod::Lh5)?;

// One-shot decompression
let decompressed = decode_lzh(&compressed, LzhMethod::Lh5, original.len() as u64)?;
assert_eq!(&decompressed, original);
```

## Compression Methods

| Method | Window | Max Length | Huffman | Description |
|--------|--------|------------|---------|-------------|
| lh0 | - | - | None | Stored (no compression) |
| lh1 | 4 KB | 60 | Adaptive | LZHUF (LHarc 1.x legacy format) |
| lh4 | 4 KB | 256 | Static | Legacy, rarely used |
| lh5 | 8 KB | 256 | Static | Most common method |
| lh6 | 32 KB | 256 | Static | Better compression |
| lh7 | 64 KB | 256 | Static | Best compression |

## Algorithm Details

### LZSS (Lempel-Ziv-Storer-Szymanski)

LZSS is a variant of LZ77 that only outputs a (length, distance) pair when it saves space:

```
For each position:
  If match found AND match_len >= threshold:
    Output: FLAG(1) + LENGTH + DISTANCE
  Else:
    Output: FLAG(0) + LITERAL_BYTE
```

Parameters by method:
- **Threshold**: 3 bytes (match must be at least 3 bytes to encode)
- **Max length**: 256 bytes
- **Window size**: Varies by method (4KB-64KB)

### Huffman Coding

LZH uses two separate Huffman trees:

**CODES Tree (Literals + Lengths)**:
- Symbols 0-255: Literal bytes
- Symbols 256-511: Match lengths (encoded as length - 3)

**OFFSETS Tree (Distances)**:
- Encodes the high bits of match distances
- Low bits are stored directly

### Block Structure

```
+------------------+
| CODES tree       | (Huffman tree for literals/lengths)
+------------------+
| OFFSETS tree     | (Huffman tree for distances)
+------------------+
| Compressed data  | (Huffman-coded LZSS tokens)
+------------------+
```

## API Reference

### Methods

```rust
use oxiarc_lzhuf::LzhMethod;

let method = LzhMethod::Lh5;
println!("Window size: {} bytes", method.window_size());
println!("Position bits: {}", method.position_bits());
```

### Encoder

```rust
use oxiarc_lzhuf::{LzhEncoder, LzhMethod};

let mut encoder = LzhEncoder::new(LzhMethod::Lh5);
let mut compressed = Vec::new();
encoder.encode(data, &mut compressed, true)?;
```

### Decoder

```rust
use oxiarc_lzhuf::LzhDecoder;
use std::io::Cursor;

let mut decoder = LzhDecoder::new(LzhMethod::Lh5, uncompressed_size);
// `decode` takes a `Read` source, not a byte slice directly.
let mut cursor = Cursor::new(&compressed);
let decompressed = decoder.decode(&mut cursor)?;
```

### LZSS Layer

```rust
use oxiarc_lzhuf::{LzssEncoder, LzssDecoder, LzssToken};

// Low-level LZSS encoding (window_size, min_match, max_match — lh5-equivalent)
let mut encoder = LzssEncoder::new(8192, 3, 256);
let tokens: Vec<LzssToken> = encoder.encode(data);

for token in &tokens {
    match token {
        LzssToken::Literal(byte) => println!("Literal: {:02x}", byte),
        LzssToken::Match { length, distance } => {
            println!("Match: len={}, dist={}", length, distance);
        }
    }
}
```

### Huffman Trees

```rust
use oxiarc_lzhuf::LzhHuffmanTree;
use oxiarc_core::MsbBitReader;
use std::io::Cursor;

// `code_lengths` is a per-symbol Huffman code length table; `tree_capacity`
// bounds the internal flat tree array (2x the symbol count is sufficient).
let tree = LzhHuffmanTree::from_code_lengths(&lengths, lengths.len() * 2)?;

let mut bit_reader = MsbBitReader::new(Cursor::new(compressed_bits));
let symbol = tree.decode(&mut bit_reader)?;
```

## Modules

| Module | Description |
|--------|-------------|
| `methods` | Method definitions (lh0-lh7) |
| `lzss` | LZSS encoder/decoder |
| `huffman` | LZH Huffman tree operations |
| `encode` | High-level encoder |
| `decode` | High-level decoder |

## Usage

Add to your `Cargo.toml`:

```toml
[dependencies]
oxiarc-lzhuf = "0.4.3"
```

## Compatibility

The `-lh4-`/`-lh5-`/`-lh6-`/`-lh7-` codec speaks genuine canonical LHA wire format — MSB-first bit order, matching the reference `LHa for UNIX` bit I/O, code-table length encoding, and position/offset encoding — not merely a self-consistent private format. This is validated two ways:

- **Decode direction**: byte-exact decoding of a real-world corpus of `.lzh` archives produced by independent LHA-family tools (`LHa for UNIX 1.14i`, `LHA 2.55e` for DOS) across header levels 0/1/2, including a 1.24 MB multi-block archive (see `tests/data/README.md` for full provenance).
- **Encode direction**: archives produced by this crate are verified readable by a live `lha` (Lhasa 0.6.0) CLI oracle (`lha t` / `lha x`), gated behind the opt-in `lha-oracle` Cargo feature.

This implementation is compatible with:
- LHA for UNIX (lha)
- LHa for Windows
- 7-Zip (extraction)
- p7zip (extraction)

## Historical Context

LZH was created by Haruyasu Yoshizaki in 1988 for the LHA archiver. It became the dominant archive format in Japan, particularly on:
- PC-98 computers
- MS-DOS BBS systems
- Japanese video game distribution

The format declined in the 2000s as ZIP became universal, but remains important for:
- Retro computing
- Embedded systems with limited resources
- Legacy data recovery

## References

- [LHA Header Documentation](https://github.com/jca02266/lha/blob/master/header.doc.md)
- [Kaitai Struct LZH](https://formats.kaitai.io/lzh/)
- Original LHA source code

## License

Apache-2.0
