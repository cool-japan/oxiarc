//! XZ file format header and reader/writer implementation.
//!
//! Based on XZ file format specification:
//! <https://tukaani.org/xz/xz-file-format.txt>
//!
//! This code used to live in `oxiarc-archive/src/xz/`; it moved here so
//! that image codecs (TIFF `Compression = 34925` stores a complete `.xz`
//! stream per strip) can depend on `oxiarc-lzma` alone. `oxiarc-archive`
//! re-exports it unchanged.
//!
//! ## Progress / cancellation
//!
//! [`XzReader`] and [`XzWriter`] expose `.with_progress()` / `.with_cancel()`
//! builders. The hooks are **emitted by the archive-crate wrapper itself** —
//! the underlying `oxiarc-lzma` LZMA2 encoder/decoder do not currently expose
//! per-chunk builders, so granularity is one-shot per block/stream.

use super::filters::XzFilter;
use crate::{Lzma2Decoder, Lzma2Encoder, LzmaLevel, dict_size_from_props, props_from_dict_size};
use oxiarc_core::cancel::CancellationToken;
use oxiarc_core::crc::{Crc32, Crc64};
use oxiarc_core::error::{OxiArcError, Result};
use oxiarc_core::progress::ProgressHandle;
use std::io::{Read, Write};

/// XZ magic bytes: 0xFD, '7', 'z', 'X', 'Z', 0x00
pub const XZ_MAGIC: [u8; 6] = [0xFD, 0x37, 0x7A, 0x58, 0x5A, 0x00];

/// XZ footer magic bytes: 'Y', 'Z'
pub const XZ_FOOTER_MAGIC: [u8; 2] = [0x59, 0x5A];

/// Check types supported by XZ.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
#[non_exhaustive]
pub enum CheckType {
    /// No check.
    None = 0x00,
    /// CRC-32.
    Crc32 = 0x01,
    /// CRC-64.
    Crc64 = 0x04,
    /// SHA-256.
    Sha256 = 0x0A,
}

impl CheckType {
    /// Create from check ID.
    pub fn from_id(id: u8) -> Option<Self> {
        match id {
            0x00 => Some(Self::None),
            0x01 => Some(Self::Crc32),
            0x04 => Some(Self::Crc64),
            0x0A => Some(Self::Sha256),
            _ => None,
        }
    }

    /// Get the size of the check in bytes.
    pub fn size(self) -> usize {
        match self {
            CheckType::None => 0,
            CheckType::Crc32 => 4,
            CheckType::Crc64 => 8,
            CheckType::Sha256 => 32,
        }
    }
}

/// XZ stream flags.
#[derive(Debug, Clone, Copy)]
pub struct StreamFlags {
    /// Check type (bits 0-3).
    pub check_type: CheckType,
}

impl StreamFlags {
    /// Create new stream flags.
    pub fn new(check_type: CheckType) -> Self {
        Self { check_type }
    }

    /// Encode stream flags to 2 bytes.
    pub fn encode(self) -> [u8; 2] {
        [0x00, self.check_type as u8]
    }

    /// Decode stream flags from 2 bytes.
    pub fn decode(bytes: [u8; 2]) -> Result<Self> {
        // First byte must be 0x00 (reserved)
        if bytes[0] != 0x00 {
            return Err(OxiArcError::invalid_header(
                "Invalid XZ stream flags: reserved byte is not zero",
            ));
        }

        // Second byte: bits 0-3 are check type, bits 4-7 must be 0
        if bytes[1] & 0xF0 != 0 {
            return Err(OxiArcError::invalid_header(
                "Invalid XZ stream flags: reserved bits are set",
            ));
        }

        let check_type = CheckType::from_id(bytes[1] & 0x0F).ok_or_else(|| {
            OxiArcError::invalid_header(format!("Unsupported XZ check type: {}", bytes[1] & 0x0F))
        })?;

        Ok(Self { check_type })
    }
}

/// LZMA2 filter ID.
pub const FILTER_LZMA2: u64 = 0x21;

/// Maximum accepted compressed size of a single XZ block (100 MiB).
///
/// Both block-reading paths honor this limit: `decompress_block` enforces
/// it while collecting self-describing LZMA2 chunks, and
/// `decompress_block_with_size` enforces it *before* allocating a buffer
/// for a header-declared size — the declared value is attacker-controlled
/// (up to ~2^63) and an unchecked `vec![0u8; declared]` allowed a 28-byte
/// crafted `.xz` to abort the process with an allocation failure.
const MAX_BLOCK_COMPRESSED_SIZE: usize = 100 * 1024 * 1024;

/// Block header flags.
#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
pub struct BlockHeaderFlags {
    /// Number of filters (1-4).
    pub num_filters: u8,
    /// Has compressed size.
    pub has_compressed_size: bool,
    /// Has uncompressed size.
    pub has_uncompressed_size: bool,
}

/// XZ reader for decompressing XZ streams.
pub struct XzReader<R: Read> {
    reader: R,
    stream_flags: StreamFlags,
    /// Optional progress sink (wrapper-emitted, per-block granularity).
    progress: Option<ProgressHandle>,
    /// Optional cancellation token checked before each block is processed.
    cancel: Option<CancellationToken>,
    /// Cumulative decompressed bytes produced so far.
    bytes_processed: u64,
    /// Size in bytes of the Index field (Index Indicator + Number of
    /// Records + List of Records + Index Padding + CRC32), populated by
    /// [`Self::skip_index`] and cross-checked against the stream footer's
    /// Backward Size field in [`Self::read_footer`].
    index_size: usize,
    /// Optional cap on the total uncompressed size of the stream, enforced
    /// *during* decoding (after every LZMA2 chunk), so a decompression bomb
    /// is rejected before its output is materialised.
    max_output: Option<u64>,
    /// Uncompressed bytes produced by blocks completed so far. Used both
    /// for the running budget check and for progress reporting.
    produced: u64,
}

impl<R: Read> XzReader<R> {
    /// Create a new XZ reader.
    pub fn new(mut reader: R) -> Result<Self> {
        // Read stream header
        let mut header = [0u8; 12];
        reader.read_exact(&mut header)?;

        // Verify magic
        if header[..6] != XZ_MAGIC {
            return Err(OxiArcError::InvalidMagic {
                expected: XZ_MAGIC.to_vec(),
                found: header[..6].to_vec(),
            });
        }

        // Decode stream flags
        let stream_flags = StreamFlags::decode([header[6], header[7]])?;

        // Verify CRC32
        let expected_crc = u32::from_le_bytes([header[8], header[9], header[10], header[11]]);
        let computed_crc = Crc32::compute(&header[6..8]);
        if expected_crc != computed_crc {
            return Err(OxiArcError::CrcMismatch {
                expected: expected_crc,
                computed: computed_crc,
            });
        }

        Ok(Self {
            reader,
            stream_flags,
            progress: None,
            cancel: None,
            bytes_processed: 0,
            index_size: 0,
            max_output: None,
            produced: 0,
        })
    }

    /// Attach a progress sink. Notified after each block is decompressed with
    /// the cumulative uncompressed byte count; `on_finish()` fires when the
    /// stream footer is read successfully.
    #[must_use]
    pub fn with_progress(mut self, handle: ProgressHandle) -> Self {
        self.progress = Some(handle);
        self
    }

