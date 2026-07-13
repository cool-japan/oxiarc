
# oxiarc-core [Stable]

Core primitives and traits for the OxiArc archive library.

![Version](https://img.shields.io/badge/version-0.3.6-blue)
![License](https://img.shields.io/badge/license-Apache--2.0-green)
![Status](https://img.shields.io/badge/status-Stable-brightgreen)

**Version 0.3.6** (2026-07-13) — 187 tests passing.

**What's new in 0.3.6**: Non-panicking `RingBuffer::try_new`/`OutputRingBuffer::try_new` constructors alongside the existing panicking `new` methods (now with documented `# Panics` contracts) — prefer the fallible form when a window/capacity size originates from untrusted input. Fixed `Crc32::is_simd_available()`/`Crc32::implementation_name()` to report the CRC-32 code path actually dispatched at runtime (previously x86_64 could misreport PCLMULQDQ while dispatch had silently fallen back to software). `FlushMode`, `CompressStatus`, `DecompressStatus`, and `OxiArcError` are now `#[non_exhaustive]` as part of a pre-1.0 API freeze — downstream `match` expressions need a wildcard arm. Removed the unused `CompressionLevel(u8)` newtype (dead code; every codec crate already defines its own, differently-ranged level type). `Compressor`/`Decompressor` trait docs now correctly describe them as optional, DEFLATE-family-only traits rather than a universal contract. New `mmap_read` example.

**What's new in 0.3.5**: Added `msb_bitstream` — `MsbBitReader`/`MsbBitWriter`, genuine most-significant-bit-first bit I/O for canonical LZH/LHA-family bitstream work (mirrors canonical LHA `getbits`/`putbits`/`fillbuf` semantics), a sibling to the existing LSB-first `BitReader`/`BitWriter` used by DEFLATE. Re-exported from the crate root and `prelude`.

**What's new in 0.3.0**: Added `MappedFile` — a zero-copy memory-mapped file primitive backed by `memmap2` (enable the `mmap` feature). SIMD CRC-32 acceleration is now auto-enabled at compile time via `cfg(target_arch)` and no longer requires the `simd` feature flag; the flag is now a deprecated no-op.

**What's new in 0.2.8**: Hardware-accelerated CRC-32 via aarch64 PMULL instructions is now automatically enabled on compatible hardware with no feature flag required. Added `ProgressSink` and `CancellationToken` types for progress reporting and cooperative cancellation in long-running operations. The `simd` feature flag is now a deprecated no-op.


## Features

- **BitStream** - Bit-level I/O for variable-length codes
- **MsbBitStream** - MSB-first bit-level I/O for canonical LZH/LHA-family codecs
- **RingBuffer** - Sliding window buffer for LZ77/LZSS
- **CRC** - CRC-32 and CRC-16 checksums
- **Traits** - Core traits for compression/decompression
- **Entry** - Archive entry metadata
- **Error** - Unified error types

All features are implemented and tested. API is stable.

## Modules

### bitstream

LSB-first bit packing with efficient u64 internal buffers.

```rust
use oxiarc_core::bitstream::{BitReader, BitWriter};
use std::io::Cursor;

// Read bits
let data = vec![0xAB, 0xCD];
let mut reader = BitReader::new(Cursor::new(data));
let bits = reader.read_bits(12)?; // Read 12 bits

// Write bits
let mut buffer = Vec::new();
let mut writer = BitWriter::new(&mut buffer);
writer.write_bits(0x1F, 5)?;  // Write 5 bits
writer.write_bits(0xABC, 12)?; // Write 12 bits
writer.flush()?;
```

Key features:
- Generic over `Read`/`Write` traits
- `read_bits(count)` / `write_bits(value, count)`
- Byte alignment with `align_to_byte()`
- Peek ahead without consuming

### msb_bitstream

MSB-first bit-level I/O for canonical LZH/LHA-family bitstream work — the most-significant-bit-first sibling of `bitstream`, for codecs (the classic `lha`/`LHarc` family, including this crate's `-lh5-` support) that pack bits opposite to DEFLATE's LSB-first order.

```rust
use oxiarc_core::msb_bitstream::{MsbBitReader, MsbBitWriter};
use std::io::Cursor;

// Write bits, MSB-first (canonical LZH/LHA bit order)
let mut output = Vec::new();
let mut writer = MsbBitWriter::new(&mut output);
writer.put_bits(3, 0b101)?; // emits 1, 0, 1
writer.put_bits(1, 0b1)?;   // emits 1
writer.flush()?;           // zero-pads the final byte

// Read them back
let mut reader = MsbBitReader::new(Cursor::new(&output));
let a = reader.get_bits(3)?; // 0b101
let b = reader.get_bits(1)?; // 0b1
```

Key features:
- Mirrors canonical LHA `getbits`/`putbits`/`fillbuf` bit semantics exactly
- `get_bits(count)` / `put_bits(count, value)`, plus single-bit `get_bit()` / `put_bit()`
- `peek_bits(count)` without consuming, paired with `skip_bits(count)` — the canonical Huffman-decode idiom (peek a max-width code, look it up, skip the matched length)
- Zero-pads past end-of-input instead of erroring, matching LHA's `fillbuf`; `padding_bits()` reports how much was synthesized so callers can detect a truncated stream
- `bits_read()` / `bits_written()` logical bit counters

### ringbuffer

Sliding window buffer for dictionary-based compression.

```rust
use oxiarc_core::ringbuffer::{RingBuffer, OutputRingBuffer};

// Basic ring buffer
let mut rb = RingBuffer::new(32768); // 32KB window
rb.write_byte(b'A');
let byte = rb.read_at_distance(1)?; // Read the most recently written byte

// Output ring buffer with copy-from-history
let mut out = OutputRingBuffer::new(32768);
out.write_literal(b'H');
out.write_literal(b'i');
out.copy_match(2, 4)?; // Copy "Hi" twice -> "HiHiHi"
assert_eq!(out.output(), b"HiHiHi");
```

Configurable sizes for different algorithms:
- 4KB (lh4)
- 8KB (lh5)
- 32KB (Deflate, lh6)
- 64KB (lh7)

Both `RingBuffer::new`/`OutputRingBuffer::new` panic on a zero or
non-power-of-two capacity (see their documented `# Panics` sections); the
non-panicking `RingBuffer::try_new`/`OutputRingBuffer::try_new` counterparts
return `Result` instead and should be preferred whenever the capacity comes
from untrusted or externally supplied input.

### crc

CRC checksum implementations.

```rust
use oxiarc_core::crc::{Crc32, Crc16};

// CRC-32 (ZIP/GZIP)
let crc = Crc32::compute(b"Hello, World!");
assert_eq!(crc, 0xEC4AC3D0);

// Incremental CRC-32
let mut crc32 = Crc32::new();
crc32.update(b"Hello, ");
crc32.update(b"World!");
let result = crc32.finalize();

// CRC-16/ARC (LZH)
let crc16 = Crc16::compute(b"data");
```

### traits

Core traits for streaming compression.

```rust
use oxiarc_core::traits::{Compressor, Decompressor, DecompressStatus};

// Decompressor trait
pub trait Decompressor {
    fn decompress(&mut self, input: &[u8], output: &mut [u8])
        -> Result<(usize, usize, DecompressStatus)>;
    fn reset(&mut self);
    fn is_finished(&self) -> bool;
    fn decompress_all(&mut self, input: &[u8]) -> Result<Vec<u8>>;
}

// Archive reader trait
pub trait ArchiveReader {
    fn entries(&mut self) -> Result<Vec<Entry>>;
    fn extract<W: Write>(&mut self, entry: &Entry, writer: &mut W) -> Result<u64>;
}
```

### entry

Archive entry metadata.

```rust
use oxiarc_core::entry::{Entry, EntryType, CompressionMethod};
use std::time::SystemTime;

let entry = Entry {
    name: "file.txt".to_string(),
    size: 1234,
    compressed_size: 567,
    entry_type: EntryType::File,
    method: CompressionMethod::Deflate,
    modified: Some(SystemTime::now()),
    crc32: Some(0xABCD1234),
    ..Default::default()
};

println!("Space savings: {:.1}%", entry.space_savings());
```

### error

Unified error types using thiserror.

```rust
use oxiarc_core::error::{OxiArcError, Result};

// A sample of error variants (OxiArcError is #[non_exhaustive]; match
// expressions need a wildcard arm)
OxiArcError::Io(io_error)
OxiArcError::InvalidMagic { expected, found }
OxiArcError::UnsupportedMethod { method }
OxiArcError::CrcMismatch { expected, computed }
OxiArcError::InvalidHuffmanCode { bit_position }
OxiArcError::CorruptedData { offset, message }
OxiArcError::InvalidHeader { message }
```

## Features

| Feature | Default | Description |
|---------|---------|-------------|
| `default` | no | Core primitives with no extra dependencies |
| `async-io` | no | Async I/O support via Tokio (`AsyncRead`/`AsyncWrite`) |
| `simd` | no | Deprecated no-op — SIMD CRC-32 (aarch64 PMULL) is now auto-enabled at compile time with no feature flag required |
| `mmap` | no | Memory-mapped file access for efficient large file processing (memmap2) |
| `serde` | no | Serde serialization/deserialization support for core types |

## Usage

Add to your `Cargo.toml`:

```toml
[dependencies]
oxiarc-core = "0.3.6"
```

Or with optional features:

```toml
[dependencies]
oxiarc-core = { version = "0.3.6", features = ["async-io", "mmap"] }
```

## API Summary

| Module | Key Types |
|--------|-----------|
| `bitstream` | `BitReader<R>`, `BitWriter<W>` |
| `msb_bitstream` | `MsbBitReader<R>`, `MsbBitWriter<W>` |
| `ringbuffer` | `RingBuffer`, `OutputRingBuffer` |
| `crc` | `Crc32`, `Crc16` |
| `traits` | `Compressor`, `Decompressor`, `ArchiveReader`, `ArchiveWriter` |
| `entry` | `Entry`, `EntryType`, `CompressionMethod`, `FileAttributes` |
| `error` | `OxiArcError`, `Result<T>` |

## Prelude

For convenient imports:

```rust
use oxiarc_core::prelude::*;
```

## License

Apache-2.0
