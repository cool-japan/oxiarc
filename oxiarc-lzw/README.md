
# oxiarc-lzw [Stable]

Pure Rust implementation of LZW (Lempel-Ziv-Welch) compression for TIFF and GIF formats.

[![Crates.io](https://img.shields.io/crates/v/oxiarc-lzw.svg)](https://crates.io/crates/oxiarc-lzw)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)
![Status](https://img.shields.io/badge/status-Stable-brightgreen)

**Version: 0.4.2 (2026-09-08) | 254 tests passing: 233 via nextest (incl. libtiff/Pillow TIFF, Pillow GIF and `compress(1)` differential oracles, adversarial decode hardening and heap-budget suites) + 21 doctests**

## Overview

LZW is a dictionary-based compression algorithm used in TIFF images, GIF animations and the legacy UNIX `compress` (`.Z`) format. This implementation provides both bit packings — MSB-first (TIFF) and LSB-first (GIF, `.Z`) — selected explicitly by `LzwConfig::bit_order`, code widths from 9 to 16 bits, a dedicated GIF LZW codec (`gif_lzw`) and a complete `.Z` container codec (`z`) that is byte-identical to `compress(1)`.


## Features

- **Pure Rust** - No C dependencies or unsafe FFI
- **TIFF support** - MSB-first bit ordering for TIFF images
- **GIF support** - LSB-first bit ordering for GIF animations via `gif_lzw` module
- **GIF LZW codec** - Dedicated `gif_compress`/`gif_decompress` functions conforming to GIF spec §22
- **UNIX `compress` / `.Z`** - Complete container codec in the `z` module: `1F 9D` header, block mode, code widths 9-16, `ZReader`/`ZWriter` adapters. Output is byte-identical to `compress -b N -c` and is accepted by `gzip -dc`/`uncompress -c` (`Content-Encoding: compress`)
- **Explicit bit order** - `LzwConfig::bit_order` (`LzwBitOrder::{Msb, Lsb}`) drives every generic entry point, so `LzwConfig::GIF` really decodes LSB-first and `LzwConfig::TIFF_COMPAT_LSB` reads libtiff's pre-1993 `LZWDecodeCompat` strips
- **Configurable** - Adjustable code width (9-16 bits; 12 is the TIFF/GIF ceiling, 16 the `compress` one)
- **Early change** - Code width increases before table full
- **Zero-allocation strip decode** - `decompress_tiff_into(src, &mut dst)` expands codes straight into a caller-supplied buffer through a packed prefix/suffix code table; no allocation per decoded code, and none at all beyond the fixed table
- **Within 1.4x of libtiff** - the decoder is measured against libtiff 4.7.1's own `LZWDecode` on strips libtiff produced: **0.73-0.80x of its throughput**, i.e. 1.25x-1.37x of its decode time (see [Performance](#performance) and `examples/lzw_vs_libtiff.rs`). Re-measured 2026-09-08 at load average 56-66 the band widens to 1.00x-1.56x of libtiff's decode time; that is a noisier measurement rather than a slower decoder — rows that agree to +-0.01x on a quiet machine scattered by +-0.3x at that load — so the figure quoted here, taken on a quiet one, remains the better estimate
- **Old-style LZW** - `LzwConfig::TIFF_OLD_STYLE` decodes streams written with the standard (late) code-width change instead of TIFF's early change, i.e. writers that followed TIFF 6.0's pseudo-code literally
- **Reference interop** - TIFF-LZW streams are byte-compatible with libtiff/Pillow in both directions (differential-tested; see `tests/tiff_lzw_oracle.rs`, `tests/tiffcp_strip_decode.rs` and the pinned fixtures in `tests/data/`), and GIF image data is checked against Pillow's own GIF decoder in both directions, including every minimum code size 2-8 (`tests/gif_oracle.rs`, feature `gif-oracle`)
- **Property-tested** - `proptest`-based round-trip and no-panic fuzzing across arbitrary inputs, for the generic codec and for `.Z`

All features are implemented and tested. API is stable. `LzwConfig` implements `Default` (returning the TIFF preset), and `LzwError` is `#[non_exhaustive]` ahead of the crate's 1.0 release, so `match` expressions over it need a wildcard arm.

## Quick Start

```rust
use oxiarc_lzw::{compress, decompress, LzwConfig};

// TIFF-style compression (MSB-first)
let config = LzwConfig::TIFF;
let original = b"ABCABCABCABC";
let compressed = compress(original, config)?;
let decompressed = decompress(&compressed, original.len(), config)?;
assert_eq!(decompressed, original);
```

## Zero-allocation TIFF strip decoding (New in 0.4.2)

The code table is the classical prefix/suffix form (`prefix: u16`, `suffix: u8`,
`first: u8`, `length: u16` per code) shared by the encoder and the decoder, and
codes are expanded **backwards directly into the output**. The previous
representation (`Vec<Vec<u8>>` plus a `HashMap<Vec<u8>, u16>`) allocated once
per emitted code on decode and once per input byte on encode; both hot loops
are now allocation-free, and the decoder additionally copies a repeated string
from its earlier occurrence in the output rather than walking the prefix chain.

```rust
use oxiarc_lzw::{compress_tiff, decompress_tiff_into};

let strip = b"row0row0row1row1row2row2";
let compressed = compress_tiff(strip)?;

// The caller already knows the strip size from the image geometry.
let mut out = vec![0u8; strip.len()];
let written = decompress_tiff_into(&compressed, &mut out)?;
assert_eq!(&out[..written], strip);
```

Decoding stops as soon as `dst` is full (leftover input is ignored, matching
libtiff's `LZWDecode`), and a stream that runs out before `dst` is full is an
error rather than a silent truncation. A malicious stream can neither overrun
`dst` nor loop forever.

Measured with criterion (`benches/lzw_into_bench.rs`, which pins a copy of the
pre-0.4.2 per-code-`Vec` decoder as the `legacy_vec` baseline and asserts it
decodes to the same bytes before timing it). Two runs on the same machine under
different load gave absolute times that differed by up to 2x, so the table
reports the **ratio** of the two decoders measured within each run, which was
stable:

| strip content | `legacy_vec` -> `decompress_tiff_into` |
|---|---|
| image-like gradient | 7.7x - 16.5x |
| text | 5.6x - 11.9x |
| incompressible | 7.6x - 14.0x |
| long runs (uniform) | 7.2x - 11.3x |

A third, independent run (20 samples, 1.5 s measurement) put the worst case
at **7.25x** (256 KiB of text) and the best at **13.1x** (1 MiB image-like),
so the roadmap's ">= 3x" target holds with a wide margin on every shape.

Run it yourself with
`cargo bench -p oxiarc-lzw --bench lzw_into_bench -- tiff_strip_decode`.

### Old-style writers (no early code-width change)

Some encoders follow TIFF 6.0's own pseudo-code and grow the code width one
code later than libtiff does. `LzwConfig::TIFF_OLD_STYLE` decodes those.
Fall back to it **only when the standard rule returns an error**, and decode
the retry into a scratch buffer so a failed retry cannot destroy a good
result; cache the winning configuration per image so the retry costs one
strip, not every strip:

```rust
use oxiarc_lzw::{decompress_into, decompress_tiff_into, LzwConfig, Result};

fn decode_strip(strip: &[u8], out: &mut [u8]) -> Result<usize> {
    match decompress_tiff_into(strip, out) {
        Ok(n) => Ok(n),
        Err(standard_err) => {
            let mut scratch = vec![0u8; out.len()];
            match decompress_into(strip, &mut scratch, LzwConfig::TIFF_OLD_STYLE) {
                Ok(n) => {
                    out.copy_from_slice(&scratch);
                    Ok(n)
                }
                // Neither rule explains the strip: report the first failure.
                Err(_) => Err(standard_err),
            }
        }
    }
}
```

A **short** return (`n < out.len()`) is a short strip, not a reason to retry:
that is an ordinary outcome (libtiff warns and keeps the partial row), while
an old-style retry on such a strip fails and overwrites what was already
decoded. Note also that `TIFF_OLD_STYLE` is not libtiff's `LZWDecodeCompat`
variant, which additionally packs its codes LSB-first, and that an old-style
stream can occasionally fill the buffer under the standard rule with wrong
bytes (~2 % of random old-style strips); a caller that *knows* a file is
old-style should select the configuration outright.

## GIF LZW Codec (New in 0.2.4)

The `gif_lzw` module implements the GIF-specific variant of LZW as described in the GIF spec §22:
- LSB-first (Least Significant Bit) bit ordering
- Variable initial code size driven by `minimum_lzw_code_size` from the GIF header
- Clear code and End-of-Information (EOI) code
- Dictionary reset on overflow (max 4096 codes / 12-bit codes)

```rust
use oxiarc_lzw::gif_lzw::{gif_compress, gif_decompress};

// minimum_code_size must be 2..=11 (GIF spec §22)
let data = b"TOBEORNOTTOBEORTOBEORNOT";
let compressed = gif_compress(data, 8)?;
let decompressed = gif_decompress(&compressed, 8)?;
assert_eq!(decompressed.as_slice(), data.as_slice());
```

## Performance

The decoder is built to libtiff's shape, and measured against it: `tiffcp`
writes the LZW strips, and the same strips are decoded by both. Three-arm
interleaved rounds (pre-0.4.2-rewrite decoder, current decoder, libtiff
4.7.1's `LZWDecode`), 4096x4096 pages, medians of 7 rounds, `decompress_tiff_into`
into an exactly-sized buffer. Absolute times are machine- and
load-dependent (this run: load 10 on 8 cores); the ratios are the portable
part.

| payload | strip | libtiff | before | after | before / libtiff | **after / libtiff** | speedup |
|---|---|---|---|---|---|---|---|
| RGB8 rows | 60 KiB - 1 MiB | 117 ms | 233 ms | 156 ms | 1.99x | **1.34x** | 1.49x |
| Gray16 rows | 64 KiB - 1 MiB | 55 ms | 103 ms | 69 ms | 1.86x | **1.25x** | 1.48x |
| text | 64 KiB - 1 MiB | 23 ms | 38 ms | 30 ms | 1.62x | **1.28x** | 1.27x |
| incompressible | 64 KiB - 1 MiB | 30 ms | 107 ms | 41 ms | 3.56x | **1.36x** | 2.62x |

An independent sweep at load 7.3 and smaller runs at load 4.5-5.5 give the
same ratios to within 0.02. What matters on a busy box is measuring over
*many* interleaved rounds rather than a few: a single round scatters (0.91x
to 1.79x observed on individual rounds at load 26-36), but both the median
and the minimum over 15 interleaved rounds are stable to 0.01. A 15-round
re-measurement at load 26-36 reproduced the whole matrix — RGB8 1.33-1.34x,
Gray16 1.25x, text 1.27-1.28x, incompressible 1.35-1.37x, worst case
**1.367x** — and a 7-round run at load 46 gave a worst case of 1.41x. That
series continues: three 7-round runs on 2026-09-08 at **load 56-66** gave
per-row medians of 1.00x-1.56x (median of the three run medians per row), worst
case **1.56x**, with rows that agree to +-0.01x on a quiet machine scattering by
+-0.3x — `text r64` came out at 0.95x, 1.55x and 1.00x on the same bytes. The
band widens with load and its centre does not move, which is the point: quote an
estimator over many interleaved rounds, never a single round, and read the
quiet-machine figures above as the estimate of the code's cost.

The figures are flat across the strip-size sweep (60 KiB, 256 KiB, 1 MiB),
so building one code table per strip costs under 1.3 us. GIF image data
gained more, because `gif_decompress` used to allocate a `Vec<u8>` per
emitted code and now shares the strip decoder:

| GIF payload (1 MiB) | before | after | speedup |
|---|---|---|---|
| image-like | 14.4 ms | 1.14 ms | 12.6x |
| text | 4.87 ms | 0.84 ms | 5.8x |
| one repeated byte | 0.30 ms | 0.03 ms | 10.5x |
| incompressible | 67.7 ms | 2.80 ms | 24.2x |

What makes it fast, in the order the measurements said it mattered:

* **one packed `u64` per code-table entry** (prefix, length, first byte,
  last byte, "all bytes equal") - as a five-field struct the optimiser
  emitted four loads and five stores per code; packed, it is one `ldr` and
  one `str`,
* **a stateless code reader**: a code is shifted out of a four-byte window
  at an absolute bit position the decode loop keeps in a register, instead
  of a bit accumulator behind `&mut` that had to be written back per code,
* **libtiff's entry-creation order**, which turns the KwKwK case into a
  single comparison and knows every string's length before writing a byte,
* **a `repeated` bit per entry**, so the long single-byte runs of a flat
  image region are emitted with a fill instead of a chain walk,
* **a chain walk that stops one step above the root**, because every entry
  on a chain carries the same first byte: a three-byte string costs one
  table load, a two-byte string none.

The libtiff arm of the table above is a harness that links `libtiff` and
times `TIFFReadEncodedStrip` on a TIFF held entirely in memory, so nothing
but the codec is in the loop; it is not committed, because this workspace is
C-free. The committed example reproduces the same comparison with PATH tools
only — `tiffcp -c none lzw.tif out` minus `tiffcp -c none uncompressed.tif out`,
which differ only by the LZW decode. That subtraction is noisier (the two
runs also read different numbers of bytes) and needs a quiet machine: it
agrees with the direct harness to within ~15 % at a load of ~10 and is
meaningless on a busy box, which is why it prints `uptime` with its results.

Reproduce with `cargo run --release --example lzw_vs_libtiff` (self-skips
without `tiffcp`) and `cargo bench --bench lzw_into_bench`.

## Configuration

`LzwConfig` selects the clear-code/early-change semantics, the code-width
range (`min_bits` 9, `max_bits` 9-16) **and the bit order** for the generic
`compress`/`decompress`/`decompress_into`/`LzwEncoder`/`LzwDecoder` API. GIF's
variable minimum code size and sub-block framing are still handled separately
by the dedicated `gif_compress`/`gif_decompress` functions (or
`LzwStreamMode::Gif` in the streaming API) — see the GIF LZW Codec section
above.

| Preset | Bit order | Width rule |
|---|---|---|
| `LzwConfig::TIFF` | MSB-first | early change (libtiff/Pillow/GDAL) |
| `LzwConfig::TIFF_OLD_STYLE` | MSB-first | standard (late) change |
| `LzwConfig::TIFF_COMPAT_LSB` | LSB-first | standard change (libtiff `LZWDecodeCompat`) |
| `LzwConfig::GIF` | LSB-first | standard change |

### TIFF Mode

```rust
use oxiarc_lzw::LzwConfig;

let config = LzwConfig::TIFF;
// MSB-first bit ordering
// 9-12 bit codes
// TIFF 6.0 clear codes (strip starts with ClearCode 256; table resets at
// entry 4094) and early code change — libtiff/Pillow/GDAL-compatible
```

### GIF-flavored Mode (LSB-first since 0.4.2)

```rust
use oxiarc_lzw::{LzwBitOrder, LzwConfig};

let config = LzwConfig::GIF;
assert_eq!(config.bit_order, LzwBitOrder::Lsb);
// LSB-first bit ordering (GIF's packing, not TIFF's)
// 9-12 bit codes
// Uses a clear code; standard (non-early) code change
```

**Breaking behaviour change in 0.4.2:** before this release `LzwConfig` had no
`bit_order` field, so `LzwConfig::GIF` and `LzwConfig::TIFF_OLD_STYLE` were
*literally the same value* and `decompress(_, _, LzwConfig::GIF)` silently
decoded MSB-first. They are now different configurations that decode the same
bytes differently. Code that used `LzwConfig::GIF` to read an MSB-first
stream must switch to `LzwConfig::TIFF_OLD_STYLE`.

### Wide code widths

```rust
use oxiarc_lzw::{LzwBitOrder, LzwConfig};

let config = LzwConfig::new(9, 16)?.with_bit_order(LzwBitOrder::Lsb);
assert_eq!(config.max_code(), 65535);
```

Widths above 12 keep the dictionary growing where the TIFF/GIF ceiling would
force a reset; a 16-bit table costs about 640 KiB.

## UNIX `compress` / `.Z` (New in 0.4.2)

The `z` module is a complete codec for the container `compress(1)`,
`ncompress` and `gzip -Z` write, and the coding HTTP calls `compress` /
`x-compress` (RFC 9110 §8.4.1.1):

```rust
use std::io::{Read, Write};
use oxiarc_lzw::z::{ZHeader, ZReader, ZWriter, compress, decompress, decompress_with_limit};

let original = b"UNIX compress round trip. ".repeat(64);

// One-shot: `compress(data, max_bits)` is `compress -b max_bits`, byte for byte.
let stream = compress(&original, 16)?;
assert_eq!(&stream[..2], &[0x1F, 0x9D]);
assert_eq!(ZHeader::parse(&stream)?.max_bits, 16);
assert_eq!(decompress(&stream)?, original);

// Untrusted input: bound the output, enforced *while* decoding.
assert!(decompress_with_limit(&stream, 8).is_err());

// Streaming adapters
let mut writer = ZWriter::new(Vec::new(), 16)?;
writer.write_all(&original)?;
let stream = writer.finish()?;
let mut out = Vec::new();
ZReader::new(&stream[..]).with_max_output(1 << 20).read_to_end(&mut out)?;
assert_eq!(out, original);
```

Format notes that bite everyone who implements this once:

- **No end-of-information code.** A truncated `.Z` decodes to a *prefix* and
  returns `Ok` — the same thing `gzip -dc` does. Completeness has to come
  from the transport.
- **Codes are packed LSB-first in groups of eight.** On a width increase or a
  block-mode reset the writer pads the group and the reader skips the padding.
- **Block mode** (bit 7 of the third header byte, set by every `compress(1)`
  in circulation) makes code 256 a table reset; without it 256 is an ordinary
  entry. `compress_with_block_mode` writes either dialect.
- **The output is unbounded** unless you ask for a limit: use
  `decompress_with_limit`, `decompress_into` or `ZReader::with_max_output`.

| | one-shot | streaming |
|---|---|---|
| decode | `decompress`, `decompress_with_limit`, `decompress_into` | `ZReader` |
| encode | `compress`, `compress_with_block_mode` | `ZWriter` |

## API

### High-Level Functions

```rust
use oxiarc_lzw::{compress, decompress, LzwConfig};

let compressed = compress(data, LzwConfig::TIFF)?;
let decompressed = decompress(&compressed, data.len(), LzwConfig::TIFF)?;
```

### Encoder / Decoder (reusable dictionary state)

`LzwEncoder`/`LzwDecoder` own a resettable dictionary, but each `encode`/
`decode` call still processes one complete buffer (see "Streaming
Encoder/Decoder" below for incremental, chunk-at-a-time I/O):

```rust
use oxiarc_lzw::{LzwEncoder, LzwConfig};

let mut encoder = LzwEncoder::new(LzwConfig::TIFF)?;
let compressed = encoder.encode(data)?;
```

```rust
use oxiarc_lzw::{LzwDecoder, LzwConfig};

let mut decoder = LzwDecoder::new(LzwConfig::TIFF)?;
let decompressed = decoder.decode(&compressed, data.len())?;
```

### Streaming Encoder/Decoder (New in 0.2.6)

`LzwStreamEncoder`/`LzwStreamDecoder` implement `std::io::Write`/
`std::io::Read`; select TIFF or GIF framing via `LzwStreamMode`. Only the
**encoder** side is incremental: it flushes an independently-decompressible
frame once its internal buffer reaches the block size, so `write`/`write_all`
calls do not accumulate the whole input before producing output. The
**decoder** is not: `LzwStreamDecoder::read` eagerly reads the entire inner
reader to EOF and decompresses every frame on the first call, serving the
result from an internal buffer on subsequent calls — memory-efficient
framing on the wire, but not incremental decoding. This decoder also speaks
only the crate's own 8-byte-per-frame length-prefixed framing (what
`LzwStreamEncoder` writes), **not** a bare TIFF/GIF LZW bitstream; to decode
or encode a real TIFF strip directly, use the crate's `decompress_tiff_into`/
`compress_tiff` functions instead (see their rustdoc for the exact strip-level
contract):

```rust
use std::io::{Read, Write};
use oxiarc_lzw::{LzwStreamEncoder, LzwStreamDecoder, LzwStreamMode};

// Streaming encoder - TIFF framing
let mut encoder = LzwStreamEncoder::new(Vec::new(), LzwStreamMode::Tiff);
encoder.write_all(data)?;
let compressed = encoder.finish()?;

// Streaming decoder
let mut decoder = LzwStreamDecoder::new(&compressed[..], LzwStreamMode::Tiff);
let mut decompressed = Vec::new();
decoder.read_to_end(&mut decompressed)?;
assert_eq!(decompressed, data);
```

`LzwStreamMode::Config(LzwConfig)` (new in 0.4.2) carries an explicit bit
order and code width through the same adapters;
`LzwStreamMode::Config(LzwConfig::TIFF)` is byte-identical to
`LzwStreamMode::Tiff`. For `.Z` streams use `z::ZReader`/`z::ZWriter`, which
speak the real container rather than this crate's frame header.

## Algorithm

LZW builds a dictionary dynamically:
1. **Start with single-byte codes** (0-255)
2. **Add new patterns** to dictionary on-the-fly
3. **Variable-width codes** - Grows from 9 bits to `max_bits` (12 for
   TIFF/GIF, up to 16 for `.Z`)
4. **Table reset** - Clear dictionary when full (4096 entries at 12 bits,
   65536 at 16)

### Code Structure

| Code Range | Meaning |
|------------|---------|
| 0-255 | Literal bytes |
| 256 | Clear code (reset dictionary) |
| 257-4095 | Dictionary entries (257-65535 at `max_bits` 16) |

`.Z` differs: there is no EOI code, and code 256 is a table reset only when
the header's block-mode flag is set.

## Features (Cargo)

| Feature | Default | Description |
|---------|---------|-------------|
| `tiff-oracle` | off | Enables differential oracle tests (`tests/tiff_lzw_oracle.rs`) that validate TIFF-LZW interop against Pillow/libtiff in both directions; tests self-skip when `python3`+Pillow are absent. Test-only — the library compiles identically either way. |
| `z-oracle` | off | Enables the `.Z` differential oracle (`tests/z_oracle.rs`) against the system `compress`/`uncompress`/`gzip -dc`: real `compress -b N` output must decode byte-identically, and this crate's output must be byte-identical to (and accepted by) the reference tools. Self-skips when the tools are absent. Test-only. |

```toml
[dependencies]
oxiarc-lzw = "0.4.2"
```

## Use Cases

- **TIFF images** - LZW is one of the standard TIFF compression methods
- **GIF animations** - Original GIF compression format (including full GIF LZW codec)
- **Legacy data** - UNIX `.Z` files (`compress`/`uncompress`), read and written natively by the `z` module
- **HTTP** - `Content-Encoding: compress` / `x-compress` bodies

## Part of OxiArc

This crate is part of the [OxiArc](https://github.com/cool-japan/oxiarc) project - a Pure Rust archive/compression library ecosystem.

## License

Apache-2.0