    /// Attach a cancellation token. Checked before each block is processed.
    #[must_use]
    pub fn with_cancel(mut self, token: CancellationToken) -> Self {
        self.cancel = Some(token);
        self
    }

    /// Cap the total uncompressed size of the stream.
    ///
    /// The cap is checked after every LZMA2 chunk, not after the stream has
    /// been expanded, so a stream that would exceed it fails with
    /// [`OxiArcError::MemoryBudgetExceeded`] while its output is still
    /// bounded by roughly one chunk (at most 2 MiB) above the limit.
    #[must_use]
    pub fn with_max_output(mut self, max_output: u64) -> Self {
        self.max_output = Some(max_output);
        self
    }

    /// Decompress the whole `.xz` file.
    ///
    /// A `.xz` *file* is one or more Streams, optionally separated and
    /// terminated by Stream Padding (xz spec §2.2), which is how
    /// `cat a.xz b.xz` and every parallel xz compressor produce their
    /// output. Every stream is decoded and the results are concatenated,
    /// matching `xz -d`; a file that ends in anything other than clean EOF
    /// or well-formed padding is an error rather than a silent short read.
    pub fn decompress(&mut self) -> Result<Vec<u8>> {
        let mut output = Vec::new();

        // The first stream's header was consumed by `new()`.
        self.decompress_stream(&mut output)?;
        while self.next_stream_header()? {
            self.decompress_stream(&mut output)?;
        }

        if let Some(ref handle) = self.progress {
            handle.on_finish();
        }

        Ok(output)
    }

    /// Read the next Stream Header after a stream footer, skipping any
    /// Stream Padding.
    ///
    /// Returns `Ok(false)` at a clean end of file (no further stream), and
    /// `Ok(true)` after a valid header whose flags replace the current
    /// ones. Trailing bytes that are neither 4-byte-aligned null padding
    /// nor a valid header are an error, exactly as `xz -d` treats them.
    fn next_stream_header(&mut self) -> Result<bool> {
        let mut quad = [0u8; 4];
        loop {
            match read_up_to(&mut self.reader, &mut quad)? {
                0 => return Ok(false),
                4 => {}
                partial => {
                    return Err(OxiArcError::corrupted(
                        0,
                        format!(
                            "XZ file ends with {partial} trailing byte(s); Stream Padding must \
                             be a multiple of 4 null bytes"
                        ),
                    ));
                }
            }
            if quad != [0u8; 4] {
                break;
            }
            // Four more bytes of Stream Padding; keep looking.
        }

        let mut header = [0u8; 12];
        header[..4].copy_from_slice(&quad);
        let tail = read_up_to(&mut self.reader, &mut header[4..])?;
        if tail != 8 {
            return Err(OxiArcError::corrupted(
                0,
                format!(
                    "XZ file has {} trailing byte(s) after the last stream, which are \
                     neither Stream Padding nor a Stream Header",
                    4 + tail
                ),
            ));
        }

        if header[..6] != XZ_MAGIC {
            return Err(OxiArcError::InvalidMagic {
                expected: XZ_MAGIC.to_vec(),
                found: header[..6].to_vec(),
            });
        }
        let stream_flags = StreamFlags::decode([header[6], header[7]])?;
        let expected_crc = u32::from_le_bytes([header[8], header[9], header[10], header[11]]);
        let computed_crc = Crc32::compute(&header[6..8]);
        if expected_crc != computed_crc {
            return Err(OxiArcError::CrcMismatch {
                expected: expected_crc,
                computed: computed_crc,
            });
        }

        self.stream_flags = stream_flags;
        self.index_size = 0;
        Ok(true)
    }

