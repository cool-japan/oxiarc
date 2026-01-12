# oxiarc-deflate

Pure Rust implementation of the DEFLATE compression algorithm (RFC 1951).

## Overview

DEFLATE is the compression algorithm used in:
- ZIP archives
- GZIP compressed files
- PNG images
- HTTP compression
- Many other formats

This crate provides both compression and decompression with no external dependencies.

## Features

- **Pure Rust** - No C bindings or unsafe code
- **Full RFC 1951 compliance** - All block types supported
- **Compression levels 0-9** - From stored to maximum compression
- **Streaming API** - Process data in chunks
- **One-shot API** - Convenient functions for simple cases

## Quick Start

```rust
use oxiarc_deflate::{deflate, inflate};

// Compress data
let original = b"Hello, World! Hello, World! Hello, World!";
let compressed = deflate(original, 6)?;  // Level 6 (default)

// Decompress data
let decompressed = inflate(&compressed)?;
assert_eq!(&decompressed, original);
```

## Compression Levels

| Level | Description | Use Case |
|-------|-------------|----------|
| 0 | Stored (no compression) | Already compressed data |
| 1-3 | Fast compression | Real-time streaming |
| 4-6 | Balanced (default: 6) | General purpose |
| 7-9 | Best compression | Archival, storage |

## API

### High-Level Functions

```rust
// Compress with specified level
let compressed = deflate(data, level)?;

// Decompress
let decompressed = inflate(&compressed)?;
```

### Streaming API

```rust
use oxiarc_deflate::{Deflater, Inflater};

// Streaming compression
let mut deflater = Deflater::new(6);
let compressed = deflater.compress_all(data)?;

// Streaming decompression
let mut inflater = Inflater::new();
loop {
    let (consumed, produced, status) = inflater.decompress(input, output)?;
    // Handle status...
}
```

### LZ77 Encoder

```rust
use oxiarc_deflate::{Lz77Encoder, Lz77Token};

let mut encoder = Lz77Encoder::new(6);
for token in encoder.encode(data) {
    match token {
        Lz77Token::Literal(byte) => { /* literal byte */ }
        Lz77Token::Match { length, distance } => { /* back-reference */ }
    }
}
```

### Huffman Trees

```rust
use oxiarc_deflate::{HuffmanTree, HuffmanBuilder};

// Build tree from code lengths
let lengths = [3, 3, 3, 3, 3, 2, 4, 4];
let tree = HuffmanTree::from_lengths(&lengths)?;

// Decode symbols
let symbol = tree.decode(&mut bit_reader)?;
```

## Algorithm Details

### DEFLATE Structure

```
+------------------+
| Block Header     | (3 bits: BFINAL + BTYPE)
+------------------+
| Block Data       | (varies by type)
+------------------+
| ... more blocks  |
+------------------+
```

### Block Types

| BTYPE | Name | Description |
|-------|------|-------------|
| 00 | Stored | Uncompressed data (up to 65535 bytes) |
| 01 | Fixed | Fixed Huffman codes (RFC 1951 Table) |
| 10 | Dynamic | Custom Huffman codes in header |
| 11 | Reserved | Invalid |

### LZ77 Parameters

- **Window size**: 32768 bytes (32KB)
- **Match length**: 3-258 bytes
- **Match distance**: 1-32768 bytes
- **Minimum match**: 3 bytes

### Huffman Alphabets

**Literal/Length (286 symbols)**:
- 0-255: Literal bytes
- 256: End of block
- 257-285: Length codes (3-258)

**Distance (30 symbols)**:
- 0-29: Distance codes (1-32768)

### Fixed Huffman Code Lengths

| Range | Code Length |
|-------|-------------|
| 0-143 | 8 bits |
| 144-255 | 9 bits |
| 256-279 | 7 bits |
| 280-287 | 8 bits |

## Modules

| Module | Description |
|--------|-------------|
| `deflate` | Compression (encoder) |
| `inflate` | Decompression (decoder) |
| `huffman` | Huffman tree operations |
| `lz77` | LZ77 dictionary encoder |
| `tables` | Fixed Huffman tables, length/distance extra bits |

## Performance

Compression ratios on typical data (Calgary Corpus):

| File | Original | Compressed | Ratio |
|------|----------|------------|-------|
| book1 | 768771 | ~300000 | ~61% |
| paper1 | 53161 | ~18000 | ~66% |
| progc | 39611 | ~13000 | ~67% |

## References

- [RFC 1951 - DEFLATE Compressed Data Format Specification](https://www.rfc-editor.org/rfc/rfc1951)
- [An Explanation of the Deflate Algorithm](https://zlib.net/feldspar.html)

## License

MIT OR Apache-2.0
