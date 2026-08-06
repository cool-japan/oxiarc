
# oxiarc-szip [Stable]

Pure Rust implementation of CCSDS-121.0-B-2 / libaec-compatible AEC (Adaptive Entropy Coding) / SZIP compression.

![Version](https://img.shields.io/badge/version-0.4.2-blue)
![License](https://img.shields.io/badge/license-Apache--2.0-green)
![Status](https://img.shields.io/badge/status-Stable-brightgreen)

**Version 0.4.2** (2026-08-06) — 47 tests passing.

**What's new in 0.3.6**:

- **libaec interoperability fixed and differentially verified (both directions).** The bit-stream framing now follows CCSDS-121.0-B-2 §5.2 / libaec exactly: the option ID precedes *all* `pixels_per_block` samples of an RSI's first block (the reference sample is sample #0 of that block, present only when `nn_preprocess` is on); fundamental-sequence codewords are zero-run/one-terminated; sample-split blocks group all FS quotients before the k-bit remainders; zero-block runs (incl. ROS and 64-block segments) and the second-extension option are fully decoded; the NN preprocessor uses the standard theta-clamped mapping; `id_len`/`k_max` match the unrestricted CCSDS option space; and blocks are always whole (trailing partial blocks are padded/discarded). Verified against libaec 1.1.4: 888/888 reference-encoded streams decode byte-identical, 1776/1776 oxiarc-encoded streams are accepted byte-identical by the reference decoder (`tests/libaec_interop.rs` embeds reference fixtures; a live gate is available behind the `libaec-oracle` feature).
- **`pixels_per_block: 64` supported** (full CCSDS/libaec block-size range).
- **Typed validation errors** instead of panics/silent corruption: short encode buffers → `InputTooShort`; out-of-range sample values → new `SampleOutOfRange`; `encode_bytes` inputs with a trailing partial sample → `InvalidParam`; out-of-spec sample-split option IDs (`k > k_max()`) → `InvalidBlockOption`; over-long zero-block runs → `LengthMismatch`.

- **`SzipError` is now `#[non_exhaustive]`** (pre-1.0 API-stability freeze), and every struct-variant field gained a doc comment (satisfying a newly-enabled `#![warn(missing_docs)]` lint).
- **`SzipParams` gained a `Default` impl** (`bits_per_pixel: 8, pixels_per_block: 8, samples: 0, reference_sample_interval: 8, msb: true, nn_preprocess: false, rsi_byte_align: false`) and now derives `PartialEq`/`Eq`.
- New `sample_decode` example.
- New `proptest`-based round-trip suite (`tests/proptest_roundtrip.rs`) and a corrupt-input regression suite (`tests/corrupt_input.rs`) covering truncated/empty/bit-flipped streams — malformed input now reliably returns `Err` instead of panicking.

**What's new in 0.3.2**: Initial release of `oxiarc-szip`. Implements the full AEC/SZIP encode/decode pipeline as specified in CCSDS-121.0-B-2 and compatible with the `libaec` reference library. Supports configurable `bits_per_pixel` (1–32), `pixels_per_block` (8/16/32), `reference_sample_interval`, MSB/LSB bit ordering, NN preprocessing (unit-delay predictor), and RSI byte alignment. Exposed via `encode`, `encode_bytes`, and `decode` free functions together with `SzipParams` and `SzipError` public types.

## Overview

AEC (Adaptive Entropy Coding) is a lossless compression algorithm standardised by the Consultative Committee for Space Data Systems (CCSDS) in document CCSDS-121.0-B-2. It is the algorithm underlying the SZIP compression filter — the same algorithm originally developed by NASA and widely deployed in the scientific computing world.

AEC combines:
- Golomb-Rice entropy coding with an adaptive code-option selection per block
- A no-compression fallback option for incompressible blocks
- An optional unit-delay NN (Nearest-Neighbour) predictor preprocessing step
- RSI (Reference Sample Interval) partitioning so that decoding can restart at regular boundaries

It is used in:
- HDF5 datasets (SZIP filter, filter ID 4)
- NetCDF-4 files (which use HDF5 internally)
- CCSDS satellite telemetry archives
- Scientific instrument data products from NASA, ESA, JAXA, and others
- FITS image files with SZIP/AEC compression


## Features

- **Pure Rust** — No C bindings, no unsafe blocks beyond those needed for bit-level arithmetic; fully safe public API
- **CCSDS-121.0-B-2 compliant** — Faithful implementation of the standard; byte-for-byte compatible with `libaec`
- **`encode`** — Compress a `&[u64]` sample array into an AEC/SZIP byte stream (no-compression option; suitable for round-trip testing and standard-compliant stream generation)
- **`encode_bytes`** — Convenience wrapper: converts raw `&[u8]` input to samples and calls `encode`
- **`decode`** — Decompress an AEC/SZIP byte stream back to raw sample bytes
- **`SzipParams`** — Strongly-typed parameter struct covering all knobs required by the standard
- **`SzipParams::default()`** — common 8-bit settings (`bits_per_pixel: 8, pixels_per_block: 8, reference_sample_interval: 8, msb: true`); combine with struct-update syntax (`SzipParams { samples: N, ..SzipParams::default() }`) to only override what differs
- **Configurable `bits_per_pixel`** — Supports 1–32 bits per sample; common values are 8, 16, and 32
- **Configurable `pixels_per_block`** — Supports block sizes of 8, 16, 32, or 64 samples (J parameter)
- **Configurable `reference_sample_interval`** — Controls restart-point frequency in the bit stream
- **MSB / LSB bit ordering** — `msb: true` selects the most-common hardware/CHIPS mode; `msb: false` selects LSB-first
- **NN preprocessing** — `nn_preprocess: true` enables the unit-delay NN predictor; decoder automatically applies the inverse transform
- **RSI byte alignment** — `rsi_byte_align: true` pads each RSI to the next byte boundary (required for some hardware AEC implementations and the HDF5 `AEC_CHIP_OPTION` flag)
- **Typed errors** — `SzipError` enum with structured fields for all failure modes

All features are implemented and tested. API is stable.

## Quick Start

```rust
use oxiarc_szip::{SzipParams, decode, encode};

let params = SzipParams {
    samples: 16,
    ..SzipParams::default()
};

let samples: Vec<u64> = (0..16u64).collect();
let compressed = encode(&samples, &params).unwrap();
let raw_bytes  = decode(&compressed, &params).unwrap();

// Verify that the decoded bytes round-trip correctly.
let decoded: Vec<u64> = raw_bytes.iter().map(|&b| b as u64).collect();
assert_eq!(decoded, samples);
```

Add to your `Cargo.toml`:

```toml
[dependencies]
oxiarc-szip = "0.4.2"
```

## API Reference

### Free Functions

#### `decode`

```rust
pub fn decode(input: &[u8], params: &SzipParams) -> Result<Vec<u8>, SzipError>
```

Decompress an AEC/SZIP compressed byte slice into raw sample bytes. The returned `Vec<u8>` contains `params.samples * params.bytes_per_sample()` bytes in the natural byte order implied by `params.bits_per_pixel`.

#### `encode`

```rust
pub fn encode(samples: &[u64], params: &SzipParams) -> Result<Vec<u8>, SzipError>
```

Compress a slice of `u64` sample values into an AEC/SZIP bit stream. Always uses the no-compression block option, making the output a standards-compliant AEC stream that any conforming decoder (including `libaec`) can read. Primarily intended for round-trip testing and stream generation.

#### `encode_bytes`

```rust
pub fn encode_bytes(input: &[u8], params: &SzipParams) -> Result<Vec<u8>, SzipError>
```

Convenience wrapper around `encode`. Packs the raw bytes in `input` into `u64` samples using the big-endian layout implied by `params.bits_per_pixel` and then delegates to `encode`.

### Types

#### `SzipParams`

```rust
#[derive(Debug, Clone)]
pub struct SzipParams {
    /// Bits encoded per sample (1–32). Common values: 8, 16, 32.
    pub bits_per_pixel: u8,

    /// Number of samples per coding block (J). Must be 8, 16, or 32.
    pub pixels_per_block: u32,

    /// Total number of samples to decode.
    pub samples: usize,

    /// Reference sample interval in samples.
    /// Setting this to 0 treats the entire stream as a single RSI.
    pub reference_sample_interval: u32,

    /// `true` = MSB-first (most common for hardware/CHIPS mode).
    /// `false` = LSB-first.
    pub msb: bool,

    /// `true` = samples have been preprocessed with the unit-delay NN predictor.
    /// The decoder automatically applies the inverse step.
    pub nn_preprocess: bool,

    /// `true` = each RSI boundary is byte-aligned in the bit stream.
    /// Required by some hardware AEC implementations and the HDF5 CHIP option.
    pub rsi_byte_align: bool,
}
```

Notable methods on `SzipParams`:

| Method | Description |
|--------|-------------|
| `validate()` | Returns `Err(SzipError::InvalidParam(...))` if any field is out of range |
| `id_len()` | Length in bits of the block option-ID field: 3 for bpp ≤ 8, 4 for bpp ≤ 16, 5 for bpp ≤ 32 (CCSDS unrestricted mode) |
| `k_max()` | Maximum Golomb-Rice `k` parameter: `min(bpp - 2, 2^id_len - 3)` |
| `id_no_compress()` | All-ones pattern of `id_len()` bits; signals a no-compression block |
| `xmax()` | Maximum representable sample value `(1 << bpp) - 1` |
| `bytes_per_sample()` | Number of output bytes per decoded sample |

#### `SzipError`

```rust
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SzipError {
    InputTooShort    { need: usize, have: usize },
    InvalidBlockOption { id: u32, bpp: u8 },
    InvalidParam     (&'static str),
    LengthMismatch   { expected: usize, actual: usize },
    UnsupportedOption { mask: u8 },
    SampleOutOfRange { index: usize, value: u64, max: u64 },
    UnexpectedEof    { offset: usize },
}
```

All variants carry structured fields so that error messages are machine-readable. The `Display` impl is derived from `thiserror`. As of 0.3.6 the enum is `#[non_exhaustive]` (pre-1.0 API-stability freeze): `match` expressions on `SzipError` must include a wildcard arm, since new variants may be added in a semver-compatible release.

## Use in OxiArc

`oxiarc-szip` is integrated into the OxiArc archive library. When reading or writing archives that contain SZIP-compressed entries (e.g., HDF5 files), `oxiarc-archive` uses this crate transparently via `CompressionMethod::Szip`.

```rust
use oxiarc_archive::{Archive, CompressionMethod};

// oxiarc-archive selects oxiarc-szip automatically for Szip entries.
let method = CompressionMethod::Szip;
```

No direct dependency on `oxiarc-szip` is required in application code that already depends on `oxiarc-archive`.

## Algorithm Details

### Block Structure

The AEC bit stream is partitioned into RSI (Reference Sample Interval) segments. Within each RSI the samples are subdivided into coding blocks of `pixels_per_block` samples. Each block begins with an option-ID field (`id_len()` bits) that selects the coding method for that block:

| Option ID | Coding Method |
|-----------|---------------|
| `0` + extension bit `0` | Zero-block run (FS-coded run length; a length of 5 means "remainder of segment") |
| `0` + extension bit `1` | Second-extension option (sample pairs coded as one FS codeword) |
| `1 ..= k_max() + 1` | Sample-split (Golomb-Rice) with `k = ID − 1`: FS-coded quotients for the whole block, then the grouped k-bit remainders |
| `id_no_compress()` (all ones) | No-compression (raw samples, `bits_per_pixel` each) |

When `nn_preprocess` is enabled, the first sample of each RSI is a verbatim reference sample: it is sample #0 of the RSI's first block and is read directly after that block's option ID (and after the low-entropy extension bit, where applicable). Blocks always contain `pixels_per_block` samples; a trailing partial block is padded by the encoder (repeating the last sample) and the padding is discarded by the decoder.

### NN Preprocessing

When `nn_preprocess` is `true`, a unit-delay predictor is applied before entropy coding. Each non-reference sample `x[i]` is replaced by the theta-clamped mapped residual of `Δ = x[i] - x[i-1]` per CCSDS-121.0-B-2 §4 (with `θ = min(x[i-1], xmax - x[i-1])`): `2Δ` for `0 ≤ Δ ≤ θ`, `2|Δ| - 1` for `-θ ≤ Δ < 0`, and `θ + |Δ|` otherwise, which guarantees every residual fits in `bits_per_pixel` bits. The decoder reverses this step after entropy decoding. Signed-sample preprocessing (libaec's `AEC_DATA_SIGNED`) is not implemented.

### MSB / LSB Bit Ordering

The `msb` flag controls whether bits are packed into bytes most-significant-first or least-significant-first. MSB-first is the CCSDS-121 / libaec bit ordering and is required for interoperability (it is what the HDF5 SZIP filter uses). LSB-first is a crate-local extension that only round-trips with this crate.

### Interoperability Verification

The framing is differentially tested against libaec 1.1.4 in both directions: reference-encoded streams must decode byte-identically, and oxiarc-encoded streams must be accepted byte-identically by the reference decoder. `tests/libaec_interop.rs` embeds 17 libaec-produced fixtures (covering zero-block runs/ROS, second-extension, sample-split at several `k`, no-compression, 1/4/8/13/16/32 bpp, block sizes 8–64, multi-RSI layouts, partial trailing blocks, and `AEC_PAD_RSI` alignment) so the gate always runs; enabling the `libaec-oracle` cargo feature additionally compiles a live harness against an installed libaec and sweeps a ~2 600-case matrix (self-skips when libaec or a C compiler is absent).

## Comparison with Other Codecs

| Codec | Ratio | Speed | Suited for |
|-------|-------|-------|------------|
| AEC/SZIP | Good on smooth integer data | Fast | Scientific raster data, satellite telemetry |
| DEFLATE | Good general-purpose | Fast | Text, mixed binary |
| LZMA | Excellent | Slow | Archives, installers |
| Zstd | Very Good | Very Fast | General-purpose streaming |

AEC/SZIP is specifically optimised for uniformly sampled integer data (e.g., sensor readings, image pixels) where adjacent samples are highly correlated, which is common in scientific and earth-observation datasets.

## References

- [CCSDS 121.0-B-2 — Lossless Data Compression](https://public.ccsds.org/Pubs/121x0b2.pdf)
- [libaec — Adaptive Entropy Coding library (reference implementation)](https://gitlab.dkrz.de/k202009/libaec)
- [HDF5 SZIP Filter documentation](https://support.hdfgroup.org/documentation/hdf5/latest/group___d_a_p_l.html)

## License

Apache-2.0

Copyright © COOLJAPAN OU (Team Kitasan). Repository: <https://github.com/cool-japan/oxiarc>