    /// Decompress one stream whose Stream Header has already been consumed,
    /// appending its blocks' output to `output`.
    fn decompress_stream(&mut self, output: &mut Vec<u8>) -> Result<()> {
        let mut blocks = 0u64;

        loop {
            // Cooperative cancellation check before each block.
            if let Some(ref token) = self.cancel {
                token.check()?;
            }

            // Read block header size byte
            let mut header_size_byte = [0u8; 1];
            self.reader.read_exact(&mut header_size_byte)?;

            if header_size_byte[0] == 0x00 {
                // Index indicator - we've reached the end of blocks
                break;
            }

            // Block header size = (byte + 1) * 4
            let header_size = (header_size_byte[0] as usize + 1) * 4;

            // Read rest of block header
            let mut header = vec![0u8; header_size - 1];
            self.reader.read_exact(&mut header)?;

            // Validate the block header CRC32 *before* trusting any parsed
            // field (xz spec §3.1.7: the last 4 bytes of the block header
            // cover everything before them, including the size byte).
            if header.len() < 5 {
                return Err(OxiArcError::corrupted(0, "XZ block header too short"));
            }
            let crc_pos = header.len() - 4;
            let expected_header_crc = u32::from_le_bytes([
                header[crc_pos],
                header[crc_pos + 1],
                header[crc_pos + 2],
                header[crc_pos + 3],
            ]);
            let mut header_crc_input = Vec::with_capacity(header_size - 4);
            header_crc_input.push(header_size_byte[0]);
            header_crc_input.extend_from_slice(&header[..crc_pos]);
            let computed_header_crc = Crc32::compute(&header_crc_input);
            if computed_header_crc != expected_header_crc {
                return Err(OxiArcError::crc_mismatch(
                    expected_header_crc,
                    computed_header_crc,
                ));
            }

            // All parsed fields (size varints, filter list) must lie before
            // the trailing CRC32 field.
            let header_body = &header[..crc_pos];

            // Parse block header flags
            let flags = header_body[0];
            // xz spec §3.1.2: bits 2-5 are reserved and must be zero. A
            // future revision may use them to add fields ahead of the
            // filter list, so accepting them would mean mis-parsing the
            // rest of the header instead of reporting an unreadable file.
            if flags & 0x3C != 0 {
                return Err(OxiArcError::corrupted(
                    0,
                    format!("XZ block header sets reserved flag bits (0x{flags:02X})"),
                ));
            }
            let num_filters = (flags & 0x03) + 1;
            let has_compressed_size = (flags & 0x40) != 0;
            let has_uncompressed_size = (flags & 0x80) != 0;

            let mut offset = 1;

            // Read compressed size if present
            let compressed_size = if has_compressed_size {
                self.read_multibyte_int(header_body, &mut offset)?
            } else {
                0
            };

            // Read uncompressed size if present
            let declared_uncompressed_size = if has_uncompressed_size {
                self.read_multibyte_int(header_body, &mut offset)?
            } else {
                0
            };

            // Read the filter chain. The xz spec lists filters in the order
            // an encoder applied them, so the last one is the compression
            // filter (LZMA2 here) and the preceding "non-last" filters must
            // be undone in reverse order after decompression. Filters other
            // than LZMA2 used to be parsed and then silently dropped, which
            // produced wrong output rather than an error for every stream
            // that used one (libtiff writes `Delta(1) + LZMA2` for TIFF
            // Compression 34925).
            let mut dict_size = 1 << 20; // Default 1MB
            let mut saw_lzma2 = false;
            let mut non_last_filters: Vec<XzFilter> = Vec::new();
            for index in 0..num_filters {
                let filter_id = self.read_multibyte_int(header_body, &mut offset)?;
                let props_size = self.read_multibyte_int(header_body, &mut offset)?;

                // The declared properties must fit inside the block header
                // body. `read_multibyte_int` only guarantees
                // `offset <= header_body.len()`, so an unchecked
                // `header_body[offset]` (or an unchecked `offset += props`)
                // could index out of bounds on a crafted header.
                let props_len = usize::try_from(props_size)
                    .ok()
                    .filter(|&len| len <= header_body.len() - offset)
                    .ok_or_else(|| {
                        OxiArcError::corrupted(
                            0,
                            "XZ filter properties exceed the block header bounds",
                        )
                    })?;
                let props = &header_body[offset..offset + props_len];
                let is_last = index + 1 == num_filters;

                if filter_id == FILTER_LZMA2 {
                    // xz spec §5.3.1: LZMA2 is a "last" filter and has
                    // exactly one property byte.
                    if !is_last {
                        return Err(OxiArcError::corrupted(
                            0,
                            "XZ LZMA2 filter must be the last filter in the chain",
                        ));
                    }
                    if props_len != 1 {
                        return Err(OxiArcError::corrupted(
                            0,
                            format!("XZ LZMA2 filter has invalid properties size {props_len}"),
                        ));
                    }
                    let dict_props = header_body[offset];
                    dict_size = dict_size_from_props(dict_props);
                    if dict_size > crate::decoder::DICT_SIZE_ALLOC_CAP {
                        return Err(OxiArcError::corrupted(
                            0,
                            format!(
                                "XZ block declares LZMA2 dictionary size {dict_size} bytes, \
                                 exceeding the maximum allowed allocation of {} bytes",
                                crate::decoder::DICT_SIZE_ALLOC_CAP
                            ),
                        ));
                    }
                    saw_lzma2 = true;
                } else {
                    if is_last {
                        return Err(OxiArcError::corrupted(
                            0,
                            format!("XZ block ends with non-compression filter 0x{filter_id:02X}"),
                        ));
                    }
                    let filter = XzFilter::parse(filter_id, props)?;
                    if non_last_filters.contains(&filter) {
                        return Err(OxiArcError::corrupted(
                            0,
                            format!("XZ block repeats filter 0x{filter_id:02X}"),
                        ));
                    }
                    non_last_filters.push(filter);
                }
                offset += props_len;
            }

            if !saw_lzma2 {
                return Err(OxiArcError::UnsupportedMethod {
                    method: "XZ block without an LZMA2 compression filter".to_string(),
                });
            }

            // Remaining header-body bytes are padding (header is padded to
            // a multiple of 4); the CRC32 validated above already covers
            // them.

            // Decompress block data, then undo the non-last filters in
            // reverse order (the encoder applied them left to right before
            // handing the result to LZMA2).
            let (mut block_data, block_check) = if has_compressed_size && compressed_size > 0 {
                self.decompress_block_with_size(dict_size, compressed_size as usize)?
            } else {
                self.decompress_block(dict_size)?
            };
            for filter in non_last_filters.iter().rev() {
                filter.decode(&mut block_data);
            }
            // The check covers the original (unfiltered) block data, so it
            // is verified only now.
            self.verify_check(&block_data, &block_check)?;

            // xz spec §3.3: when the optional Uncompressed Size field is
            // present it must equal the size of the block's original data.
            // libtiff writes `LZMA_CHECK_NONE` strips, so on that path this
            // is the *only* integrity cross-check the format offers; without
            // it a corrupt block that still decodes is accepted silently.
            if has_uncompressed_size && declared_uncompressed_size != block_data.len() as u64 {
                return Err(OxiArcError::corrupted(
                    0,
                    format!(
                        "XZ block declares an uncompressed size of \
                         {declared_uncompressed_size} bytes but decoded to {}",
                        block_data.len()
                    ),
                ));
            }

            // Update cumulative progress after each block.
            self.produced = self.produced.saturating_add(block_data.len() as u64);
            self.bytes_processed = self.bytes_processed.saturating_add(block_data.len() as u64);
            if let Some(ref handle) = self.progress {
                handle.on_progress(self.bytes_processed, None);
            }

            output.extend_from_slice(&block_data);
            blocks = blocks.saturating_add(1);
        }

        // Parse the index, validating its record count and trailing CRC-32.
        self.skip_index(blocks)?;

        // Read stream footer
        self.read_footer()?;

        Ok(())
    }

    /// Read a multibyte integer (variable-length encoding).
    fn read_multibyte_int(&self, data: &[u8], offset: &mut usize) -> Result<u64> {
        let mut result = 0u64;
        let mut shift = 0;

        loop {
            if *offset >= data.len() {
                return Err(OxiArcError::corrupted(0, "Truncated multibyte integer"));
            }

            let byte = data[*offset];
            *offset += 1;

            result |= ((byte & 0x7F) as u64) << shift;
            shift += 7;

            if byte & 0x80 == 0 {
                break;
            }

            if shift > 63 {
                return Err(OxiArcError::corrupted(0, "Multibyte integer overflow"));
            }
        }

        Ok(result)
    }

    /// Verify a block check value.
    fn verify_check(&self, data: &[u8], check_bytes: &[u8]) -> Result<()> {
        match self.stream_flags.check_type {
            CheckType::None => Ok(()),
            CheckType::Crc32 => {
                if check_bytes.len() != 4 {
                    return Err(OxiArcError::corrupted(0, "Invalid CRC-32 check size"));
                }
                let expected = u32::from_le_bytes([
                    check_bytes[0],
                    check_bytes[1],
                    check_bytes[2],
                    check_bytes[3],
                ]);
                let computed = Crc32::compute(data);
                if computed != expected {
                    return Err(OxiArcError::crc_mismatch(expected, computed));
                }
                Ok(())
            }
            CheckType::Crc64 => {
                if check_bytes.len() != 8 {
                    return Err(OxiArcError::corrupted(0, "Invalid CRC-64 check size"));
                }
                let expected = u64::from_le_bytes([
                    check_bytes[0],
                    check_bytes[1],
                    check_bytes[2],
                    check_bytes[3],
                    check_bytes[4],
                    check_bytes[5],
                    check_bytes[6],
                    check_bytes[7],
                ]);
                let computed = Crc64::compute(data);
                if computed != expected {
                    return Err(OxiArcError::corrupted(
                        0,
                        format!(
                            "CRC-64 mismatch: expected {:016X}, computed {:016X}",
                            expected, computed
                        ),
                    ));
                }
                Ok(())
            }
            CheckType::Sha256 => {
                if check_bytes.len() < 32 {
                    return Err(OxiArcError::corrupted(
                        0,
                        format!(
                            "XZ SHA-256 check field too short: {} bytes",
                            check_bytes.len()
                        ),
                    ));
                }
                let mut expected = [0u8; 32];
                expected.copy_from_slice(&check_bytes[..32]);
                let computed = super::sha256::Sha256::compute(data);
                if computed != expected {
                    return Err(OxiArcError::corrupted(
                        0,
                        format!(
                            "SHA-256 mismatch: expected {}, computed {}",
                            super::sha256::hex32(&expected),
                            super::sha256::hex32(&computed),
                        ),
                    ));
                }
                Ok(())
            }
        }
    }

