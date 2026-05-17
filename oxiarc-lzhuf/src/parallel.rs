//! Parallel LHA archive compression.
//!
//! This module provides multi-entry LHA archive construction where each
//! entry's LZSS+Huffman compression runs in a separate rayon worker.
//!
//! Parallelism is *across entries* only — there is no cross-entry dictionary
//! sharing (not in the LH4–LH7 spec). Each worker independently calls
//! [`encode_lzh`] and packs a complete level-1 LHA member. Serial assembly
//! then concatenates the members in input order and appends the `0x00`
//! archive terminator.
//!
//! # Example
//!
//! ```rust,ignore
//! #[cfg(feature = "parallel")]
//! use oxiarc_lzhuf::{lzh_compress_parallel, LzhEntryInput, LzhMethod};
//!
//! let entries = vec![
//!     LzhEntryInput { name: "hello.txt", data: b"Hello, world!" },
//!     LzhEntryInput { name: "foo.txt",   data: b"foo bar baz" },
//! ];
//! let archive = lzh_compress_parallel(&entries, LzhMethod::Lh5).unwrap();
//! ```

use crate::encode::encode_lzh;
use crate::methods::LzhMethod;
use oxiarc_core::Crc16;
use oxiarc_core::error::{OxiArcError, Result};

#[cfg(feature = "parallel")]
use rayon::prelude::*;

// Compile-time assertion: LzhEncoder must be Send so workers can, in
// principle, hold one.  In practice this module calls `encode_lzh` (which
// allocates a fresh encoder on the stack) inside each closure, so no encoder
// crosses a thread boundary, but the assertion is kept as a contract check.
#[allow(dead_code)]
fn _assert_lzh_encoder_send() {
    fn is_send<T: Send>() {}
    is_send::<crate::encode::LzhEncoder>();
}

/// A single input entry for parallel LHA compression.
#[derive(Debug, Clone)]
pub struct LzhEntryInput<'a> {
    /// Entry filename as it will appear in the archive header.
    pub name: &'a str,
    /// Uncompressed payload bytes.
    pub data: &'a [u8],
}

/// Builder for a parallel LHA archive.
///
/// Construct with [`ParallelLzhBuilder::new`], optionally set the thread
/// count with [`with_num_threads`](ParallelLzhBuilder::with_num_threads),
/// then call [`build`](ParallelLzhBuilder::build) to produce the archive
/// bytes.
#[derive(Debug, Clone)]
pub struct ParallelLzhBuilder {
    method: LzhMethod,
    num_threads: Option<usize>,
}

impl ParallelLzhBuilder {
    /// Create a new builder with the given LZH method.
    pub fn new(method: LzhMethod) -> Self {
        Self {
            method,
            num_threads: None,
        }
    }

    /// Override the number of rayon worker threads.
    ///
    /// When `None` (the default), rayon uses its global thread pool.
    pub fn with_num_threads(mut self, n: usize) -> Self {
        self.num_threads = Some(n);
        self
    }

