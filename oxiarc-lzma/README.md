
# oxiarc-lzma [Stable]

Pure Rust implementation of LZMA (Lempel-Ziv-Markov chain Algorithm) compression.

![Version](https://img.shields.io/badge/version-0.3.1-blue)
![License](https://img.shields.io/badge/license-Apache--2.0-green)
![Status](https://img.shields.io/badge/status-Stable-brightgreen)

**Version 0.3.1** (2026-05-30) — 139 tests passing.

**What's new in 0.3.1**: Custom dictionary support via `LzmaEncoder::with_dictionary(level, dict_size, dict)` / `set_dictionary` and `LzmaDecoder::with_dictionary(reader, props, dict_size, dict)` / `set_dictionary`; thread-safe memory pool `LzmaPool` with `PooledBuf<'a>` RAII wrapper and `LzmaDecoderPooled<'p, R>` for amortizing large dict buffer allocations.

**What's new in 0.3.0**: `Bt4MatchFinder` — BT4 binary tree match finder with 3-table hash (h2/h3/h4), level 9 now uses BT4 for superior compression quality; `MatchFinder` trait abstracting both `HashChainMatchFinder` (levels 0–8) and `Bt4MatchFinder` (level 9).

**What's new in 0.2.8**: `with_progress(Arc<dyn ProgressSink>)` and `with_cancel(CancellationToken)` builder methods on `Lzma2Encoder`, `Lzma2Decoder`, and `Lzma2ChunkedEncoder` for progress reporting and cooperative cancellation.

**What's new in 0.2.6**: Encoder improvements including probability model refinements and optimal parsing enhancements for better compression ratios on structured data.

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
- **Progress reporting** - `with_progress(Arc<dyn ProgressSink>)` builder on LZMA2 codecs
- **Cooperative cancellation** - `with_cancel(CancellationToken)` builder on LZMA2 codecs
- **BT4 match finder** - `Bt4MatchFinder` with 3-table hash (h2/h3/h4); level 9 uses BT4 for superior compression
- **Match finder trait** - `MatchFinder` abstracting `HashChainMatchFinder` (levels 0–8) and `Bt4MatchFinder` (level 9)
- **Custom dictionary** - `LzmaEncoder::with_dictionary` and `LzmaDecoder::with_dictionary`
- **Memory pool** - `LzmaPool`, `PooledBuf`, `LzmaDecoderPooled` for allocation-efficient workloads
- **Parallel LZMA2** - `lzma2_compress_parallel` free function and `ParallelLzma2Encoder` builder for multi-threaded LZMA2 compression; output is a valid LZMA2 stream decodable by `Lzma2Decoder` (requires `features = ["parallel"]`)

All features are implemented and tested. API is stable.

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

### Parallel LZMA2 Compression

Requires `features = ["parallel"]`. The output is a valid LZMA2 stream that can be decoded by `Lzma2Decoder`. Default chunk size is 1 MiB; note that compression ratio may be slightly lower than serial for small inputs because there is no cross-chunk dictionary.

```rust
use oxiarc_lzma::{lzma2_compress_parallel, Lzma2Decoder, ParallelLzma2Encoder};

let data = b"Hello, LZMA2 parallel! ".repeat(10_000);

// Free-function API (level, chunk_size, num_threads)
let compressed = lzma2_compress_parallel(&data, 6, 1024 * 1024, None)?;

// Decoder roundtrip
let decompressed = Lzma2Decoder::new().decode(&compressed)?;
assert_eq!(&decompressed, data.as_ref());

// Builder API
let compressed = ParallelLzma2Encoder::new()
    .level(6)
    .chunk_size(512 * 1024)
    .encode(&data)?;

let decompressed = Lzma2Decoder::new().decode(&compressed)?;
assert_eq!(&decompressed, data.as_ref());
```

## Features (Cargo)

| Feature | Default | Description |
|---------|---------|-------------|
| `parallel` | no | Multi-threaded LZMA2 compression via Rayon |

```toml
[dependencies]
# Default (serial only)
oxiarc-lzma = "0.3.1"

# With parallel LZMA2 compression
oxiarc-lzma = { version = "0.3.1", features = ["parallel"] }
```

## Usage

Add to your `Cargo.toml`:

```toml
[dependencies]
oxiarc-lzma = "0.3.1"
```

## Modules

| Module | Description |
|--------|-------------|
| `encoder` | LZMA compression |
| `decoder` | LZMA decompression |
| `model` | Context-dependent probability models |
| `optimal` | Optimal parsing for improved compression decisions |
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

Apache-2.0