    /// Decompress a block with known compressed size.
    ///
    /// Returns the LZMA2 output (still carrying any non-last filters) and
    /// the block's check bytes; the caller undoes the filters and then
    /// verifies the check.
    fn decompress_block_with_size(
        &mut self,
        dict_size: u32,
        compressed_size: usize,
    ) -> Result<(Vec<u8>, Vec<u8>)> {
        // The declared size comes straight from the (attacker-controlled)
        // block header. Cap it to the same limit `decompress_block`
        // enforces, and allocate via `try_reserve_exact` so an allocation
        // failure surfaces as an error instead of aborting the process.
        if compressed_size > MAX_BLOCK_COMPRESSED_SIZE {
            return Err(OxiArcError::corrupted(
                0,
                format!(
                    "XZ block declares compressed size {compressed_size} bytes, \
                     exceeding the {MAX_BLOCK_COMPRESSED_SIZE}-byte limit"
                ),
            ));
        }
        let mut compressed: Vec<u8> = Vec::new();
        compressed.try_reserve_exact(compressed_size).map_err(|_| {
            OxiArcError::corrupted(
                0,
                format!("failed to allocate {compressed_size} bytes for an XZ block"),
            )
        })?;
        compressed.resize(compressed_size, 0);
        self.reader.read_exact(&mut compressed)?;

        // Decompress LZMA2 (budget-checked chunk by chunk)
        let (data, consumed) = self.decode_lzma2_payload(&compressed, dict_size)?;
        // xz spec §3.3: the Compressed Size field is the exact size of the
        // Compressed Data field, so the filter chain must consume all of it.
        // Ignoring a shorter consumption would silently accept trailing
        // bytes inside the block -- and on a `LZMA_CHECK_NONE` stream
        // nothing else would notice.
        if consumed != compressed_size {
            return Err(OxiArcError::corrupted(
                0,
                format!(
                    "XZ block declares a compressed size of {compressed_size} bytes but its                      LZMA2 payload ended after {consumed}"
                ),
            ));
        }

        // Read block padding (to 4-byte boundary)
        let padding = (4 - (compressed_size % 4)) % 4;
        if padding > 0 {
            let mut pad = vec![0u8; padding];
            self.reader.read_exact(&mut pad)?;
        }

        // Read the check field. It is *not* verified here: the xz spec
        // computes it over the block's original uncompressed data, which is
        // what comes out after the non-last filters have been undone, so
        // the caller verifies it once the filter chain has run.
        let check = self.read_check()?;

        Ok((data, check))
    }

    /// Decompress a block whose header does not declare the compressed size
    /// (real liblzma streams omit the optional size fields).
    ///
    /// LZMA2 chunk framing is self-describing: each chunk header carries the
    /// exact payload length, so the block payload can be collected chunk by
    /// chunk until the end-of-stream control byte (0x00).
    ///
    /// Returns the LZMA2 output and the block's check bytes, as
    /// [`Self::decompress_block_with_size`] does.
    fn decompress_block(&mut self, dict_size: u32) -> Result<(Vec<u8>, Vec<u8>)> {
        let mut compressed = Vec::new();
        loop {
            let mut ctrl = [0u8; 1];
            self.reader.read_exact(&mut ctrl)?;
            compressed.push(ctrl[0]);

            match ctrl[0] {
                // End of LZMA2 stream.
                0x00 => break,
                // Uncompressed chunk: 2-byte big-endian (size - 1) + payload.
                0x01 | 0x02 => {
                    let mut size_bytes = [0u8; 2];
                    self.reader.read_exact(&mut size_bytes)?;
                    compressed.extend_from_slice(&size_bytes);
                    let size = u16::from_be_bytes(size_bytes) as usize + 1;
                    let start = compressed.len();
                    compressed.resize(start + size, 0);
                    self.reader.read_exact(&mut compressed[start..])?;
                }
                // LZMA chunk: 2 bytes unpacked-size low bits, 2 bytes
                // (compressed size - 1), a props byte when the reset field
                // (bits 5-6) includes a property reset, then the payload.
                ctrl_byte if ctrl_byte >= 0x80 => {
                    let mut hdr = [0u8; 4];
                    self.reader.read_exact(&mut hdr)?;
                    compressed.extend_from_slice(&hdr);
                    let chunk_compressed = u16::from_be_bytes([hdr[2], hdr[3]]) as usize + 1;
                    let reset = (ctrl_byte >> 5) & 0x03;
                    if reset >= 2 {
                        let mut props = [0u8; 1];
                        self.reader.read_exact(&mut props)?;
                        compressed.push(props[0]);
                    }
                    let start = compressed.len();
                    compressed.resize(start + chunk_compressed, 0);
                    self.reader.read_exact(&mut compressed[start..])?;
                }
                invalid => {
                    return Err(OxiArcError::corrupted(
                        0,
                        format!("Invalid LZMA2 control byte 0x{invalid:02X}"),
                    ));
                }
            }

            // Safety limit (same cap as `decompress_block_with_size`)
            if compressed.len() > MAX_BLOCK_COMPRESSED_SIZE {
                return Err(OxiArcError::corrupted(0, "Block too large"));
            }
        }

        // Decompress LZMA2 (budget-checked chunk by chunk). The payload was
        // collected chunk by chunk from the same framing the decoder reads,
        // so it always consumes all of it; there is no declared size to
        // cross-check here.
        let (data, _consumed) = self.decode_lzma2_payload(&compressed, dict_size)?;

        // Read block padding (compressed data is padded to a 4-byte
        // boundary; the block header is always 4-aligned already)
        let padding = (4 - (compressed.len() % 4)) % 4;
        if padding > 0 {
            let mut pad = vec![0u8; padding];
            self.reader.read_exact(&mut pad)?;
        }

        // See `decompress_block_with_size`: the check covers the block's
        // original data, i.e. the bytes after the filter chain is undone.
        let check = self.read_check()?;

        Ok((data, check))
    }

    /// Read the block's check field (empty for `CheckType::None`).
    fn read_check(&mut self) -> Result<Vec<u8>> {
        let check_size = self.stream_flags.check_type.size();
        if check_size == 0 {
            return Ok(Vec::new());
        }
        let mut check = vec![0u8; check_size];
        self.reader.read_exact(&mut check)?;
        Ok(check)
    }

    /// Decode one block's LZMA2 payload, enforcing [`Self::max_output`]
    /// after every chunk.
    ///
    /// `Lzma2Decoder::decode` would expand the whole payload before any cap
    /// could be applied; decoding chunk by chunk keeps peak memory bounded
    /// by the budget (plus one chunk) even for a hostile stream.
    fn decode_lzma2_payload(&self, compressed: &[u8], dict_size: u32) -> Result<(Vec<u8>, usize)> {
        let mut decoder = Lzma2Decoder::new(dict_size);
        let mut cursor = std::io::Cursor::new(compressed);
        let mut data = Vec::new();
        loop {
            let more = decoder.decode_chunk(&mut cursor, &mut data)?;
            if let Some(max) = self.max_output {
                let total = self.produced.saturating_add(data.len() as u64);
                if total > max {
                    return Err(OxiArcError::memory_budget_exceeded(
                        usize::try_from(max).unwrap_or(usize::MAX),
                        usize::try_from(total).unwrap_or(usize::MAX),
                    ));
                }
            }
            if !more {
                break;
            }
        }
        let consumed = usize::try_from(cursor.position()).unwrap_or(compressed.len());
        Ok((data, consumed))
    }

