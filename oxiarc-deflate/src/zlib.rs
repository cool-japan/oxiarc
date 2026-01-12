//! Zlib format wrapper for DEFLATE compression.
//!
//! The zlib format (RFC 1950) wraps raw DEFLATE data with a header and
//! an Adler-32 checksum. It is widely used in PNG, HTTP compression, and
//! many other applications.
//!
//! # Format
//!
//! ```text
//! +---+---+============+---+---+---+---+
//! |CMF|FLG| compressed |    ADLER32    |
//! +---+---+============+---+---+---+---+
//! ```
//!
//! - CMF: Compression Method and Flags
//!   - Bits 0-3: CM (Compression Method) - must be 8 for DEFLATE
//!   - Bits 4-7: CINFO (Compression Info) - log2(window size) - 8
//! - FLG: Flags
//!   - Bits 0-4: FCHECK - check bits so (CMF*256 + FLG) mod 31 == 0
//!   - Bit 5: FDICT - preset dictionary present
//!   - Bits 6-7: FLEVEL - compression level (0-3)
//! - Compressed data (DEFLATE format)
//! - ADLER32: Adler-32 checksum of uncompressed data (big-endian)

use crate::deflate::deflate;
use crate::inflate::inflate;
use oxiarc_core::error::{OxiArcError, Result};

/// Zlib compression level indicator in header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ZlibLevel {
    /// Fastest compression.
    Fastest = 0,
    /// Fast compression.
    Fast = 1,
    /// Default compression.
    Default = 2,
    /// Maximum compression.
    Maximum = 3,
}

impl ZlibLevel {
    /// Convert from compression level (0-9) to zlib level indicator.
    fn from_level(level: u8) -> Self {
        match level {
            0..=2 => Self::Fastest,
            3..=5 => Self::Fast,
            6 => Self::Default,
            7..=9 => Self::Maximum,
            _ => Self::Default,
        }
    }
}

/// Adler-32 checksum calculator.
///
/// Adler-32 is a checksum algorithm designed by Mark Adler.
/// It is faster than CRC-32 but provides less protection against random errors.
#[derive(Clone, Debug)]
pub struct Adler32 {
    a: u32,
    b: u32,
}

/// Largest prime smaller than 65536.
const ADLER_MOD: u32 = 65521;

/// Number of bytes to process before reducing.
const NMAX: usize = 5552;

impl Adler32 {
    /// Create a new Adler-32 calculator.
    pub fn new() -> Self {
        Self { a: 1, b: 0 }
    }

    /// Update the checksum with more data.
    pub fn update(&mut self, data: &[u8]) {
        let mut a = self.a;
        let mut b = self.b;

        let mut remaining = data;

        // Process in chunks to avoid overflow
        while remaining.len() >= NMAX {
            let (chunk, rest) = remaining.split_at(NMAX);
            remaining = rest;

            for &byte in chunk {
                a += byte as u32;
                b += a;
            }

            a %= ADLER_MOD;
            b %= ADLER_MOD;
        }

        // Process remaining bytes
        for &byte in remaining {
            a += byte as u32;
            b += a;
        }

        self.a = a % ADLER_MOD;
        self.b = b % ADLER_MOD;
    }

    /// Finalize and return the checksum.
    pub fn finish(&self) -> u32 {
        (self.b << 16) | self.a
    }

    /// Compute Adler-32 checksum of data in one shot.
    pub fn checksum(data: &[u8]) -> u32 {
        let mut adler = Self::new();
        adler.update(data);
        adler.finish()
    }
}

impl Default for Adler32 {
    fn default() -> Self {
        Self::new()
    }
}

/// Compress data using zlib format.
///
/// # Arguments
///
/// * `input` - Data to compress
/// * `level` - Compression level (0-9)
///
/// # Example
///
/// ```
/// use oxiarc_deflate::zlib::{zlib_compress, zlib_decompress};
///
/// let data = b"Hello, World! Hello, World!";
/// let compressed = zlib_compress(data, 6).unwrap();
/// let decompressed = zlib_decompress(&compressed).unwrap();
/// assert_eq!(decompressed, data);
/// ```
pub fn zlib_compress(input: &[u8], level: u8) -> Result<Vec<u8>> {
    let level = level.min(9);

    // Compress with DEFLATE
    let compressed = deflate(input, level)?;

    // Build output with header and checksum
    let mut output = Vec::with_capacity(6 + compressed.len());

    // CMF byte: CM=8 (DEFLATE), CINFO=7 (32KB window)
    let cmf: u8 = 0x78; // 0111_1000 = CINFO=7, CM=8

    // FLG byte: FCHECK calculated so (CMF*256 + FLG) % 31 == 0
    let flevel = ZlibLevel::from_level(level) as u8;
    let fdict = 0u8; // No preset dictionary
    let fcheck = {
        let base = (cmf as u16) * 256 + ((flevel << 6) | (fdict << 5)) as u16;
        let remainder = base % 31;
        if remainder == 0 {
            0
        } else {
            (31 - remainder) as u8
        }
    };
    let flg = (flevel << 6) | (fdict << 5) | fcheck;

    output.push(cmf);
    output.push(flg);

    // Compressed data
    output.extend_from_slice(&compressed);

    // Adler-32 checksum (big-endian)
    let checksum = Adler32::checksum(input);
    output.push((checksum >> 24) as u8);
    output.push((checksum >> 16) as u8);
    output.push((checksum >> 8) as u8);
    output.push(checksum as u8);

    Ok(output)
}