    /// Compress all entries in parallel and return a complete LHA archive.
    ///
    /// Returns an error if any entry fails to compress or if a filename
    /// exceeds 255 bytes.
    #[cfg(feature = "parallel")]
    pub fn build(&self, entries: &[LzhEntryInput<'_>]) -> Result<Vec<u8>> {
        match self.num_threads {
            None => lzh_compress_parallel(entries, self.method),
            Some(n) => {
                let pool = rayon::ThreadPoolBuilder::new()
                    .num_threads(n)
                    .build()
                    .map_err(|e| OxiArcError::invalid_header(format!("rayon pool error: {e}")))?;
                pool.install(|| lzh_compress_parallel(entries, self.method))
            }
        }
    }
}

/// Compress `entries` in parallel and return a complete, valid LHA archive.
///
/// Each entry is independently compressed (using `method`) by a rayon worker.
/// The members are assembled in the same order as `entries` and terminated
/// with a single `0x00` byte as required by the LHA archive format.
///
/// mtime is intentionally set to `0` (epoch / unknown) so that two calls with
/// the same input always produce byte-identical output.
///
/// # Errors
///
/// Returns an error if:
/// - Any filename exceeds 255 bytes.
/// - Any payload exceeds [`u32::MAX`] bytes.
/// - Any compression step fails internally.
#[cfg(feature = "parallel")]
pub fn lzh_compress_parallel(entries: &[LzhEntryInput<'_>], method: LzhMethod) -> Result<Vec<u8>> {
    // Compress all entries in parallel; preserve input order via collect().
    let members: Vec<Result<Vec<u8>>> = entries
        .par_iter()
        .map(|entry| compress_one_member(entry, method))
        .collect();

    // Fail fast: propagate the first error.
    let members: Vec<Vec<u8>> = members.into_iter().collect::<Result<Vec<_>>>()?;

    // Serial assembly: concatenate members in input order, then append 0x00.
    let total: usize = members.iter().map(|m| m.len()).sum::<usize>() + 1;
    let mut archive = Vec::with_capacity(total);
    for m in members {
        archive.extend_from_slice(&m);
    }
    archive.push(0x00); // LHA end-of-archive marker

    Ok(archive)
}

/// Compress one entry into a complete level-1 LHA member (header + payload).
///
/// mtime is always `0` for deterministic output.
fn compress_one_member(entry: &LzhEntryInput<'_>, method: LzhMethod) -> Result<Vec<u8>> {
    let name_bytes = entry.name.as_bytes();
    if name_bytes.len() > 255 {
        return Err(OxiArcError::invalid_header(
            "Filename too long for LZH level-1 header (max 255 bytes)",
        ));
    }

    let original_size = entry.data.len();
    if original_size > u32::MAX as usize {
        return Err(OxiArcError::invalid_header(
            "Entry data too large for LZH level-1 header (max 4 GiB)",
        ));
    }

    // Compute CRC-16 over the *uncompressed* data (LHA spec).
    let crc16 = Crc16::compute(entry.data);

    // Compress (no fallback to stored — use the requested method verbatim).
    let compressed = encode_lzh(entry.data, method)?;

    if compressed.len() > u32::MAX as usize {
        return Err(OxiArcError::invalid_header(
            "Compressed data too large for LZH level-1 header (max 4 GiB)",
        ));
    }

    // Build the level-1 LHA header (matches oxiarc-archive LzhWriter::write_level1_header).
    let header = build_level1_header(
        name_bytes,
        compressed.len() as u32,
        original_size as u32,
        crc16,
        0u32, // mtime = 0 for determinism
        method,
    );

    // member = header || compressed_payload
    let mut member = header;
    member.extend_from_slice(&compressed);
    Ok(member)
}

/// Build a level-1 LHA header byte vector.
///
/// Layout (all little-endian):
/// ```text
/// [0]     header_size  (u8)  — bytes from [2] to end of filename+extras
/// [1]     checksum     (u8)  — sum of bytes [2..end] mod 256
/// [2..7]  method_id    (5 bytes)
/// [7..11] compressed_size (u32)
/// [11..15] original_size (u32)
/// [15..19] mtime (u32)
/// [19]    attribute    (u8)  = 0x20
/// [20]    level        (u8)  = 1
/// [21]    fname_len    (u8)
/// [22..22+fname_len]  filename
/// [22+fname_len..+2]  crc16 (u16)
/// [24+fname_len]      os_id (u8) = 'U'
/// [25+fname_len..+2]  next_ext_size (u16) = 0
/// ```
fn build_level1_header(
    name_bytes: &[u8],
    compressed_size: u32,
    original_size: u32,
    crc16: u16,
    mtime: u32,
    method: LzhMethod,
) -> Vec<u8> {
    // Total header = 22 + name_len + 2 (crc) + 1 (os) + 2 (ext) = 27 + name_len
    // header_size field = total - 2 (excludes [0] and [1])
    let fname_len = name_bytes.len();
    let header_size_byte = (20 + fname_len) as u8; // bytes in [2..end]

    let mut header = Vec::with_capacity(27 + fname_len);

    // [0] header_size (checksum computed later)
    header.push(header_size_byte);
    // [1] checksum placeholder
    header.push(0u8);
    // [2..7] method id
    header.extend_from_slice(method.id());
    // [7..11] compressed size
    header.extend_from_slice(&compressed_size.to_le_bytes());
    // [11..15] original size
    header.extend_from_slice(&original_size.to_le_bytes());
    // [15..19] mtime
    header.extend_from_slice(&mtime.to_le_bytes());
    // [19] attribute
    header.push(0x20u8);
    // [20] level
    header.push(1u8);
    // [21] filename length
    header.push(fname_len as u8);
    // [22..] filename
    header.extend_from_slice(name_bytes);
    // crc16
    header.extend_from_slice(&crc16.to_le_bytes());
    // os id
    header.push(b'U');
    // next extended header size (none)
    header.extend_from_slice(&0u16.to_le_bytes());

    // Compute and fill in checksum: sum of bytes [2..] mod 256.
    let checksum: u8 = header[2..].iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
    header[1] = checksum;

    header
}

#[cfg(test)]
mod unit_tests {
    use super::*;

    #[test]
    fn test_build_level1_header_basics() {
        let h = build_level1_header(b"test.txt", 100, 200, 0xABCD, 0, LzhMethod::Lh5);
        // Check method id at bytes [2..7]
        assert_eq!(&h[2..7], b"-lh5-");
        // Level byte
        assert_eq!(h[20], 1u8);
        // OS id
        let fname_len = b"test.txt".len();
        assert_eq!(h[24 + fname_len], b'U');
    }

    #[test]
    fn test_compress_one_member_filename_too_long() {
        let long_name: String = "a".repeat(256);
        let entry = LzhEntryInput {
            name: &long_name,
            data: b"hello",
        };
        let result = compress_one_member(&entry, LzhMethod::Lh5);
        assert!(result.is_err(), "expected error for overlong filename");
    }
}