    /// Parse the index, validate its record count against the number of
    /// blocks actually decoded, and validate its trailing CRC-32.
    ///
    /// The record count is checked *before* the record loop runs: it is an
    /// attacker-controlled multibyte integer (up to 2^63), and comparing it
    /// with `blocks` first turns a crafted count into an immediate error
    /// instead of a byte-at-a-time read loop.
    fn skip_index(&mut self, blocks: u64) -> Result<()> {
        // The index indicator (0x00) was already read when we detected end of blocks
        // Now we need to read the number of records and skip the index

        // Read index data into buffer to properly parse
        // Index format: indicator (already read) + num_records + records + padding + CRC32

        // Read number of records (multibyte)
        let mut index_data = Vec::new();
        index_data.push(0x00); // The index indicator we already saw

        // Read bytes until we have the full index
        // Read number of records first
        let mut num_records = 0u64;
        let mut shift = 0;
        loop {
            let mut byte = [0u8; 1];
            self.reader.read_exact(&mut byte)?;
            index_data.push(byte[0]);
            num_records |= ((byte[0] & 0x7F) as u64) << shift;
            shift += 7;
            if byte[0] & 0x80 == 0 {
                break;
            }
            if shift > 63 {
                return Err(OxiArcError::corrupted(
                    0,
                    "XZ index record count overflows a 64-bit integer",
                ));
            }
        }

        if num_records != blocks {
            return Err(OxiArcError::corrupted(
                0,
                format!(
                    "XZ index declares {num_records} record(s) but the stream contains \
                     {blocks} block(s)"
                ),
            ));
        }

        // Read each record (unpadded size + uncompressed size, both multibyte)
        for _ in 0..num_records {
            // Unpadded size
            loop {
                let mut byte = [0u8; 1];
                self.reader.read_exact(&mut byte)?;
                index_data.push(byte[0]);
                if byte[0] & 0x80 == 0 {
                    break;
                }
            }
            // Uncompressed size
            loop {
                let mut byte = [0u8; 1];
                self.reader.read_exact(&mut byte)?;
                index_data.push(byte[0]);
                if byte[0] & 0x80 == 0 {
                    break;
                }
            }
        }

        // Read padding (zeros to align to 4 bytes)
        while (index_data.len() + 4) % 4 != 0 {
            let mut byte = [0u8; 1];
            self.reader.read_exact(&mut byte)?;
            index_data.push(byte[0]);
        }

        // Read and verify the trailing CRC32, which covers everything parsed
        // above (Index Indicator + Number of Records + List of Records +
        // Index Padding) but not the CRC32 field itself.
        let mut crc = [0u8; 4];
        self.reader.read_exact(&mut crc)?;
        let expected_crc = u32::from_le_bytes(crc);
        let computed_crc = Crc32::compute(&index_data);
        if expected_crc != computed_crc {
            return Err(OxiArcError::crc_mismatch(expected_crc, computed_crc));
        }

        // Remember the total on-disk size of the Index field (everything
        // just parsed, plus the 4-byte CRC32) so it can be cross-checked
        // against the stream footer's Backward Size field.
        self.index_size = index_data.len() + 4;

        Ok(())
    }

    /// Read and verify the stream footer.
    fn read_footer(&mut self) -> Result<()> {
        // Read footer
        let mut footer = [0u8; 12];
        self.reader.read_exact(&mut footer)?;

        // Verify footer magic
        if footer[10..12] != XZ_FOOTER_MAGIC {
            return Err(OxiArcError::invalid_header("Invalid XZ footer magic"));
        }

        // Verify stream flags match header
        let footer_flags = StreamFlags::decode([footer[8], footer[9]])?;
        if footer_flags.check_type != self.stream_flags.check_type {
            return Err(OxiArcError::invalid_header(
                "Stream flags in footer don't match header",
            ));
        }

        // Backward Size is stored as `(real_index_size / 4) - 1`, and the
        // real Index field size must always be a multiple of 4 bytes
        // (it is explicitly padded to that alignment). Cross-check it
        // against the Index field we actually parsed in `skip_index`.
        let backward_size_field = u32::from_le_bytes([footer[4], footer[5], footer[6], footer[7]]);
        if self.index_size % 4 != 0 {
            return Err(OxiArcError::corrupted(
                0,
                format!(
                    "XZ index size {} is not a multiple of 4 bytes",
                    self.index_size
                ),
            ));
        }
        // `self.index_size` is a `usize` derived from the untrusted, parsed
        // Index field, while the footer's Backward Size is a `u32`. A plain
        // `as u32` here would silently wrap on a 64-bit target once the index
        // exceeds ~16 GiB, letting a crafted oversized index alias a forged
        // footer value and defeat this consistency check. `try_from` instead
        // rejects the stream outright when the real size cannot be
        // represented, which is always correct: a genuine Backward Size field
        // can never legitimately describe an index that large.
        let expected_backward_size = u32::try_from((self.index_size / 4).saturating_sub(1))
            .map_err(|_| {
                OxiArcError::corrupted(
                    0,
                    format!(
                        "XZ index size {} does not fit the 32-bit Backward Size field",
                        self.index_size
                    ),
                )
            })?;
        if backward_size_field != expected_backward_size {
            return Err(OxiArcError::corrupted(
                0,
                format!(
                    "XZ footer Backward Size ({backward_size_field}) does not match \
                     the parsed index size (expected {expected_backward_size})"
                ),
            ));
        }

        Ok(())
    }
}

/// XZ writer for creating XZ compressed files.
pub struct XzWriter {
    level: LzmaLevel,
    check_type: CheckType,
    /// Optional progress sink (wrapper-emitted, one-shot).
    progress: Option<ProgressHandle>,
    /// Optional cancellation token checked before compression.
    cancel: Option<CancellationToken>,
}

impl XzWriter {
    /// Create a new XZ writer.
    pub fn new(level: LzmaLevel) -> Self {
        Self {
            level,
            check_type: CheckType::Crc32,
            progress: None,
            cancel: None,
        }
    }

    /// Set the check type.
    #[must_use]
    pub fn with_check_type(mut self, check_type: CheckType) -> Self {
        self.check_type = check_type;
        self
    }

    /// Attach a progress sink. Notified once after compression completes with
    /// the uncompressed byte count, followed by `on_finish()`.
    #[must_use]
    pub fn with_progress(mut self, handle: ProgressHandle) -> Self {
        self.progress = Some(handle);
        self
    }

    /// Attach a cancellation token. Checked before compression begins.
    #[must_use]
    pub fn with_cancel(mut self, token: CancellationToken) -> Self {
        self.cancel = Some(token);
        self
    }

    /// Compress data to XZ format.
    pub fn compress(&self, data: &[u8]) -> Result<Vec<u8>> {
        if let Some(ref token) = self.cancel {
            token.check()?;
        }

        let mut output = Vec::new();

        // Write stream header
        let stream_flags = StreamFlags::new(self.check_type);
        self.write_stream_header(&mut output, stream_flags)?;

        // Write block; keep its Unpadded Size (header + compressed data +
        // check, excluding block padding) for the index record.
        let unpadded_size = self.write_block(&mut output, data)?;

        // Write index
        let index_start = output.len();
        self.write_index(&mut output, unpadded_size, data.len())?;
        let index_end = output.len();

        // Write stream footer
        self.write_stream_footer(&mut output, stream_flags, index_end - index_start)?;

        if let Some(ref handle) = self.progress {
            let total = data.len() as u64;
            handle.on_progress(total, Some(total));
            handle.on_finish();
        }

        Ok(output)
    }

