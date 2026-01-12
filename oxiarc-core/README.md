# oxiarc-core

Core primitives and traits for the OxiArc archive library.

## Overview

This crate provides the fundamental building blocks for archive and compression operations:

- **BitStream** - Bit-level I/O for variable-length codes
- **RingBuffer** - Sliding window buffer for LZ77/LZSS
- **CRC** - CRC-32 and CRC-16 checksums
- **Traits** - Core traits for compression/decompression
- **Entry** - Archive entry metadata
- **Error** - Unified error types

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

### ringbuffer

Sliding window buffer for dictionary-based compression.

```rust
use oxiarc_core::ringbuffer::{RingBuffer, OutputRingBuffer};

// Basic ring buffer
let mut rb = RingBuffer::new(32768); // 32KB window
rb.push(b'A');
let byte = rb.get(-1); // Get last byte

// Output ring buffer with copy-from-self
let mut out = OutputRingBuffer::new(32768);
out.put_byte(b'H');
out.put_byte(b'i');
out.copy_from_self(2, 4); // Copy "Hi" twice -> "HiHiHi"
```

Configurable sizes for different algorithms:
- 4KB (lh4)
- 8KB (lh5)
- 32KB (Deflate, lh6)
- 64KB (lh7)

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

let entry = Entry {
    name: "file.txt".to_string(),
    size: 1234,
    compressed_size: 567,
    entry_type: EntryType::File,
    method: CompressionMethod::Deflate,
    mtime: Some(1704067200),
    crc32: Some(0xABCD1234),
    ..Default::default()
};

println!("Space savings: {:.1}%", entry.space_savings());
```

### error

Unified error types using thiserror.

```rust
use oxiarc_core::error::{OxiArcError, Result};

// Error variants
OxiArcError::Io(io_error)
OxiArcError::InvalidMagic { expected, found }
OxiArcError::UnsupportedMethod(method_name)
OxiArcError::CrcMismatch { expected, computed }
OxiArcError::InvalidHuffmanCode
OxiArcError::Corrupted { offset, message }
OxiArcError::InvalidHeader(message)
```

## Usage

Add to your `Cargo.toml`:

```toml
[dependencies]
oxiarc-core = { path = "../oxiarc-core" }
```

## API Summary

| Module | Key Types |
|--------|-----------|
| `bitstream` | `BitReader<R>`, `BitWriter<W>` |
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

MIT OR Apache-2.0
