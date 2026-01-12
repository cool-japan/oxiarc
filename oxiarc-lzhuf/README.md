# oxiarc-lzhuf

Pure Rust implementation of LZH (LZSS + Huffman) compression.

## Overview

LZH is the compression algorithm used in LHA/LZH archives. It was particularly popular in Japan during the BBS era and is still used in some embedded systems and legacy applications.

This crate implements the core compression algorithm, separate from the archive container format (handled by `oxiarc-archive`).

## Features

- **Pure Rust** - No C bindings or unsafe code
- **Multiple methods** - lh0, lh4, lh5, lh6, lh7
- **Dual Huffman trees** - Codes + Offsets
- **Configurable window sizes** - 4KB to 64KB
- **Streaming and one-shot APIs**

## Quick Start

```rust
use oxiarc_lzhuf::{LzhMethod, LzhEncoder, LzhDecoder, encode_lzh, decode_lzh};

// One-shot compression
let original = b"Hello, World! Hello, World!";
let compressed = encode_lzh(original, LzhMethod::Lh5)?;

// One-shot decompression
let decompressed = decode_lzh(&compressed, LzhMethod::Lh5, original.len())?;
assert_eq!(&decompressed, original);
```

## Compression Methods

| Method | Window | Max Length | Huffman | Description |
|--------|--------|------------|---------|-------------|
| lh0 | - | - | None | Stored (no compression) |
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
use oxiarc_lzhuf::LzhEncoder;

let encoder = LzhEncoder::new(LzhMethod::Lh5);
let compressed = encoder.encode(data)?;
```

### Decoder

```rust
use oxiarc_lzhuf::LzhDecoder;

let mut decoder = LzhDecoder::new(LzhMethod::Lh5, uncompressed_size);
let decompressed = decoder.decode(&compressed)?;
```

### LZSS Layer

```rust
use oxiarc_lzhuf::{LzssEncoder, LzssDecoder, LzssToken};

// Low-level LZSS encoding
let mut encoder = LzssEncoder::new(LzhMethod::Lh5);
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

let tree = LzhHuffmanTree::from_code_lengths(&lengths)?;
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

## Compatibility

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

MIT OR Apache-2.0