    /// Write stream header.
    fn write_stream_header<W: Write>(&self, writer: &mut W, flags: StreamFlags) -> Result<()> {
        // Magic
        writer.write_all(&XZ_MAGIC)?;

        // Stream flags
        let flags_bytes = flags.encode();
        writer.write_all(&flags_bytes)?;

        // CRC32 of stream flags
        let crc = Crc32::compute(&flags_bytes);
        writer.write_all(&crc.to_le_bytes())?;

        Ok(())
    }

    /// Write a compressed block.
    ///
    /// Returns the block's Unpadded Size (block header + compressed data +
    /// check, excluding block padding) as required by the index record.
    ///
    /// The block header declares both optional size fields (flags `0xC0`),
    /// exactly as the `xz` CLI does. **Uncompressed Size is the size of the
    /// block's *original* data — before any filter chain, not after it** —
    /// and Compressed Size is the exact length of the Compressed Data
    /// field. The reader enforces both (`decompress_stream` /
    /// `decompress_block_with_size`), so if this writer ever grows a
    /// non-last filter (Delta, BCJ), it must keep declaring the original
    /// size here or produce files it cannot read back.
    fn write_block<W: Write>(&self, writer: &mut W, data: &[u8]) -> Result<usize> {
        // Compress data with LZMA2
        let encoder = Lzma2Encoder::new(self.level);
        let compressed = encoder.encode(data)?;

        // Calculate dictionary size props
        let dict_size = self.level.dict_size();
        let dict_props = props_from_dict_size(dict_size);

        // Build compressed size as multibyte int
        let mut compressed_size_bytes = Vec::new();
        Self::write_multibyte_int_static(&mut compressed_size_bytes, compressed.len() as u64);

        // Build uncompressed size as multibyte int
        let mut uncompressed_size_bytes = Vec::new();
        Self::write_multibyte_int_static(&mut uncompressed_size_bytes, data.len() as u64);

        // Build block header content (not including size byte or CRC)
        let mut block_header = Vec::new();

        // Flags: 1 filter, has compressed size, has uncompressed size
        block_header.push(0xC0); // 1 filter, has compressed size (0x40), has uncompressed size (0x80)

        // Compressed size
        block_header.extend_from_slice(&compressed_size_bytes);

        // Uncompressed size
        block_header.extend_from_slice(&uncompressed_size_bytes);

        // Filter: LZMA2
        block_header.push(FILTER_LZMA2 as u8); // Filter ID (single byte for LZMA2)
        block_header.push(0x01); // Properties size = 1
        block_header.push(dict_props); // Dictionary size properties

        // Calculate header size byte first
        // Total header size = 1 (size byte) + content + padding + 4 (CRC)
        // Must be multiple of 4, so: (size_byte + 1) * 4 = 1 + content + padding + 4
        // padding = ((size_byte + 1) * 4) - 1 - content - 4 = (size_byte + 1) * 4 - 5 - content
        // We need the smallest size_byte such that (size_byte + 1) * 4 >= 1 + content + 4
        // (size_byte + 1) * 4 >= content + 5
        // size_byte >= (content + 5) / 4 - 1
        // size_byte = ceil((content + 5) / 4) - 1 = (content + 5 + 3) / 4 - 1 = (content + 4) / 4
        let header_size_byte = ((block_header.len() + 4) / 4) as u8;
        let total_header_size = (header_size_byte as usize + 1) * 4;
        let padding = total_header_size - 1 - block_header.len() - 4;

        // Add padding
        block_header.resize(block_header.len() + padding, 0x00);

        // CRC32 of block header (size byte + padded content, per the xz
        // format spec section 3.1: everything except the CRC32 field itself)
        let mut header_crc_input = Vec::with_capacity(1 + block_header.len());
        header_crc_input.push(header_size_byte);
        header_crc_input.extend_from_slice(&block_header);
        let header_crc = Crc32::compute(&header_crc_input);

        // Write size byte
        writer.write_all(&[header_size_byte])?;

        // Write block header content
        writer.write_all(&block_header)?;

        // Write block header CRC
        writer.write_all(&header_crc.to_le_bytes())?;

        // Write compressed data
        writer.write_all(&compressed)?;

        // Pad compressed data to a 4-byte boundary (block padding is NOT
        // part of the Unpadded Size recorded in the index)
        let padding = (4 - (compressed.len() % 4)) % 4;
        for _ in 0..padding {
            writer.write_all(&[0x00])?;
        }

        // Write check
        match self.check_type {
            CheckType::None => {}
            CheckType::Crc32 => {
                let crc = Crc32::compute(data);
                writer.write_all(&crc.to_le_bytes())?;
            }
            CheckType::Crc64 => {
                let crc = Crc64::compute(data);
                writer.write_all(&crc.to_le_bytes())?;
            }
            CheckType::Sha256 => {
                let digest = super::sha256::Sha256::compute(data);
                writer.write_all(&digest)?;
            }
        }

        // Unpadded Size = block header + compressed data + check
        Ok(total_header_size + compressed.len() + self.check_type.size())
    }

    /// Write a multibyte integer (static version).
    fn write_multibyte_int_static(output: &mut Vec<u8>, mut value: u64) {
        loop {
            let byte = (value & 0x7F) as u8;
            value >>= 7;
            if value == 0 {
                output.push(byte);
                break;
            } else {
                output.push(byte | 0x80);
            }
        }
    }

    /// Write index.
    ///
    /// `unpadded_size` is the block size WITHOUT the trailing block padding
    /// (header + compressed data + check), per the xz format spec.
    fn write_index<W: Write>(
        &self,
        writer: &mut W,
        unpadded_size: usize,
        uncompressed_size: usize,
    ) -> Result<()> {
        let mut index = Vec::new();

        // Index indicator
        index.push(0x00);

        // Number of records (1)
        index.push(0x01);

        // Record: unpadded size, uncompressed size
        self.write_multibyte_int(&mut index, unpadded_size as u64);
        self.write_multibyte_int(&mut index, uncompressed_size as u64);

        // Pad to 4 bytes
        while (index.len() + 4) % 4 != 0 {
            index.push(0x00);
        }

        // CRC32
        let crc = Crc32::compute(&index);
        index.extend_from_slice(&crc.to_le_bytes());

        writer.write_all(&index)?;

        Ok(())
    }

    /// Write a multibyte integer.
    fn write_multibyte_int(&self, output: &mut Vec<u8>, mut value: u64) {
        loop {
            let byte = (value & 0x7F) as u8;
            value >>= 7;
            if value == 0 {
                output.push(byte);
                break;
            } else {
                output.push(byte | 0x80);
            }
        }
    }

