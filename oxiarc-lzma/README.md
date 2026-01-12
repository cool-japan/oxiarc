# oxiarc-lzma

Pure Rust implementation of LZMA (Lempel-Ziv-Markov chain Algorithm) compression.

## Overview

LZMA is a high-ratio compression algorithm that combines:
- LZ77-style dictionary compression
- Range coding for entropy encoding
- Context-dependent probability models

It's used in:
- 7-Zip archives (.7z)
- XZ compressed files (.xz)
- LZMA SDK (.lzma)
- Some ZIP archives (method 14)

## Features

- **Pure Rust** - No C bindings, fully safe code
- **Compression and Decompression** - Full roundtrip support
- **Configurable levels** - 0-9 compression levels
- **Streaming API** - Memory-efficient processing
- **Range Coder** - Precise 11-bit probability model

## Quick Start

```rust
use oxiarc_lzma::{compress, decompress_bytes, LzmaLevel};

// Compress with default level
let data = b"Hello, World! Hello, World!";
let compressed = compress(data, LzmaLevel::DEFAULT)?;

// Decompress
let decompressed = decompress_bytes(&compressed)?;
assert_eq!(&decompressed, data);
```

## Compression Levels

| Level | Dictionary | Use Case |
|-------|------------|----------|
| 0 | 64 KB | Fastest, minimal compression |
| 1 | 256 KB | Fast compression |
| 2 | 512 KB | Fast compression |
| 3 | 1 MB | Balanced |
| 4 | 2 MB | Balanced |
| 5 | 4 MB | Balanced |
| 6 | 8 MB | Default, good ratio |
| 7 | 16 MB | Better ratio |
| 8 | 32 MB | High ratio |
| 9 | 64 MB | Maximum ratio |

## Algorithm Details

### LZMA Stream Format

```
+------------------+
| Properties (1B)  | lc, lp, pb encoded
+------------------+
| Dict Size (4B)   | Little-endian
+------------------+
| Uncomp Size (8B) | Little-endian, 0xFF...FF = unknown
+------------------+
| Compressed Data  | Range-coded LZMA
+------------------+
```

### Properties Encoding

The properties byte encodes three parameters:
- **lc** (literal context bits): 0-8, default 3
- **lp** (literal position bits): 0-4, default 0
- **pb** (position bits): 0-4, default 2

```
properties = (pb * 5 + lp) * 9 + lc
```

### Range Coding

LZMA uses range coding with:
- 11-bit probability model (2048 = 50%)
- Normalization threshold: 2^24
- 64-bit accumulator with carry handling
- Adaptive probability updates: `prob += (target - prob) >> 5`

### State Machine

LZMA has 12 states representing recent history:

| State | After Literal | After Match | After Rep |
|-------|--------------|-------------|-----------|
| 0 | 0 | 7 | 8 |
| 1 | 0 | 7 | 8 |
| ... | ... | ... | ... |
| 11 | 0 | 10 | 11 |

### Match Types

1. **Literal** - Single byte, context-dependent encoding
2. **Match** - New distance + length
3. **Rep0** - Repeat at distance rep[0]
4. **Rep1** - Repeat at distance rep[1], swap with rep[0]
5. **Rep2** - Repeat at distance rep[2], shift down
6. **Rep3** - Repeat at distance rep[3], shift down
7. **ShortRep** - Single byte at rep[0]

### Probability Models

| Model | Purpose | Size |
|-------|---------|------|
| is_match | Literal vs match | 12 * num_pos_states |
| is_rep | Match vs rep | 12 |
| is_rep0 | Rep0 vs rep1/2/3 | 12 |
| is_rep0_long | Long rep0 vs short | 12 * num_pos_states |
| is_rep1 | Rep1 vs rep2/3 | 12 |
| is_rep2 | Rep2 vs rep3 | 12 |
| literal | Literal bytes | 768 * num_lit_states |
| match_len | Match lengths | varies |
| rep_len | Rep lengths | varies |
| distance | Distance slots | 4 * 64 |

## API Reference

### Compression

```rust
use oxiarc_lzma::{compress, compress_raw, LzmaEncoder, LzmaLevel};

// One-shot compression
let compressed = compress(data, LzmaLevel::DEFAULT)?;

// Raw compression (no header)
let raw = compress_raw(data, LzmaLevel::DEFAULT)?;

// Streaming encoder
let mut encoder = LzmaEncoder::new(LzmaLevel::DEFAULT);
let compressed = encoder.compress(data)?;
```

### Decompression

```rust
use oxiarc_lzma::{decompress, decompress_bytes, decompress_raw, LzmaDecoder};
use std::io::Cursor;

// From reader (with header)
let decompressed = decompress(Cursor::new(compressed))?;

// From bytes
let decompressed = decompress_bytes(&compressed)?;

// Raw decompression (no header)
let props = LzmaProperties::new(3, 0, 2);
let decompressed = decompress_raw(reader, props, dict_size, Some(uncompressed_size))?;
```

### Properties

```rust
use oxiarc_lzma::LzmaProperties;

let props = LzmaProperties::new(3, 0, 2);  // lc=3, lp=0, pb=2
let byte = props.to_byte();  // 0x5D
let decoded = LzmaProperties::from_byte(byte)?;
```

### Range Coder

```rust
use oxiarc_lzma::{RangeEncoder, RangeDecoder};

// Encoder
let mut encoder = RangeEncoder::new();
encoder.encode_bit(&mut prob, bit);
encoder.encode_direct_bits(value, num_bits);
let output = encoder.finish();

// Decoder
let mut decoder = RangeDecoder::new(reader)?;
let bit = decoder.decode_bit(&mut prob)?;
let value = decoder.decode_direct_bits(num_bits)?;
```

## Modules

| Module | Description |
|--------|-------------|
| `encoder` | LZMA compression |
| `decoder` | LZMA decompression |
| `model` | Probability models |
| `range_coder` | Range encoder/decoder |

## Comparison with Other Codecs

| Codec | Ratio | Speed | Memory |
|-------|-------|-------|--------|
| DEFLATE | Good | Fast | Low |
| LZMA | Excellent | Slow | High |
| Zstd | Very Good | Fast | Medium |
| BZip2 | Very Good | Medium | Medium |

## References

- [LZMA SDK](https://www.7-zip.org/sdk.html)
- [XZ Embedded](https://tukaani.org/xz/embedded.html)
- [LZMA specification (informal)](https://github.com/jljusten/LZMA-SDK/blob/master/DOC/lzma-specification.txt)

## License

MIT OR Apache-2.0
