
# oxiarc-lzma [Stable]

Pure Rust implementation of LZMA (Lempel-Ziv-Markov chain Algorithm) compression.

![Version](https://img.shields.io/badge/version-0.4.2-blue)
![License](https://img.shields.io/badge/license-Apache--2.0-green)
![Status](https://img.shields.io/badge/status-Stable-brightgreen)

**Version 0.4.2** (2026-09-08) — 277 tests passing (+ 12 doctests, 14 with
`--all-features`).

**What's new in 0.4.2**:

- **A reusable decoder context, `xz::XzDecoder`.** `xz::decompress_into` is a
  thin one-shot wrapper: every call parses a fresh `XzReader`, which starts
  with no cached LZMA2 decoder and so rebuilds its dictionary buffer from
  nothing. `XzDecoder::new()` / `.with_max_output(u64)` /
  `.decompress_into(&mut self, src, dst)` / `.reset()` keeps that decoder
  (dictionary buffer, probability model, coder state) allocated across
  calls instead — the case this targets is TIFF `Compression = 34925`,
  where every strip or tile of one image is its own complete `.xz` stream
  and a large image can carry thousands of them, all sharing one dictionary
  size. A stream declaring a different dictionary size still decodes
  correctly; it just falls back to a fresh allocation, exactly like the
  free function always does. `xz::decompress_into` /
  `xz::decompress_with_limit` are unchanged, existing one-shot entry
  points — `XzDecoder` is additive, not a replacement. Reuse is proven
  byte-identical to always-fresh decoding (including across a dictionary
  *size mismatch* mid-sequence) before it is measured for speed; a
  criterion bench (`benches/xz_decoder_reuse.rs`, 1000 × 64 KiB same-shape
  streams) compares reuse against fresh-per-call — on an otherwise-idle
  machine this has measured as modestly faster (low single digits to
  ~18%), but run it yourself for a number you can trust on your own
  hardware rather than trusting a figure quoted here, since a build
  machine under concurrent load can easily swing the two comparisons in
  either direction. The LZMA2 probability model is reallocated fresh per
  block by the format itself regardless of reuse (an independent XZ block
  must reset its dictionary, state *and* properties), so only the
  dictionary buffer's allocation is actually avoided; the win is real but
  bounded at this payload size, and is reported honestly rather
  than engineered to look larger.
- **SHA-256 moved to `oxiarc_core::sha256`.** The `.xz` container's
  `CheckType::Sha256` block check used to carry its own private FIPS 180-4
  implementation; it now shares `oxiarc_core::sha256::Sha256` (`new`,
  `update`, `finalize`, one-shot `compute`) with the rest of the ecosystem
  — `oxiarc-http` can use the same implementation for the dictionary-hash
  matching RFC 9842 (Compression Dictionary Transport) defines, without
  depending on `oxiarc-lzma`. No wire-format or behavioural change; the
  bytes a `CheckType::Sha256` stream carries are identical to before.
- **New `oxiarc_lzma::xz` module — the `.xz` container lives here now.** The
  stream/block framing, index, and CRC-32 / CRC-64 / SHA-256 checks moved out
  of `oxiarc-archive/src/xz/`, which now re-exports this module unchanged
  (`oxiarc_archive::xz::{CheckType, XzReader, XzWriter, compress, decompress}`
  are the same paths, same behaviour). Image codecs can now read `.xz` without
  pulling in eight archive codecs — TIFF `Compression = 34925` stores a
  complete `.xz` stream per strip/tile.
- **`.xz` block filter chains are implemented, not ignored.** The reader used
  to parse the filter list only to find LZMA2's dictionary-size property and
  silently drop every other filter, which produced *silently wrong output*
  rather than an error. It now implements the Delta filter and all eight
  BCJ branch converters (x86, PowerPC, IA-64, ARM, ARM-Thumb, SPARC, ARM64
  and RISC-V), each validated byte-for-byte against liblzma and the `xz`
  CLI in **both** directions — encode and decode are compared separately,
  because a converter can round-trip its own output perfectly while
  disagreeing with the reference. Unknown filter IDs are a hard error.
  This is what makes libtiff's LZMA TIFFs (`Delta(dist=1) + LZMA2`) decode
  correctly.
- **The block check is verified after the filter chain**, per the xz spec —
  it covers the block's original data, not the LZMA2 output.
- **Multi-stream `.xz` files decode completely.** A `.xz` file is one or more
  streams with optional Stream Padding (what `cat a.xz b.xz` and parallel
  compressors produce); the reader used to stop after the first stream and
  return its bytes as a clean success — a silent short read. It now decodes
  every stream and concatenates them exactly as `xz -d` does (verified
  against `xz 5.8.3`, including its rule that trailing padding must be a
  multiple of four null bytes), and rejects trailing garbage instead of
  ignoring it.
- **Header fields that carry redundancy are cross-checked.** libtiff writes
  `LZMA_CHECK_NONE`, so on that path there is no checksum at all: a block's
  declared Uncompressed Size is now compared with what it decoded to, the
  index's record count with the number of blocks actually read, and the
  reserved block-header flag bits must be zero (all three were parsed and
  discarded before).
- **Every padding and checksum field of the container is now verified.**
  Block Padding and Index Padding must be null bytes (xz spec 3.4 / 4.4);
  Block Padding is covered by no checksum at all, and on a
  `LZMA_CHECK_NONE` stream — what libtiff writes — neither is the block, so
  accepting arbitrary bytes there meant accepting bytes nothing checked.
  The Stream Footer's own CRC-32 is verified (it was read and ignored), a
  declared Compressed Size of zero is rejected instead of falling through
  to the self-describing block path, and a block's LZMA2 payload must be
  consumed exactly by the decoder.
- **`XzWriter` writes multi-block streams.** A payload whose compressed form
  exceeded the reader's 100 MiB per-block limit used to produce a file this
  crate could not read back; input is now split into blocks of at most
  `with_block_size(...)` uncompressed bytes (64 MiB by default), one index
  record each. Streams that fit in one block are byte-identical to before.
- **Bounded decoding**: `xz::decompress_into(src, &mut dst)` decodes a complete
  `.xz` stream into a caller-sized buffer, and `xz::decompress_with_limit(data,
  max)` caps a growable decode. Both enforce the cap *during* decoding (after
  every LZMA2 chunk), so a decompression bomb is rejected before it is
  materialised; `XzReader::with_max_output(u64)` exposes the same guard.

**What's new in 0.3.6**:

- **Fixed a genuine LZMA2 multi-chunk encoder/decoder desync** that corrupted varied (non-repeated-byte) data spanning more than one chunk in the default `encode_lzma2`/`decode_lzma2` path above the 2 MiB chunk-size threshold. Every chunk (and sub-chunk) now resets the decoder's dictionary to match the fresh, history-less `LzmaEncoder` used for each chunk (previously only the first chunk reset the dictionary).
- **Dictionary allocation hardening**: dictionary size is now capped at 1.5 GiB (`DICT_SIZE_ALLOC_CAP`) and the backing buffer is grown lazily/incrementally as data is decoded, instead of eagerly zero-filled at the header-declared size — closing a memory-exhaustion risk from a crafted `.lzma`/`.xz` header.
- **XZ container validation**: the XZ index CRC-32 and footer Backward-Size field are now validated (previously unchecked), and any LZMA2 filter whose declared dictionary size exceeds the 1.5 GiB cap is rejected before a decoder is constructed.
- **`LzmaPool` poison recovery**: `acquire`/`release` now recover from a poisoned mutex (`unwrap_or_else` instead of `.lock().expect(...)`) instead of permanently disabling the pool for every other thread after one unrelated panic.
- New `lzma2_chunked` and `xz_compress` examples (`cargo run -p oxiarc-lzma --example lzma2_chunked`).
- **API surface narrowed** (pre-1.0): the `match_finder` and `model` modules are now private (`pub(crate)`). `Bt4MatchFinder`, `HashChainMatchFinder`, `MatchFinder`, `LzmaModel`, and `State` are no longer part of the public API — only `LzmaProperties` remains re-exported from `model`. `LzmaPool`/`PooledBuf`/`LzmaDecoderPooled` are still public but only via `oxiarc_lzma::memory_pool::*`; they are no longer re-exported at the crate root.
- Previously `ignore`-fenced doctests now compile and run as part of `cargo test`.

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
- **Streaming API** - `Lzma2StreamEncoder`/`Lzma2StreamDecoder` (`std::io::Write`/`Read`) genuinely stream LZMA2 chunk-at-a-time with bounded memory; `LzmaCompressor`/`LzmaDecompressor` (in `streaming.rs`) are a *different*, one-shot `&[u8] -> Vec<u8>` pair that only pre-flights a memory-budget estimate before running — see their doc comments before reaching for them expecting incremental I/O
- **Range Coder** - Precise 11-bit probability model
- **Progress reporting** - `with_progress(Arc<dyn ProgressSink>)` builder on LZMA2 codecs
- **Cooperative cancellation** - `with_cancel(CancellationToken)` builder on LZMA2 codecs
- **Advanced match finding (internal)** - level 9 uses a binary-tree match finder (3-table hash: h2/h3/h4) for superior compression; levels 0–8 use hash-chain matching. As of 0.3.6 these match-finder types are private implementation details, not part of the public API.
- **Custom dictionary** - `LzmaEncoder::with_dictionary` and `LzmaDecoder::with_dictionary`
- **Memory pool** - `oxiarc_lzma::memory_pool::{LzmaPool, PooledBuf, LzmaDecoderPooled}` for allocation-efficient workloads (not re-exported at the crate root)
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

// Builder-configured one-shot encoder (NOT streaming despite the type
// living alongside the codec's other builders: `compress` takes `self`
// and the whole input in one call, returning the whole output)
let mut encoder = LzmaEncoder::new(LzmaLevel::DEFAULT);
let compressed = encoder.compress(data)?;
// For genuine chunk-at-a-time I/O, use `Lzma2StreamEncoder`/`Lzma2StreamDecoder` instead.
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
oxiarc-lzma = "0.4.2"

# With parallel LZMA2 compression
oxiarc-lzma = { version = "0.4.2", features = ["parallel"] }
```

## Usage

Add to your `Cargo.toml`:

```toml
[dependencies]
oxiarc-lzma = "0.4.2"
```

## Modules

| Module | Description |
|--------|-------------|
| `encoder` | LZMA compression |
| `decoder` | LZMA decompression |
| `optimal` | Optimal parsing for improved compression decisions |
| `range_coder` | Range encoder/decoder |

Internal modules (`match_finder`, `model`, and others) are `pub(crate)` as of 0.3.6 and are not part of the public API surface.

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