    /// Write stream footer.
    fn write_stream_footer<W: Write>(
        &self,
        writer: &mut W,
        flags: StreamFlags,
        index_size: usize,
    ) -> Result<()> {
        // Backward size (index size / 4 - 1). `index_size` is a `usize` we
        // computed ourselves while writing the Index field, but a plain
        // `as u32` would still silently wrap once it exceeds `u32::MAX`
        // (an index that large implies an implausibly large archive, but
        // "implausible" is not "impossible" on a 64-bit target) and emit a
        // footer whose Backward Size does not describe the Index we just
        // wrote -- a self-corrupting archive our own reader would then
        // reject. Mirrors the `try_from` guard `read_footer` applies to the
        // same field on the decode side.
        let backward_size = u32::try_from((index_size / 4).saturating_sub(1)).map_err(|_| {
            OxiArcError::encoding_error(format!(
                "XZ index size {index_size} does not fit the 32-bit Backward Size field"
            ))
        })?;

        // CRC32 of backward size and stream flags
        let mut footer_data = Vec::new();
        footer_data.extend_from_slice(&backward_size.to_le_bytes());
        footer_data.extend_from_slice(&flags.encode());
        let crc = Crc32::compute(&footer_data);

        // Write footer
        writer.write_all(&crc.to_le_bytes())?;
        writer.write_all(&backward_size.to_le_bytes())?;
        writer.write_all(&flags.encode())?;
        writer.write_all(&XZ_FOOTER_MAGIC)?;

        Ok(())
    }
}