/// Decompress zlib format data.
///
/// # Arguments
///
/// * `input` - Zlib compressed data
///
/// # Example
///
/// ```
/// use oxiarc_deflate::zlib::{zlib_compress, zlib_decompress};
///
/// let data = b"Hello, World! Hello, World!";
/// let compressed = zlib_compress(data, 6).unwrap();
/// let decompressed = zlib_decompress(&compressed).unwrap();
/// assert_eq!(decompressed, data);
/// ```
pub fn zlib_decompress(input: &[u8]) -> Result<Vec<u8>> {
    if input.len() < 6 {
        return Err(OxiArcError::invalid_header("zlib data too short"));
    }

    let cmf = input[0];
    let flg = input[1];

    // Validate CMF
    let cm = cmf & 0x0F;
    if cm != 8 {
        return Err(OxiArcError::invalid_header(
            "unsupported compression method",
        ));
    }

    let cinfo = cmf >> 4;
    if cinfo > 7 {
        return Err(OxiArcError::invalid_header("invalid window size"));
    }

    // Validate check bits
    let check = (cmf as u16) * 256 + (flg as u16);
    if check % 31 != 0 {
        return Err(OxiArcError::invalid_header("zlib header check failed"));
    }

    // Check for preset dictionary (not supported)
    let fdict = (flg >> 5) & 1;
    if fdict != 0 {
        return Err(OxiArcError::unsupported_method("preset dictionary"));
    }

    // Decompress DEFLATE data
    let deflate_data = &input[2..input.len() - 4];
    let decompressed = inflate(deflate_data)?;

    // Verify Adler-32 checksum
    let stored_checksum = u32::from_be_bytes([
        input[input.len() - 4],
        input[input.len() - 3],
        input[input.len() - 2],
        input[input.len() - 1],
    ]);
    let computed_checksum = Adler32::checksum(&decompressed);

    if stored_checksum != computed_checksum {
        return Err(OxiArcError::crc_mismatch(
            computed_checksum,
            stored_checksum,
        ));
    }

    Ok(decompressed)
}

/// Zlib compressor implementing streaming interface.
#[derive(Debug)]
pub struct ZlibCompressor {
    level: u8,
    buffer: Vec<u8>,
    finished: bool,
}

impl ZlibCompressor {
    /// Create a new zlib compressor.
    pub fn new(level: u8) -> Self {
        Self {
            level: level.min(9),
            buffer: Vec::new(),
            finished: false,
        }
    }

    /// Feed data to the compressor.
    pub fn write(&mut self, data: &[u8]) {
        self.buffer.extend_from_slice(data);
    }

    /// Finish compression and return compressed data.
    pub fn finish(&mut self) -> Result<Vec<u8>> {
        if self.finished {
            return Ok(Vec::new());
        }
        self.finished = true;
        zlib_compress(&self.buffer, self.level)
    }

    /// Reset the compressor.
    pub fn reset(&mut self) {
        self.buffer.clear();
        self.finished = false;
    }
}

/// Zlib decompressor implementing streaming interface.
#[derive(Debug)]
pub struct ZlibDecompressor {
    buffer: Vec<u8>,
    finished: bool,
}

impl ZlibDecompressor {
    /// Create a new zlib decompressor.
    pub fn new() -> Self {
        Self {
            buffer: Vec::new(),
            finished: false,
        }
    }

    /// Feed data to the decompressor.
    pub fn write(&mut self, data: &[u8]) {
        self.buffer.extend_from_slice(data);
    }

    /// Finish decompression and return decompressed data.
    pub fn finish(&mut self) -> Result<Vec<u8>> {
        if self.finished {
            return Ok(Vec::new());
        }
        self.finished = true;
        zlib_decompress(&self.buffer)
    }

    /// Reset the decompressor.
    pub fn reset(&mut self) {
        self.buffer.clear();
        self.finished = false;
    }
}

impl Default for ZlibDecompressor {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_adler32_empty() {
        let checksum = Adler32::checksum(&[]);
        assert_eq!(checksum, 1);
    }

    #[test]
    fn test_adler32_hello() {
        // Known value for "Hello"
        let checksum = Adler32::checksum(b"Hello");
        assert_eq!(checksum, 0x058C01F5);
    }