/// Fill `buf` from `reader`, returning how many bytes were read before the
/// end of input.
///
/// `Read::read` may return fewer bytes than asked for without being at EOF,
/// so a single call cannot distinguish "no more data" from "not yet"; this
/// loops until the buffer is full or a read returns zero. `Interrupted` is
/// retried, as `read_exact` does.
fn read_up_to<R: Read>(reader: &mut R, buf: &mut [u8]) -> Result<usize> {
    let mut filled = 0usize;
    while filled < buf.len() {
        match reader.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(filled)
}

/// Decompress XZ data from a reader.
pub fn decompress<R: Read>(reader: &mut R) -> Result<Vec<u8>> {
    let mut xz_reader = XzReader::new(reader)?;
    xz_reader.decompress()
}

/// Decompress a complete `.xz` stream directly into a caller-supplied
/// buffer.
///
/// This is the entry point for TIFF `Compression = 34925`, where every
/// strip/tile is a complete `.xz` stream whose uncompressed size the caller
/// already knows from the image geometry. The output cap is enforced
/// *during* decoding (after every LZMA2 chunk), so a strip that claims to
/// expand far beyond `dst` fails immediately instead of allocating first.
///
/// Note that the LZMA2 layer needs a contiguous window over the block it is
/// decoding, so one block-sized buffer is still used internally; what this
/// function guarantees is that the caller allocates nothing, that the
/// decoded size never has to be guessed, and that a stream larger than
/// `dst` is an error rather than a truncation.
///
/// # Returns
///
/// The number of bytes written to `dst`, which may be fewer than
/// `dst.len()` when the stream is shorter than the buffer.
///
/// # Errors
///
/// - [`OxiArcError::BufferTooSmall`] if the stream expands past `dst`. The
///   overflow is detected *during* decoding, so `needed` is a lower bound
///   (the bytes produced when the limit was crossed), not the stream's full
///   uncompressed size — which is exactly why nothing has to be expanded to
///   find out.
/// - the usual framing/CRC errors for a corrupt stream.
///
/// # Example
///
/// ```rust
/// use oxiarc_lzma::xz;
///
/// let original = b"one TIFF strip, one .xz stream";
/// let stream = xz::compress(original, 6)?;
///
/// let mut strip = vec![0u8; original.len()];
/// let written = xz::decompress_into(&stream, &mut strip)?;
/// assert_eq!(written, original.len());
/// assert_eq!(&strip[..written], original);
/// # Ok::<(), oxiarc_core::error::OxiArcError>(())
/// ```
pub fn decompress_into(src: &[u8], dst: &mut [u8]) -> Result<usize> {
    let mut reader = XzReader::new(std::io::Cursor::new(src))?.with_max_output(dst.len() as u64);
    // The cap is `dst.len()`, so an oversized stream trips the running
    // budget check inside the decoder rather than this function's own
    // post-hoc comparison. Report it the way this entry point documents it
    // (and the way `oxiarc_deflate::inflate_into` reports the same
    // condition) instead of leaking a budget the caller never configured;
    // `requested` is the running total that crossed the limit, i.e. a lower
    // bound on what `dst` would have needed.
    let data = reader.decompress().map_err(|err| match err {
        OxiArcError::MemoryBudgetExceeded { requested, .. } => OxiArcError::BufferTooSmall {
            needed: requested,
            available: dst.len(),
        },
        other => other,
    })?;
    if data.len() > dst.len() {
        return Err(OxiArcError::BufferTooSmall {
            needed: data.len(),
            available: dst.len(),
        });
    }
    dst[..data.len()].copy_from_slice(&data);
    Ok(data.len())
}

/// Decompress a complete `.xz` stream with an explicit output cap.
///
/// Like [`decompress`] but bounded: the cap is enforced after every LZMA2
/// chunk, so an `.xz` decompression bomb is rejected while its output is
/// still bounded by roughly the limit plus one chunk.
///
/// # Errors
///
/// [`OxiArcError::MemoryBudgetExceeded`] when the stream expands past
/// `max_output`, plus the usual framing/CRC errors.
///
/// # Example
///
/// ```rust
/// use oxiarc_lzma::xz;
///
/// let stream = xz::compress(&vec![0u8; 1 << 20], 6)?;
///
/// // Within budget:
/// assert_eq!(xz::decompress_with_limit(&stream, 1 << 20)?.len(), 1 << 20);
///
/// // Over budget: rejected, not expanded.
/// assert!(xz::decompress_with_limit(&stream, 4096).is_err());
/// # Ok::<(), oxiarc_core::error::OxiArcError>(())
/// ```
pub fn decompress_with_limit(data: &[u8], max_output: usize) -> Result<Vec<u8>> {
    let mut reader = XzReader::new(std::io::Cursor::new(data))?.with_max_output(max_output as u64);
    reader.decompress()
}

/// Decompress XZ data from a byte slice (test utility).
#[cfg(test)]
fn decompress_slice(data: &[u8]) -> Result<Vec<u8>> {
    decompress(&mut std::io::Cursor::new(data))
}

/// Compress data to XZ format.
pub fn compress(data: &[u8], level: u8) -> Result<Vec<u8>> {
    let lzma_level = LzmaLevel::new(level);
    let writer = XzWriter::new(lzma_level);
    writer.compress(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_stream_flags_encode_decode() {
        let flags = StreamFlags::new(CheckType::Crc32);
        let encoded = flags.encode();
        let decoded = StreamFlags::decode(encoded).expect("StreamFlags::decode");
        assert_eq!(decoded.check_type, CheckType::Crc32);
    }

    #[test]
    fn test_check_type_sizes() {
        assert_eq!(CheckType::None.size(), 0);
        assert_eq!(CheckType::Crc32.size(), 4);
        assert_eq!(CheckType::Crc64.size(), 8);
        assert_eq!(CheckType::Sha256.size(), 32);
    }

    #[test]
    fn test_xz_magic() {
        assert_eq!(XZ_MAGIC, [0xFD, 0x37, 0x7A, 0x58, 0x5A, 0x00]);
        assert_eq!(XZ_FOOTER_MAGIC, [0x59, 0x5A]);
    }

    #[test]
    fn test_xz_roundtrip_empty() {
        let original: Vec<u8> = vec![];
        let compressed = compress(&original, 6).expect("compress empty");
        // XZ header (12) + block + footer (12) = should have XZ structure
        assert!(compressed.len() > 24); // At minimum: header + empty block + footer
        assert_eq!(&compressed[0..6], XZ_MAGIC);

        let decompressed = decompress_slice(&compressed).expect("decompress empty");
        assert_eq!(decompressed, original);
    }

    #[test]
    fn test_xz_roundtrip_hello() {
        let original = b"Hello, World!";
        let compressed = compress(original, 6).expect("compress hello");
        assert_eq!(&compressed[0..6], XZ_MAGIC);

        let decompressed = decompress_slice(&compressed).expect("decompress hello");
        assert_eq!(&decompressed, original);
    }

    #[test]
    fn test_xz_roundtrip_single_byte() {
        let original = [0x42u8];
        let compressed = compress(&original, 6).expect("compress single byte");

        let decompressed = decompress_slice(&compressed).expect("decompress single byte");
        assert_eq!(decompressed, original);
    }

    #[test]
    fn test_xz_roundtrip_repeated_pattern() {
        // Highly compressible data
        let original: Vec<u8> = (0..1000).map(|_| b'A').collect();
        let compressed = compress(&original, 6).expect("compress repeated pattern");
        // Should compress well
        assert!(compressed.len() < original.len());

        let decompressed = decompress_slice(&compressed).expect("decompress repeated pattern");
        assert_eq!(decompressed, original);
    }

    /// Small deterministic xorshift PRNG so tests can generate reproducible,
    /// incompressible-looking data without depending on an external `rand`
    /// crate (SciRS2-Core is for numeric/array workloads, not needed here).
    fn xorshift_bytes(seed: u64, len: usize) -> Vec<u8> {
        let mut state = seed | 1;
        let mut out = Vec::with_capacity(len);
        while out.len() < len {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            out.extend_from_slice(&state.to_le_bytes());
        }
        out.truncate(len);
        out
    }

    #[test]
    fn test_xz_roundtrip_incompressible_random() {
        // Pseudo-random, effectively incompressible payload well above the
        // "Hello, World!" / 1000x'A' sizes used by the other roundtrip tests.
        let original = xorshift_bytes(0xDEAD_BEEF_C0FF_EE01, 64 * 1024);
        let compressed = compress(&original, 6).expect("compress random data");
        assert_eq!(&compressed[0..6], XZ_MAGIC);

        let decompressed = decompress_slice(&compressed).expect("decompress random data");
        assert_eq!(decompressed, original);
    }

    #[test]
    fn test_xz_roundtrip_large_multi_block() {
        // Hand-assemble a two-block XZ stream (our own `XzWriter::compress`
        // only ever emits a single block) to exercise the reader's
        // multi-block loop together with the new index CRC-32 and footer
        // Backward Size validation against a genuine, format-compliant
        // multi-record index.
        let writer = XzWriter::new(LzmaLevel::new(6));
        let stream_flags = StreamFlags::new(writer.check_type);

        let block_a = xorshift_bytes(0x1234_5678_9ABC_DEF0, 48 * 1024);
        let block_b: Vec<u8> = (0..96 * 1024).map(|i| (i % 251) as u8).collect();

        let mut output = Vec::new();
        writer
            .write_stream_header(&mut output, stream_flags)
            .expect("write stream header");
        let unpadded_a = writer
            .write_block(&mut output, &block_a)
            .expect("write block a");
        let unpadded_b = writer
            .write_block(&mut output, &block_b)
            .expect("write block b");

        // Build a genuine 2-record index (Index Indicator + Number of
        // Records + records + padding + CRC32), matching the on-disk layout
        // `write_index` produces for a single record.
        let mut index = vec![0x00u8];
        index.push(0x02); // number of records
        writer.write_multibyte_int(&mut index, unpadded_a as u64);
        writer.write_multibyte_int(&mut index, block_a.len() as u64);
        writer.write_multibyte_int(&mut index, unpadded_b as u64);
        writer.write_multibyte_int(&mut index, block_b.len() as u64);
        while (index.len() + 4) % 4 != 0 {
            index.push(0x00);
        }
        let index_crc = Crc32::compute(&index);
        index.extend_from_slice(&index_crc.to_le_bytes());
        output.extend_from_slice(&index);

        writer
            .write_stream_footer(&mut output, stream_flags, index.len())
            .expect("write stream footer");

        let mut expected = block_a.clone();
        expected.extend_from_slice(&block_b);

        let decompressed =
            decompress_slice(&output).expect("decompress hand-assembled multi-block stream");
        assert_eq!(decompressed, expected);
    }

    #[test]
    fn test_xz_index_crc_mismatch_detected() {
        let original = b"index CRC mismatch should be detected".to_vec();
        let mut compressed = compress(&original, 3).expect("compress");

        // Corrupt a byte inside the index's CRC32 trailer (the last 4 bytes
        // before the 12-byte stream footer).
        let crc_offset = compressed.len() - 12 - 1;
        compressed[crc_offset] ^= 0xFF;

        let err = decompress_slice(&compressed).expect_err("corrupted index CRC must be rejected");
        assert!(
            matches!(err, OxiArcError::CrcMismatch { .. }),
            "expected CrcMismatch, got {err:?}"
        );
    }

    #[test]
    fn test_xz_progress_forwarding() {
        use oxiarc_core::progress::{ProgressHandle, ProgressSink};
        use std::sync::Arc;
        use std::sync::atomic::{AtomicU64, Ordering};

        struct CountingSink {
            progress_count: AtomicU64,
            finish_count: AtomicU64,
            last_processed: AtomicU64,
        }
        impl ProgressSink for CountingSink {
            fn on_progress(&self, processed: u64, _total: Option<u64>) {
                self.progress_count.fetch_add(1, Ordering::SeqCst);
                self.last_processed.store(processed, Ordering::SeqCst);
            }
            fn on_entry(&self, _name: &str, _index: u64) {}
            fn on_finish(&self) {
                self.finish_count.fetch_add(1, Ordering::SeqCst);
            }
        }

        let sink = Arc::new(CountingSink {
            progress_count: AtomicU64::new(0),
            finish_count: AtomicU64::new(0),
            last_processed: AtomicU64::new(0),
        });
        let handle: ProgressHandle = sink.clone();

        // Use a small repeating payload so the underlying LZMA encoder
        // handles it correctly (see module notes on complex data patterns).
        let data: Vec<u8> = (0..1_000).map(|_| b'A').collect();
        let writer = XzWriter::new(LzmaLevel::new(6)).with_progress(handle);
        let _compressed = writer
            .compress(&data)
            .expect("xz compression with progress should succeed");

        assert!(sink.progress_count.load(Ordering::SeqCst) >= 1);
        assert_eq!(sink.finish_count.load(Ordering::SeqCst), 1);
        assert_eq!(
            sink.last_processed.load(Ordering::SeqCst),
            data.len() as u64
        );
    }

    #[test]
    fn test_xz_cancel_forwarding() {
        use oxiarc_core::cancel::CancellationToken;
        use oxiarc_core::error::OxiArcError;

        let token = CancellationToken::new();
        token.cancel();
        let writer = XzWriter::new(LzmaLevel::new(6)).with_cancel(token);
        let data: Vec<u8> = (0..100).map(|_| b'A').collect();
        let result = writer.compress(&data);
        assert!(matches!(result, Err(OxiArcError::Cancelled)));
    }
}