    #[test]
    fn test_adler32_incremental() {
        let data = b"Hello, World!";

        let one_shot = Adler32::checksum(data);

        let mut adler = Adler32::new();
        adler.update(&data[..6]);
        adler.update(&data[6..]);
        let incremental = adler.finish();

        assert_eq!(one_shot, incremental);
    }

    #[test]
    fn test_adler32_large() {
        // Test with data larger than NMAX
        let data = vec![0x42u8; 10000];
        let mut adler = Adler32::new();
        adler.update(&data);
        let checksum = adler.finish();
        assert_ne!(checksum, 0);
    }

    #[test]
    fn test_zlib_header() {
        let compressed = zlib_compress(b"test", 6).expect("compress failed");

        // Check CMF byte
        assert_eq!(compressed[0], 0x78);

        // Check FLG header validation
        let cmf = compressed[0] as u16;
        let flg = compressed[1] as u16;
        assert_eq!((cmf * 256 + flg) % 31, 0);
    }

    #[test]
    fn test_zlib_roundtrip_simple() {
        let data = b"Hello, World!";
        let compressed = zlib_compress(data, 6).expect("compress failed");
        let decompressed = zlib_decompress(&compressed).expect("decompress failed");
        assert_eq!(decompressed, data);
    }

    #[test]
    fn test_zlib_roundtrip_repeated() {
        let data = b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
        let compressed = zlib_compress(data, 6).expect("compress failed");
        // Should compress well
        assert!(compressed.len() < data.len());
        let decompressed = zlib_decompress(&compressed).expect("decompress failed");
        assert_eq!(decompressed, data);
    }

    #[test]
    fn test_zlib_roundtrip_empty() {
        let data: &[u8] = b"";
        let compressed = zlib_compress(data, 6).expect("compress failed");
        let decompressed = zlib_decompress(&compressed).expect("decompress failed");
        assert_eq!(decompressed, data);
    }

    #[test]
    fn test_zlib_roundtrip_large() {
        let data: Vec<u8> = (0..10000).map(|i| (i % 256) as u8).collect();
        let compressed = zlib_compress(&data, 6).expect("compress failed");
        let decompressed = zlib_decompress(&compressed).expect("decompress failed");
        assert_eq!(decompressed, data);
    }

    #[test]
    fn test_zlib_levels() {
        // Test with data that compresses well with fixed Huffman (levels 1-4)
        let data = b"Hello, World! Hello, World! Hello, World!";

        for level in 1..=9 {
            let compressed =
                zlib_compress(data, level).expect(&format!("level {} compress failed", level));
            let decompressed =
                zlib_decompress(&compressed).expect(&format!("level {} decompress failed", level));
            assert_eq!(&decompressed[..], &data[..]);
        }
    }

    #[test]
    fn test_zlib_level_0() {
        // Level 0 (stored blocks) with smaller data
        let data = b"Hello, World!";
        let compressed = zlib_compress(data, 0).expect("level 0 compress failed");
        let decompressed = zlib_decompress(&compressed).expect("level 0 decompress failed");
        assert_eq!(decompressed, data);
    }

    #[test]
    fn test_zlib_checksum_verification() {
        let data = b"Test data for checksum";
        let mut compressed = zlib_compress(data, 6).expect("compress failed");

        // Corrupt the checksum (last 4 bytes)
        let len = compressed.len();
        compressed[len - 1] ^= 0xFF;

        let result = zlib_decompress(&compressed);
        assert!(result.is_err());
    }

    #[test]
    fn test_zlib_invalid_header() {
        // Invalid compression method
        let bad_data = [0x08, 0x1D, 0x00, 0x00, 0x00, 0x01]; // CM != 8
        let result = zlib_decompress(&bad_data);
        assert!(result.is_err());
    }

    #[test]
    fn test_zlib_too_short() {
        let short_data = [0x78, 0x9C];
        let result = zlib_decompress(&short_data);
        assert!(result.is_err());
    }

    #[test]
    fn test_compressor_streaming() {
        let mut compressor = ZlibCompressor::new(6);
        compressor.write(b"Hello, ");
        compressor.write(b"World!");
        let compressed = compressor.finish().expect("compress failed");

        let decompressed = zlib_decompress(&compressed).expect("decompress failed");
        assert_eq!(decompressed, b"Hello, World!");
    }

    #[test]
    fn test_decompressor_streaming() {
        let compressed = zlib_compress(b"Hello, World!", 6).expect("compress failed");

        let mut decompressor = ZlibDecompressor::new();
        decompressor.write(&compressed[..5]);
        decompressor.write(&compressed[5..]);
        let decompressed = decompressor.finish().expect("decompress failed");
        assert_eq!(decompressed, b"Hello, World!");
    }
}
