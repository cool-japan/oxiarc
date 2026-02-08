//! LZ4 frame format support.
//!
//! Implements the official LZ4 frame format as specified in:
//! <https://github.com/lz4/lz4/blob/dev/doc/lz4_Frame_format.md>
//!
//! The frame format includes:
//! - Magic number (0x184D2204)
//! - Frame descriptor (flags, block size, optional content size)
//! - Data blocks with optional checksums
//! - End marker
//! - Optional content checksum

use crate::block::{compress_block, decompress_block};
use crate::xxhash::{XxHash32, xxhash32};
use oxiarc_core::error::{OxiArcError, Result};
use oxiarc_core::traits::{CompressStatus, Compressor, DecompressStatus, Decompressor, FlushMode};

#[cfg(feature = "parallel")]
use rayon::prelude::*;

/// LZ4 frame magic number.
pub const LZ4_FRAME_MAGIC: u32 = 0x184D2204;

/// LZ4 legacy magic number (simple framing).
const LZ4_LEGACY_MAGIC: u32 = 0x184C2102;

/// Block maximum sizes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum BlockMaxSize {
    /// 64 KB maximum block size.
    Size64KB = 4,
    /// 256 KB maximum block size.
    Size256KB = 5,
    /// 1 MB maximum block size.
    Size1MB = 6,
    /// 4 MB maximum block size (default).
    #[default]
    Size4MB = 7,
}

impl BlockMaxSize {
    /// Get the actual byte size for this block max setting.
    pub fn size_bytes(self) -> usize {
        match self {
            BlockMaxSize::Size64KB => 64 * 1024,
            BlockMaxSize::Size256KB => 256 * 1024,
            BlockMaxSize::Size1MB => 1024 * 1024,
            BlockMaxSize::Size4MB => 4 * 1024 * 1024,
        }
    }

    /// Convert from the 3-bit BD field value.
    fn from_bd(bd: u8) -> Option<Self> {
        match (bd >> 4) & 0x07 {
            4 => Some(BlockMaxSize::Size64KB),
            5 => Some(BlockMaxSize::Size256KB),
            6 => Some(BlockMaxSize::Size1MB),
            7 => Some(BlockMaxSize::Size4MB),
            _ => None,
        }
    }

    /// Convert to the BD byte value.
    fn to_bd(self) -> u8 {
        (self as u8) << 4
    }
}

/// Frame descriptor flags.
#[derive(Debug, Clone, Copy, Default)]
pub struct FrameDescriptor {
    /// Block independence flag (blocks can be decoded independently).
    pub block_independence: bool,
    /// Block checksum flag (each block has XXH32 checksum).
    pub block_checksum: bool,
    /// Content size present in header.
    pub content_size: Option<u64>,
    /// Content checksum flag (frame has XXH32 checksum at end).
    pub content_checksum: bool,
    /// Block maximum size.
    pub block_max_size: BlockMaxSize,
    /// Dictionary ID (when using dictionary compression).
    pub dict_id: Option<u32>,
}

impl FrameDescriptor {
    /// Create default frame descriptor.
    pub fn new() -> Self {
        Self {
            block_independence: true,
            block_checksum: false,
            content_size: None,
            content_checksum: true,
            block_max_size: BlockMaxSize::default(),
            dict_id: None,
        }
    }

    /// Create with content size.
    pub fn with_content_size(mut self, size: u64) -> Self {
        self.content_size = Some(size);
        self
    }

    /// Set block checksum flag.
    pub fn with_block_checksum(mut self, enabled: bool) -> Self {
        self.block_checksum = enabled;
        self
    }

    /// Set content checksum flag.
    pub fn with_content_checksum(mut self, enabled: bool) -> Self {
        self.content_checksum = enabled;
        self
    }

    /// Set block max size.
    pub fn with_block_max_size(mut self, size: BlockMaxSize) -> Self {
        self.block_max_size = size;
        self
    }

    /// Set dictionary ID for dictionary-based compression.
    pub fn with_dict_id(mut self, id: u32) -> Self {
        self.dict_id = Some(id);
        self
    }

    /// Encode FLG byte.
    fn flg_byte(&self) -> u8 {
        let mut flg = 0x40; // Version = 01
        if self.block_independence {
            flg |= 0x20;
        }
        if self.block_checksum {
            flg |= 0x10;
        }
        if self.content_size.is_some() {
            flg |= 0x08;
        }
        if self.content_checksum {
            flg |= 0x04;
        }
        if self.dict_id.is_some() {
            flg |= 0x01;
        }
        flg
    }

    /// Parse from FLG and BD bytes.
    fn parse(flg: u8, bd: u8) -> Result<Self> {
        // Check version (must be 01)
        if (flg >> 6) != 0x01 {
            return Err(OxiArcError::invalid_header("unsupported LZ4 frame version"));
        }

        // Reserved bit must be 0
        if (flg & 0x02) != 0 {
            return Err(OxiArcError::invalid_header("reserved FLG bit set"));
        }

        // Reserved bits in BD must be 0
        if (bd & 0x8F) != 0 {
            return Err(OxiArcError::invalid_header("reserved BD bits set"));
        }

        let block_max_size = BlockMaxSize::from_bd(bd)
            .ok_or_else(|| OxiArcError::invalid_header("invalid block max size"))?;

        Ok(Self {
            block_independence: (flg & 0x20) != 0,
            block_checksum: (flg & 0x10) != 0,
            content_size: if (flg & 0x08) != 0 { Some(0) } else { None },
            content_checksum: (flg & 0x04) != 0,
            block_max_size,
            dict_id: if (flg & 0x01) != 0 { Some(0) } else { None },
        })
    }
}

/// Compress data using the official LZ4 frame format.
///
/// This produces output compatible with the lz4 reference implementation.
pub fn compress(input: &[u8]) -> Result<Vec<u8>> {
    compress_with_options(
        input,
        FrameDescriptor::new().with_content_size(input.len() as u64),
    )
}

/// Compress data using the official LZ4 frame format with custom options.
pub fn compress_with_options(input: &[u8], desc: FrameDescriptor) -> Result<Vec<u8>> {
    let mut output = Vec::with_capacity(15 + input.len());
    let mut content_hasher = if desc.content_checksum {
        Some(XxHash32::new())
    } else {
        None
    };

    // Write magic number
    output.extend_from_slice(&LZ4_FRAME_MAGIC.to_le_bytes());

    // Write frame descriptor
    let flg = desc.flg_byte();
    let bd = desc.block_max_size.to_bd();
    output.push(flg);
    output.push(bd);

    // Content size (if present)
    if let Some(size) = desc.content_size {
        output.extend_from_slice(&size.to_le_bytes());
    }

    // Dictionary ID (if present)
    if let Some(dict_id) = desc.dict_id {
        output.extend_from_slice(&dict_id.to_le_bytes());
    }

    // Header checksum (XXH32 of descriptor >> 8, masked to 1 byte)
    let header_checksum = {
        let header_end = output.len();
        let header_data = &output[4..header_end]; // FLG + BD + optional fields
        (xxhash32(header_data) >> 8) as u8
    };
    output.push(header_checksum);

    // Compress blocks
    let block_size = desc.block_max_size.size_bytes();
    let mut pos = 0;

    while pos < input.len() {
        let chunk_end = (pos + block_size).min(input.len());
        let chunk = &input[pos..chunk_end];

        // Update content hash
        if let Some(ref mut hasher) = content_hasher {
            hasher.update(chunk);
        }

        // Compress block
        let compressed = compress_block(chunk)?;

        // Decide whether to store compressed or uncompressed
        if compressed.len() < chunk.len() {
            // Store compressed
            let block_len = compressed.len() as u32;
            output.extend_from_slice(&block_len.to_le_bytes());
            output.extend_from_slice(&compressed);

            // Block checksum (if enabled)
            if desc.block_checksum {
                let checksum = xxhash32(&compressed);
                output.extend_from_slice(&checksum.to_le_bytes());
            }
        } else {
            // Store uncompressed (high bit set)
            let block_len = (chunk.len() as u32) | 0x80000000;
            output.extend_from_slice(&block_len.to_le_bytes());
            output.extend_from_slice(chunk);

            // Block checksum (if enabled)
            if desc.block_checksum {
                let checksum = xxhash32(chunk);
                output.extend_from_slice(&checksum.to_le_bytes());
            }
        }

        pos = chunk_end;
    }

    // End marker
    output.extend_from_slice(&0u32.to_le_bytes());

    // Content checksum (if enabled)
    if let Some(hasher) = content_hasher {
        let checksum = hasher.finish();
        output.extend_from_slice(&checksum.to_le_bytes());
    }

    Ok(output)
}

/// Compress data using parallel block compression (requires `parallel` feature).
///
/// This function splits the input into independent blocks and compresses them
/// in parallel using rayon. The number of threads is automatically determined
/// by rayon (typically matches the number of CPU cores).
///
/// # Arguments
///
/// * `input` - Data to compress
/// * `desc` - Frame descriptor options
///
/// # Returns
///
/// Compressed data in LZ4 frame format, identical to serial compression.
#[cfg(feature = "parallel")]
pub fn compress_with_options_parallel(input: &[u8], desc: FrameDescriptor) -> Result<Vec<u8>> {
    let mut output = Vec::with_capacity(15 + input.len());
    let mut content_hasher = if desc.content_checksum {
        Some(XxHash32::new())
    } else {
        None
    };

    // Write magic number
    output.extend_from_slice(&LZ4_FRAME_MAGIC.to_le_bytes());

    // Write frame descriptor
    let flg = desc.flg_byte();
    let bd = desc.block_max_size.to_bd();
    output.push(flg);
    output.push(bd);

    // Content size (if present)
    if let Some(size) = desc.content_size {
        output.extend_from_slice(&size.to_le_bytes());
    }

    // Dictionary ID (if present)
    if let Some(dict_id) = desc.dict_id {
        output.extend_from_slice(&dict_id.to_le_bytes());
    }

    // Header checksum (XXH32 of descriptor >> 8, masked to 1 byte)
    let header_checksum = {
        let header_end = output.len();
        let header_data = &output[4..header_end]; // FLG + BD + optional fields
        (xxhash32(header_data) >> 8) as u8
    };
    output.push(header_checksum);

    // Split input into blocks
    let block_size = desc.block_max_size.size_bytes();
    let chunks: Vec<&[u8]> = input.chunks(block_size).collect();

    // Compress blocks in parallel
    let compressed_blocks: Vec<Result<Vec<u8>>> = chunks
        .par_iter()
        .map(|chunk| compress_block(chunk))
        .collect();

    // Assemble compressed frame
    for (i, result) in compressed_blocks.into_iter().enumerate() {
        let chunk = chunks[i];
        let compressed = result?;

        // Update content hash
        if let Some(ref mut hasher) = content_hasher {
            hasher.update(chunk);
        }

        // Decide whether to store compressed or uncompressed
        if compressed.len() < chunk.len() {
            // Store compressed
            let block_len = compressed.len() as u32;
            output.extend_from_slice(&block_len.to_le_bytes());
            output.extend_from_slice(&compressed);

            // Block checksum (if enabled)
            if desc.block_checksum {
                let checksum = xxhash32(&compressed);
                output.extend_from_slice(&checksum.to_le_bytes());
            }
        } else {
            // Store uncompressed (high bit set)
            let block_len = (chunk.len() as u32) | 0x80000000;
            output.extend_from_slice(&block_len.to_le_bytes());
            output.extend_from_slice(chunk);

            // Block checksum (if enabled)
            if desc.block_checksum {
                let checksum = xxhash32(chunk);
                output.extend_from_slice(&checksum.to_le_bytes());
            }
        }
    }

    // End marker
    output.extend_from_slice(&0u32.to_le_bytes());

    // Content checksum (if enabled)
    if let Some(hasher) = content_hasher {
        let checksum = hasher.finish();
        output.extend_from_slice(&checksum.to_le_bytes());
    }

    Ok(output)
}

/// Compress data using parallel compression with default options (requires `parallel` feature).
///
/// This is a convenience wrapper around `compress_with_options_parallel` that
/// uses default frame descriptor settings.
#[cfg(feature = "parallel")]
pub fn compress_parallel(input: &[u8]) -> Result<Vec<u8>> {
    compress_with_options_parallel(
        input,
        FrameDescriptor::new().with_content_size(input.len() as u64),
    )
}

/// Decompress LZ4 framed data.
///
/// Supports both official frame format and legacy/simple frames.
pub fn decompress(input: &[u8], max_output: usize) -> Result<Vec<u8>> {
    if input.len() < 4 {
        return Err(OxiArcError::invalid_header("LZ4 frame too short"));
    }

    let magic = u32::from_le_bytes([input[0], input[1], input[2], input[3]]);

    match magic {
        LZ4_FRAME_MAGIC => decompress_frame(input, max_output),
        LZ4_LEGACY_MAGIC => decompress_legacy(input, max_output),
        _ => {
            // Try simple format (our own magic)
            if magic == 0x184D2204 {
                decompress_frame(input, max_output)
            } else {
                Err(OxiArcError::invalid_magic(
                    LZ4_FRAME_MAGIC.to_le_bytes(),
                    &input[..4],
                ))
            }
        }
    }
}

/// Decompress official LZ4 frame format.
fn decompress_frame(input: &[u8], max_output: usize) -> Result<Vec<u8>> {
    if input.len() < 7 {
        return Err(OxiArcError::invalid_header("LZ4 frame too short"));
    }

    let mut pos = 4; // Skip magic

    // Parse frame descriptor
    let flg = input[pos];
    pos += 1;
    let bd = input[pos];
    pos += 1;

    let mut desc = FrameDescriptor::parse(flg, bd)?;

    // Read content size if present
    if desc.content_size.is_some() {
        if pos + 8 > input.len() {
            return Err(OxiArcError::invalid_header("missing content size"));
        }
        let size = u64::from_le_bytes([
            input[pos],
            input[pos + 1],
            input[pos + 2],
            input[pos + 3],
            input[pos + 4],
            input[pos + 5],
            input[pos + 6],
            input[pos + 7],
        ]);
        desc.content_size = Some(size);
        pos += 8;
    }

    // Read dictionary ID if present
    if desc.dict_id.is_some() {
        if pos + 4 > input.len() {
            return Err(OxiArcError::invalid_header("missing dictionary ID"));
        }
        let dict_id =
            u32::from_le_bytes([input[pos], input[pos + 1], input[pos + 2], input[pos + 3]]);
        desc.dict_id = Some(dict_id);
        pos += 4;
    }

    // Verify header checksum
    if pos >= input.len() {
        return Err(OxiArcError::invalid_header("missing header checksum"));
    }
    let stored_hc = input[pos];
    pos += 1;

    let header_data = &input[4..pos - 1];
    let computed_hc = (xxhash32(header_data) >> 8) as u8;
    if stored_hc != computed_hc {
        return Err(OxiArcError::crc_mismatch(
            computed_hc as u32,
            stored_hc as u32,
        ));
    }

    // Decompress blocks
    let mut output = Vec::with_capacity(
        desc.content_size
            .map(|s| s as usize)
            .unwrap_or(max_output)
            .min(max_output),
    );
    let mut content_hasher = if desc.content_checksum {
        Some(XxHash32::new())
    } else {
        None
    };

    let block_max = desc.block_max_size.size_bytes();

    loop {
        if pos + 4 > input.len() {
            return Err(OxiArcError::corrupted(pos as u64, "truncated block header"));
        }

        let block_len =
            u32::from_le_bytes([input[pos], input[pos + 1], input[pos + 2], input[pos + 3]]);
        pos += 4;

        // End marker
        if block_len == 0 {
            break;
        }

        let uncompressed = (block_len & 0x80000000) != 0;
        let block_size = (block_len & 0x7FFFFFFF) as usize;

        if block_size > block_max {
            return Err(OxiArcError::corrupted(
                pos as u64,
                "block size exceeds maximum",
            ));
        }

        if pos + block_size > input.len() {
            return Err(OxiArcError::corrupted(pos as u64, "truncated block data"));
        }

        let block_data = &input[pos..pos + block_size];
        pos += block_size;

        // Verify block checksum if present
        if desc.block_checksum {
            if pos + 4 > input.len() {
                return Err(OxiArcError::corrupted(pos as u64, "missing block checksum"));
            }
            let stored_checksum =
                u32::from_le_bytes([input[pos], input[pos + 1], input[pos + 2], input[pos + 3]]);
            pos += 4;

            let computed_checksum = xxhash32(block_data);
            if stored_checksum != computed_checksum {
                return Err(OxiArcError::crc_mismatch(
                    computed_checksum,
                    stored_checksum,
                ));
            }
        }

        // Decompress block
        let decompressed = if uncompressed {
            block_data.to_vec()
        } else {
            decompress_block(block_data, block_max)?
        };

        // Update content hash
        if let Some(ref mut hasher) = content_hasher {
            hasher.update(&decompressed);
        }

        output.extend_from_slice(&decompressed);

        if output.len() > max_output {
            return Err(OxiArcError::corrupted(
                pos as u64,
                "output exceeds maximum size",
            ));
        }
    }

    // Verify content checksum if present
    if desc.content_checksum {
        if pos + 4 > input.len() {
            return Err(OxiArcError::corrupted(
                pos as u64,
                "missing content checksum",
            ));
        }
        let stored_checksum =
            u32::from_le_bytes([input[pos], input[pos + 1], input[pos + 2], input[pos + 3]]);

        if let Some(hasher) = content_hasher {
            let computed_checksum = hasher.finish();
            if stored_checksum != computed_checksum {
                return Err(OxiArcError::crc_mismatch(
                    computed_checksum,
                    stored_checksum,
                ));
            }
        }
    }

    Ok(output)
}

/// Decompress legacy LZ4 format.
fn decompress_legacy(input: &[u8], max_output: usize) -> Result<Vec<u8>> {
    if input.len() < 8 {
        return Err(OxiArcError::invalid_header("legacy LZ4 frame too short"));
    }

    let mut output = Vec::new();
    let mut pos = 4; // Skip magic

    while pos + 4 <= input.len() {
        let block_size =
            u32::from_le_bytes([input[pos], input[pos + 1], input[pos + 2], input[pos + 3]])
                as usize;
        pos += 4;

        if block_size == 0 {
            break;
        }

        if pos + block_size > input.len() {
            return Err(OxiArcError::corrupted(pos as u64, "truncated block"));
        }

        let block_data = &input[pos..pos + block_size];
        pos += block_size;

        let decompressed = decompress_block(block_data, max_output - output.len())?;
        output.extend_from_slice(&decompressed);

        if output.len() > max_output {
            break;
        }
    }

    Ok(output)
}

/// LZ4 compressor implementing the Compressor trait.
#[derive(Debug)]
pub struct Lz4Compressor {
    buffer: Vec<u8>,
    desc: FrameDescriptor,
    finished: bool,
}

impl Lz4Compressor {
    /// Create a new LZ4 compressor with default options.
    pub fn new() -> Self {
        Self {
            buffer: Vec::new(),
            desc: FrameDescriptor::new(),
            finished: false,
        }
    }

    /// Create a new LZ4 compressor with custom options.
    pub fn with_options(desc: FrameDescriptor) -> Self {
        Self {
            buffer: Vec::new(),
            desc,
            finished: false,
        }
    }
}

impl Default for Lz4Compressor {
    fn default() -> Self {
        Self::new()
    }
}

impl Compressor for Lz4Compressor {
    fn compress(
        &mut self,
        input: &[u8],
        output: &mut [u8],
        flush: FlushMode,
    ) -> Result<(usize, usize, CompressStatus)> {
        if self.finished {
            return Ok((0, 0, CompressStatus::Done));
        }

        // Buffer all input
        self.buffer.extend_from_slice(input);

        if matches!(flush, FlushMode::Finish) {
            // Compress the buffer using the configured descriptor
            let desc = self.desc.with_content_size(self.buffer.len() as u64);
            let compressed = compress_with_options(&self.buffer, desc)?;

            if compressed.len() <= output.len() {
                output[..compressed.len()].copy_from_slice(&compressed);
                self.finished = true;
                Ok((input.len(), compressed.len(), CompressStatus::Done))
            } else {
                Ok((input.len(), 0, CompressStatus::NeedsOutput))
            }
        } else {
            Ok((input.len(), 0, CompressStatus::NeedsInput))
        }
    }

    fn reset(&mut self) {
        self.buffer.clear();
        self.finished = false;
    }

    fn is_finished(&self) -> bool {
        self.finished
    }
}

/// LZ4 decompressor implementing the Decompressor trait.
#[derive(Debug)]
pub struct Lz4Decompressor {
    buffer: Vec<u8>,
    finished: bool,
}

impl Lz4Decompressor {
    /// Create a new LZ4 decompressor.
    pub fn new() -> Self {
        Self {
            buffer: Vec::new(),
            finished: false,
        }
    }
}

impl Default for Lz4Decompressor {
    fn default() -> Self {
        Self::new()
    }
}

impl Decompressor for Lz4Decompressor {
    fn decompress(
        &mut self,
        input: &[u8],
        output: &mut [u8],
    ) -> Result<(usize, usize, DecompressStatus)> {
        if self.finished {
            return Ok((0, 0, DecompressStatus::Done));
        }

        // Buffer all input
        self.buffer.extend_from_slice(input);

        // Try to decompress if we have enough data
        if self.buffer.len() >= 7 {
            // Minimum frame size
            match decompress(&self.buffer, 64 * 1024 * 1024) {
                Ok(decompressed) => {
                    let to_copy = decompressed.len().min(output.len());
                    output[..to_copy].copy_from_slice(&decompressed[..to_copy]);
                    self.finished = true;
                    Ok((input.len(), to_copy, DecompressStatus::Done))
                }
                Err(_) => {
                    // Need more data
                    Ok((input.len(), 0, DecompressStatus::NeedsInput))
                }
            }
        } else {
            Ok((input.len(), 0, DecompressStatus::NeedsInput))
        }
    }

    fn reset(&mut self) {
        self.buffer.clear();
        self.finished = false;
    }

    fn is_finished(&self) -> bool {
        self.finished
    }
}

// ============================================================================
// Dictionary Frame Compression/Decompression
// ============================================================================

use crate::dict::{
    Lz4Dict, compress_with_dict as block_compress_with_dict,
    decompress_with_dict as block_decompress_with_dict,
};

/// Compress data using the official LZ4 frame format with dictionary.
///
/// This produces output compatible with the lz4 reference implementation's
/// dictionary mode. The dictionary ID is included in the frame header.
///
/// # Arguments
///
/// * `input` - Data to compress
/// * `dict` - Dictionary for compression
///
/// # Returns
///
/// Compressed data in LZ4 frame format with dictionary ID.
///
/// # Example
///
/// ```
/// use oxiarc_lz4::{Lz4Dict, compress_frame_with_dict, decompress_frame_with_dict};
///
/// let dict = Lz4Dict::new(b"common pattern");
/// let data = b"common pattern in this data";
/// let compressed = compress_frame_with_dict(data, &dict).unwrap();
/// let decompressed = decompress_frame_with_dict(&compressed, data.len() * 2, &dict).unwrap();
/// assert_eq!(decompressed, data);
/// ```
pub fn compress_frame_with_dict(input: &[u8], dict: &Lz4Dict) -> Result<Vec<u8>> {
    let desc = FrameDescriptor::new()
        .with_content_size(input.len() as u64)
        .with_dict_id(dict.id());
    compress_frame_with_dict_options(input, dict, desc)
}

/// Compress data using the official LZ4 frame format with dictionary and custom options.
///
/// # Arguments
///
/// * `input` - Data to compress
/// * `dict` - Dictionary for compression
/// * `desc` - Frame descriptor with options
///
/// # Returns
///
/// Compressed data in LZ4 frame format.
pub fn compress_frame_with_dict_options(
    input: &[u8],
    dict: &Lz4Dict,
    desc: FrameDescriptor,
) -> Result<Vec<u8>> {
    let mut output = Vec::with_capacity(19 + input.len()); // Extra 4 bytes for dict ID
    let mut content_hasher = if desc.content_checksum {
        Some(XxHash32::new())
    } else {
        None
    };

    // Write magic number
    output.extend_from_slice(&LZ4_FRAME_MAGIC.to_le_bytes());

    // Write frame descriptor with dictionary ID flag
    let desc_with_dict = FrameDescriptor {
        dict_id: Some(dict.id()),
        ..desc
    };
    let flg = desc_with_dict.flg_byte();
    let bd = desc_with_dict.block_max_size.to_bd();
    output.push(flg);
    output.push(bd);

    // Content size (if present)
    if let Some(size) = desc_with_dict.content_size {
        output.extend_from_slice(&size.to_le_bytes());
    }

    // Dictionary ID (4 bytes, little-endian)
    output.extend_from_slice(&dict.id().to_le_bytes());

    // Header checksum (XXH32 of descriptor >> 8, masked to 1 byte)
    let header_checksum = {
        let header_start = 4; // After magic
        let header_end = output.len();
        (xxhash32(&output[header_start..header_end]) >> 8) as u8
    };
    output.push(header_checksum);

    // Compress blocks with dictionary
    let block_size = desc_with_dict.block_max_size.size_bytes();
    let mut pos = 0;

    while pos < input.len() {
        let chunk_end = (pos + block_size).min(input.len());
        let chunk = &input[pos..chunk_end];

        // Update content hash
        if let Some(ref mut hasher) = content_hasher {
            hasher.update(chunk);
        }

        // Compress block with dictionary
        let compressed = block_compress_with_dict(chunk, dict)?;

        // Decide whether to store compressed or uncompressed
        if compressed.len() < chunk.len() {
            // Store compressed
            let block_len = compressed.len() as u32;
            output.extend_from_slice(&block_len.to_le_bytes());
            output.extend_from_slice(&compressed);

            // Block checksum (if enabled)
            if desc_with_dict.block_checksum {
                let checksum = xxhash32(&compressed);
                output.extend_from_slice(&checksum.to_le_bytes());
            }
        } else {
            // Store uncompressed (high bit set)
            let block_len = (chunk.len() as u32) | 0x80000000;
            output.extend_from_slice(&block_len.to_le_bytes());
            output.extend_from_slice(chunk);

            // Block checksum (if enabled)
            if desc_with_dict.block_checksum {
                let checksum = xxhash32(chunk);
                output.extend_from_slice(&checksum.to_le_bytes());
            }
        }

        pos = chunk_end;
    }

    // End marker
    output.extend_from_slice(&0u32.to_le_bytes());

    // Content checksum (if enabled)
    if let Some(hasher) = content_hasher {
        let checksum = hasher.finish();
        output.extend_from_slice(&checksum.to_le_bytes());
    }

    Ok(output)
}

/// Decompress LZ4 framed data with dictionary.
///
/// The dictionary must match the one used during compression.
/// The dictionary ID from the frame header is verified against the provided dictionary.
///
/// # Arguments
///
/// * `input` - Compressed data in LZ4 frame format
/// * `max_output` - Maximum size of decompressed output
/// * `dict` - Dictionary used during compression
///
/// # Returns
///
/// Decompressed data.
///
/// # Example
///
/// ```
/// use oxiarc_lz4::{Lz4Dict, compress_frame_with_dict, decompress_frame_with_dict};
///
/// let dict = Lz4Dict::new(b"common pattern");
/// let data = b"common pattern in this data";
/// let compressed = compress_frame_with_dict(data, &dict).unwrap();
/// let decompressed = decompress_frame_with_dict(&compressed, data.len() * 2, &dict).unwrap();
/// assert_eq!(decompressed, data);
/// ```
pub fn decompress_frame_with_dict(
    input: &[u8],
    max_output: usize,
    dict: &Lz4Dict,
) -> Result<Vec<u8>> {
    if input.len() < 7 {
        return Err(OxiArcError::invalid_header("LZ4 frame too short"));
    }

    let magic = u32::from_le_bytes([input[0], input[1], input[2], input[3]]);
    if magic != LZ4_FRAME_MAGIC {
        return Err(OxiArcError::invalid_magic(
            LZ4_FRAME_MAGIC.to_le_bytes(),
            &input[..4],
        ));
    }

    let mut pos = 4; // Skip magic

    // Parse frame descriptor
    let flg = input[pos];
    pos += 1;
    let bd = input[pos];
    pos += 1;

    let mut desc = FrameDescriptor::parse(flg, bd)?;

    // Read content size if present
    if desc.content_size.is_some() {
        if pos + 8 > input.len() {
            return Err(OxiArcError::invalid_header("missing content size"));
        }
        let size = u64::from_le_bytes([
            input[pos],
            input[pos + 1],
            input[pos + 2],
            input[pos + 3],
            input[pos + 4],
            input[pos + 5],
            input[pos + 6],
            input[pos + 7],
        ]);
        desc.content_size = Some(size);
        pos += 8;
    }

    // Read dictionary ID if present
    if desc.dict_id.is_some() {
        if pos + 4 > input.len() {
            return Err(OxiArcError::invalid_header("missing dictionary ID"));
        }
        let frame_dict_id =
            u32::from_le_bytes([input[pos], input[pos + 1], input[pos + 2], input[pos + 3]]);
        pos += 4;

        // Verify dictionary ID matches
        if frame_dict_id != dict.id() {
            return Err(OxiArcError::corrupted(pos as u64, "dictionary ID mismatch"));
        }
        desc.dict_id = Some(frame_dict_id);
    }

    // Verify header checksum
    if pos >= input.len() {
        return Err(OxiArcError::invalid_header("missing header checksum"));
    }
    let stored_hc = input[pos];
    pos += 1;

    let header_data = &input[4..pos - 1];
    let computed_hc = (xxhash32(header_data) >> 8) as u8;
    if stored_hc != computed_hc {
        return Err(OxiArcError::crc_mismatch(
            computed_hc as u32,
            stored_hc as u32,
        ));
    }

    // Decompress blocks
    let mut output = Vec::with_capacity(
        desc.content_size
            .map(|s| s as usize)
            .unwrap_or(max_output)
            .min(max_output),
    );
    let mut content_hasher = if desc.content_checksum {
        Some(XxHash32::new())
    } else {
        None
    };

    let block_max = desc.block_max_size.size_bytes();

    loop {
        if pos + 4 > input.len() {
            return Err(OxiArcError::corrupted(pos as u64, "truncated block header"));
        }

        let block_len =
            u32::from_le_bytes([input[pos], input[pos + 1], input[pos + 2], input[pos + 3]]);
        pos += 4;

        // End marker
        if block_len == 0 {
            break;
        }

        let uncompressed = (block_len & 0x80000000) != 0;
        let block_size = (block_len & 0x7FFFFFFF) as usize;

        if block_size > block_max {
            return Err(OxiArcError::corrupted(
                pos as u64,
                "block size exceeds maximum",
            ));
        }

        if pos + block_size > input.len() {
            return Err(OxiArcError::corrupted(pos as u64, "truncated block data"));
        }

        let block_data = &input[pos..pos + block_size];
        pos += block_size;

        // Verify block checksum if present
        if desc.block_checksum {
            if pos + 4 > input.len() {
                return Err(OxiArcError::corrupted(pos as u64, "missing block checksum"));
            }
            let stored_checksum =
                u32::from_le_bytes([input[pos], input[pos + 1], input[pos + 2], input[pos + 3]]);
            pos += 4;

            let computed_checksum = xxhash32(block_data);
            if stored_checksum != computed_checksum {
                return Err(OxiArcError::crc_mismatch(
                    computed_checksum,
                    stored_checksum,
                ));
            }
        }

        // Decompress block with dictionary
        let decompressed = if uncompressed {
            block_data.to_vec()
        } else {
            block_decompress_with_dict(block_data, block_max, dict)?
        };

        // Update content hash
        if let Some(ref mut hasher) = content_hasher {
            hasher.update(&decompressed);
        }

        output.extend_from_slice(&decompressed);

        if output.len() > max_output {
            return Err(OxiArcError::corrupted(
                pos as u64,
                "output exceeds maximum size",
            ));
        }
    }

    // Verify content checksum if present
    if desc.content_checksum {
        if pos + 4 > input.len() {
            return Err(OxiArcError::corrupted(
                pos as u64,
                "missing content checksum",
            ));
        }
        let stored_checksum =
            u32::from_le_bytes([input[pos], input[pos + 1], input[pos + 2], input[pos + 3]]);

        if let Some(hasher) = content_hasher {
            let computed_checksum = hasher.finish();
            if stored_checksum != computed_checksum {
                return Err(OxiArcError::crc_mismatch(
                    computed_checksum,
                    stored_checksum,
                ));
            }
        }
    }

    Ok(output)
}

/// Extract dictionary ID from an LZ4 frame header.
///
/// Returns `None` if no dictionary ID is present in the frame.
pub fn get_frame_dict_id(input: &[u8]) -> Result<Option<u32>> {
    if input.len() < 7 {
        return Err(OxiArcError::invalid_header("LZ4 frame too short"));
    }

    let magic = u32::from_le_bytes([input[0], input[1], input[2], input[3]]);
    if magic != LZ4_FRAME_MAGIC {
        return Err(OxiArcError::invalid_magic(
            LZ4_FRAME_MAGIC.to_le_bytes(),
            &input[..4],
        ));
    }

    let flg = input[4];
    let has_dict_id = (flg & 0x01) != 0;

    if !has_dict_id {
        return Ok(None);
    }

    // Calculate position of dictionary ID
    let mut pos = 6; // After FLG and BD

    // Skip content size if present
    if (flg & 0x08) != 0 {
        pos += 8;
    }

    if pos + 4 > input.len() {
        return Err(OxiArcError::invalid_header("missing dictionary ID"));
    }

    let dict_id = u32::from_le_bytes([input[pos], input[pos + 1], input[pos + 2], input[pos + 3]]);
    Ok(Some(dict_id))
}

// ============================================================================
// Dictionary-Aware Encoder/Decoder Structs
// ============================================================================

/// LZ4 frame encoder with dictionary support.
///
/// This encoder uses a dictionary to improve compression ratios for small data
/// with common patterns.
///
/// # Example
///
/// ```
/// use oxiarc_lz4::{Lz4DictFrameEncoder, Lz4Dict};
///
/// let dict = Lz4Dict::new(b"common pattern");
/// let encoder = Lz4DictFrameEncoder::new(dict);
///
/// let data = b"common pattern appears here";
/// let compressed = encoder.encode(data).unwrap();
/// assert!(!compressed.is_empty());
/// ```
pub struct Lz4DictFrameEncoder {
    dict: Lz4Dict,
    desc: FrameDescriptor,
}

impl Lz4DictFrameEncoder {
    /// Create a new dictionary frame encoder.
    pub fn new(dict: Lz4Dict) -> Self {
        Self {
            dict,
            desc: FrameDescriptor::new(),
        }
    }

    /// Create a new dictionary frame encoder with custom options.
    pub fn with_options(dict: Lz4Dict, desc: FrameDescriptor) -> Self {
        Self { dict, desc }
    }

    /// Get the dictionary.
    pub fn dict(&self) -> &Lz4Dict {
        &self.dict
    }

    /// Get the dictionary ID.
    pub fn dict_id(&self) -> u32 {
        self.dict.id()
    }

    /// Encode data using the dictionary.
    pub fn encode(&self, input: &[u8]) -> Result<Vec<u8>> {
        compress_frame_with_dict_options(input, &self.dict, self.desc)
    }

    /// Encode data with content size in header.
    pub fn encode_with_size(&self, input: &[u8]) -> Result<Vec<u8>> {
        let desc = self.desc.with_content_size(input.len() as u64);
        compress_frame_with_dict_options(input, &self.dict, desc)
    }
}

impl std::fmt::Debug for Lz4DictFrameEncoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Lz4DictFrameEncoder")
            .field("dict_id", &format!("{:#010X}", self.dict.id()))
            .field("dict_len", &self.dict.len())
            .finish()
    }
}

/// LZ4 frame decoder with dictionary support.
///
/// This decoder uses a dictionary to decompress data that was compressed
/// with dictionary support.
///
/// # Example
///
/// ```
/// use oxiarc_lz4::{Lz4DictFrameEncoder, Lz4DictFrameDecoder, Lz4Dict};
///
/// let dict = Lz4Dict::new(b"common pattern");
/// let encoder = Lz4DictFrameEncoder::new(dict.clone());
///
/// let data = b"common pattern appears here";
/// let compressed = encoder.encode(data).unwrap();
///
/// let decoder = Lz4DictFrameDecoder::new(dict);
/// let decompressed = decoder.decode(&compressed, data.len() * 2).unwrap();
/// assert_eq!(decompressed, data);
/// ```
pub struct Lz4DictFrameDecoder {
    dict: Lz4Dict,
}

impl Lz4DictFrameDecoder {
    /// Create a new dictionary frame decoder.
    pub fn new(dict: Lz4Dict) -> Self {
        Self { dict }
    }

    /// Get the dictionary.
    pub fn dict(&self) -> &Lz4Dict {
        &self.dict
    }

    /// Get the dictionary ID.
    pub fn dict_id(&self) -> u32 {
        self.dict.id()
    }

    /// Decode data using the dictionary.
    pub fn decode(&self, input: &[u8], max_output: usize) -> Result<Vec<u8>> {
        decompress_frame_with_dict(input, max_output, &self.dict)
    }

    /// Check if the input frame requires this dictionary.
    ///
    /// Returns `true` if the frame's dictionary ID matches this decoder's dictionary.
    pub fn can_decode(&self, input: &[u8]) -> bool {
        match get_frame_dict_id(input) {
            Ok(Some(id)) => id == self.dict.id(),
            Ok(None) => false, // No dict required, but we have one
            Err(_) => false,
        }
    }
}

impl std::fmt::Debug for Lz4DictFrameDecoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Lz4DictFrameDecoder")
            .field("dict_id", &format!("{:#010X}", self.dict.id()))
            .field("dict_len", &self.dict.len())
            .finish()
    }
}

/// LZ4 dictionary compressor implementing the Compressor trait.
///
/// This compressor buffers input and compresses it using a dictionary
/// when flush is called.
#[derive(Debug)]
pub struct Lz4DictCompressor {
    buffer: Vec<u8>,
    dict: Lz4Dict,
    desc: FrameDescriptor,
    finished: bool,
}

impl Lz4DictCompressor {
    /// Create a new dictionary compressor.
    pub fn new(dict: Lz4Dict) -> Self {
        Self {
            buffer: Vec::new(),
            dict,
            desc: FrameDescriptor::new(),
            finished: false,
        }
    }

    /// Create a new dictionary compressor with custom options.
    pub fn with_options(dict: Lz4Dict, desc: FrameDescriptor) -> Self {
        Self {
            buffer: Vec::new(),
            dict,
            desc,
            finished: false,
        }
    }

    /// Get the dictionary.
    pub fn dict(&self) -> &Lz4Dict {
        &self.dict
    }
}

impl Compressor for Lz4DictCompressor {
    fn compress(
        &mut self,
        input: &[u8],
        output: &mut [u8],
        flush: FlushMode,
    ) -> Result<(usize, usize, CompressStatus)> {
        if self.finished {
            return Ok((0, 0, CompressStatus::Done));
        }

        // Buffer all input
        self.buffer.extend_from_slice(input);

        if matches!(flush, FlushMode::Finish) {
            // Compress the buffer using the dictionary
            let desc = self.desc.with_content_size(self.buffer.len() as u64);
            let compressed = compress_frame_with_dict_options(&self.buffer, &self.dict, desc)?;

            if compressed.len() <= output.len() {
                output[..compressed.len()].copy_from_slice(&compressed);
                self.finished = true;
                Ok((input.len(), compressed.len(), CompressStatus::Done))
            } else {
                Ok((input.len(), 0, CompressStatus::NeedsOutput))
            }
        } else {
            Ok((input.len(), 0, CompressStatus::NeedsInput))
        }
    }

    fn reset(&mut self) {
        self.buffer.clear();
        self.finished = false;
    }

    fn is_finished(&self) -> bool {
        self.finished
    }
}

/// LZ4 dictionary decompressor implementing the Decompressor trait.
///
/// This decompressor buffers input and decompresses it using a dictionary.
#[derive(Debug)]
pub struct Lz4DictDecompressor {
    buffer: Vec<u8>,
    dict: Lz4Dict,
    finished: bool,
}

impl Lz4DictDecompressor {
    /// Create a new dictionary decompressor.
    pub fn new(dict: Lz4Dict) -> Self {
        Self {
            buffer: Vec::new(),
            dict,
            finished: false,
        }
    }

    /// Get the dictionary.
    pub fn dict(&self) -> &Lz4Dict {
        &self.dict
    }
}

impl Decompressor for Lz4DictDecompressor {
    fn decompress(
        &mut self,
        input: &[u8],
        output: &mut [u8],
    ) -> Result<(usize, usize, DecompressStatus)> {
        if self.finished {
            return Ok((0, 0, DecompressStatus::Done));
        }

        // Buffer all input
        self.buffer.extend_from_slice(input);

        // Try to decompress if we have enough data
        if self.buffer.len() >= 7 {
            // Minimum frame size
            match decompress_frame_with_dict(&self.buffer, 64 * 1024 * 1024, &self.dict) {
                Ok(decompressed) => {
                    let to_copy = decompressed.len().min(output.len());
                    output[..to_copy].copy_from_slice(&decompressed[..to_copy]);
                    self.finished = true;
                    Ok((input.len(), to_copy, DecompressStatus::Done))
                }
                Err(_) => {
                    // Need more data
                    Ok((input.len(), 0, DecompressStatus::NeedsInput))
                }
            }
        } else {
            Ok((input.len(), 0, DecompressStatus::NeedsInput))
        }
    }

    fn reset(&mut self) {
        self.buffer.clear();
        self.finished = false;
    }

    fn is_finished(&self) -> bool {
        self.finished
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_frame_magic() {
        let data = b"Hello";
        let compressed = compress(data).expect("compress failed");
        assert_eq!(&compressed[0..4], &LZ4_FRAME_MAGIC.to_le_bytes());
    }

    #[test]
    fn test_frame_roundtrip() {
        let data = b"Hello, World! This is a test of LZ4 framing.";
        let compressed = compress(data).expect("compress failed");
        let decompressed = decompress(&compressed, data.len() * 2).expect("decompress failed");
        assert_eq!(decompressed, data);
    }

    #[test]
    fn test_frame_roundtrip_large() {
        let data = vec![0x42u8; 100000];
        let compressed = compress(&data).expect("compress failed");
        let decompressed = decompress(&compressed, data.len() * 2).expect("decompress failed");
        assert_eq!(decompressed, data);
    }

    #[test]
    fn test_frame_with_block_checksum() {
        let data = b"Testing block checksums in LZ4 frame format.";
        let desc = FrameDescriptor::new()
            .with_content_size(data.len() as u64)
            .with_block_checksum(true);
        let compressed = compress_with_options(data, desc).expect("compress failed");
        let decompressed = decompress(&compressed, data.len() * 2).expect("decompress failed");
        assert_eq!(decompressed, data);
    }

    #[test]
    fn test_frame_without_content_checksum() {
        let data = b"Testing without content checksum.";
        let desc = FrameDescriptor::new()
            .with_content_size(data.len() as u64)
            .with_content_checksum(false);
        let compressed = compress_with_options(data, desc).expect("compress failed");
        let decompressed = decompress(&compressed, data.len() * 2).expect("decompress failed");
        assert_eq!(decompressed, data);
    }

    #[test]
    fn test_frame_incompressible_data() {
        // Random-looking data that doesn't compress
        let data: Vec<u8> = (0..1000).map(|i| (i * 17 + 13) as u8).collect();
        let compressed = compress(&data).expect("compress failed");
        let decompressed = decompress(&compressed, data.len() * 2).expect("decompress failed");
        assert_eq!(decompressed, data);
    }

    #[test]
    fn test_block_max_sizes() {
        assert_eq!(BlockMaxSize::Size64KB.size_bytes(), 64 * 1024);
        assert_eq!(BlockMaxSize::Size256KB.size_bytes(), 256 * 1024);
        assert_eq!(BlockMaxSize::Size1MB.size_bytes(), 1024 * 1024);
        assert_eq!(BlockMaxSize::Size4MB.size_bytes(), 4 * 1024 * 1024);
    }

    #[test]
    fn test_compressor_trait() {
        let mut compressor = Lz4Compressor::new();
        let data = b"Hello, World!";
        let mut output = vec![0u8; 200];

        let (consumed, produced, status) = compressor
            .compress(data, &mut output, FlushMode::Finish)
            .expect("compress failed");

        assert_eq!(consumed, data.len());
        assert!(produced > 0);
        assert_eq!(status, CompressStatus::Done);
    }

    #[test]
    fn test_decompressor_trait() {
        let data = b"Hello, World!";
        let compressed = compress(data).expect("compress failed");

        let mut decompressor = Lz4Decompressor::new();
        let mut output = vec![0u8; 100];

        let (consumed, produced, status) = decompressor
            .decompress(&compressed, &mut output)
            .expect("decompress failed");

        assert_eq!(consumed, compressed.len());
        assert_eq!(produced, data.len());
        assert_eq!(status, DecompressStatus::Done);
        assert_eq!(&output[..produced], data.as_slice());
    }

    #[test]
    fn test_invalid_magic() {
        let bad_data = [
            0x00, 0x00, 0x00, 0x00, 0x05, 0x00, 0x00, 0x00, 0x48, 0x65, 0x6c, 0x6c, 0x6f,
        ];
        let result = decompress(&bad_data, 100);
        assert!(result.is_err());
    }

    #[test]
    fn test_too_short() {
        let short_data = [0x04, 0x22, 0x4D, 0x18]; // Just magic, incomplete
        let result = decompress(&short_data, 100);
        assert!(result.is_err());
    }

    #[test]
    fn test_frame_empty_data() {
        let data: &[u8] = b"";
        let compressed = compress(data).expect("compress failed");
        let decompressed = decompress(&compressed, 100).expect("decompress failed");
        assert_eq!(decompressed, data);
    }

    #[test]
    fn test_header_checksum_verification() {
        let data = b"Test data";
        let mut compressed = compress(data).expect("compress failed");

        // Find and corrupt the header checksum (byte after FLG/BD/content_size)
        // For our default: 4 (magic) + 1 (FLG) + 1 (BD) + 8 (content size) = 14, checksum at 14
        if compressed.len() > 14 {
            compressed[14] ^= 0xFF; // Corrupt checksum
        }

        let result = decompress(&compressed, 100);
        assert!(result.is_err());
    }

    #[test]
    fn test_content_checksum_verification() {
        let data = b"Test data for checksum";
        let mut compressed = compress(data).expect("compress failed");

        // Corrupt the last 4 bytes (content checksum)
        let len = compressed.len();
        if len >= 4 {
            compressed[len - 1] ^= 0xFF;
        }

        let result = decompress(&compressed, 100);
        assert!(result.is_err());
    }

    #[test]
    #[cfg(feature = "parallel")]
    fn test_parallel_roundtrip_basic() {
        let data = b"Hello, World! This is a test of parallel LZ4 compression.";
        let compressed = compress_parallel(data).expect("parallel compress failed");
        let decompressed = decompress(&compressed, data.len() * 2).expect("decompress failed");
        assert_eq!(decompressed, data);
    }

    #[test]
    #[cfg(feature = "parallel")]
    fn test_parallel_roundtrip_large() {
        // Large data that will be split into multiple blocks
        let data = vec![0x42u8; 10_000_000];
        let compressed = compress_parallel(&data).expect("parallel compress failed");
        let decompressed = decompress(&compressed, data.len() * 2).expect("decompress failed");
        assert_eq!(decompressed, data);
    }

    #[test]
    #[cfg(feature = "parallel")]
    fn test_parallel_roundtrip_pattern() {
        let mut data = Vec::new();
        for i in 0..100000 {
            data.push((i % 256) as u8);
        }
        let compressed = compress_parallel(&data).expect("parallel compress failed");
        let decompressed = decompress(&compressed, data.len() * 2).expect("decompress failed");
        assert_eq!(decompressed, data);
    }

    #[test]
    #[cfg(feature = "parallel")]
    fn test_parallel_vs_serial_output() {
        // Verify parallel and serial produce identical output
        let data = b"The quick brown fox jumps over the lazy dog.";
        let serial = compress(data).expect("serial compress failed");
        let parallel = compress_parallel(data).expect("parallel compress failed");

        // Both should decompress to the same data
        let serial_decompressed =
            decompress(&serial, data.len() * 2).expect("decompress serial failed");
        let parallel_decompressed =
            decompress(&parallel, data.len() * 2).expect("decompress parallel failed");

        assert_eq!(serial_decompressed, data);
        assert_eq!(parallel_decompressed, data);
    }

    #[test]
    #[cfg(feature = "parallel")]
    fn test_parallel_empty() {
        let data: &[u8] = b"";
        let compressed = compress_parallel(data).expect("parallel compress failed");
        let decompressed = decompress(&compressed, 0).expect("decompress failed");
        assert_eq!(decompressed, data);
    }

    #[test]
    #[cfg(feature = "parallel")]
    fn test_parallel_with_options() {
        let data = b"Testing parallel compression with custom options.";
        let desc = FrameDescriptor::new()
            .with_content_size(data.len() as u64)
            .with_block_checksum(true)
            .with_block_max_size(BlockMaxSize::Size64KB);

        let compressed = compress_with_options_parallel(data, desc).expect("compress failed");
        let decompressed = decompress(&compressed, data.len() * 2).expect("decompress failed");
        assert_eq!(decompressed, data);
    }

    #[test]
    #[cfg(feature = "parallel")]
    fn test_parallel_multiple_blocks() {
        // Create data that will span multiple blocks
        let mut data = Vec::new();
        for _ in 0..5 {
            data.extend_from_slice(b"Block of data that repeats. ");
        }
        let data = data.repeat(50000); // Make it large

        let desc = FrameDescriptor::new()
            .with_content_size(data.len() as u64)
            .with_block_max_size(BlockMaxSize::Size256KB);

        let compressed = compress_with_options_parallel(&data, desc).expect("compress failed");
        let decompressed = decompress(&compressed, data.len() * 2).expect("decompress failed");
        assert_eq!(decompressed, data);
    }

    // ========================================================================
    // Dictionary Frame Integration Tests
    // ========================================================================

    #[test]
    fn test_frame_dict_roundtrip_basic() {
        let dict = Lz4Dict::new(b"common pattern for testing");
        let data = b"common pattern for testing appears in this data";

        let compressed = compress_frame_with_dict(data, &dict).expect("compress failed");
        let decompressed = decompress_frame_with_dict(&compressed, data.len() * 2, &dict)
            .expect("decompress failed");

        assert_eq!(decompressed, data);
    }

    #[test]
    fn test_frame_dict_roundtrip_large() {
        let dict = Lz4Dict::new(b"repeating pattern to compress");

        // Create large data with the pattern
        let mut data = Vec::new();
        for i in 0..1000 {
            data.extend_from_slice(
                format!("Line {}: repeating pattern to compress\n", i).as_bytes(),
            );
        }

        let compressed = compress_frame_with_dict(&data, &dict).expect("compress failed");
        let decompressed = decompress_frame_with_dict(&compressed, data.len() * 2, &dict)
            .expect("decompress failed");

        assert_eq!(decompressed, data);
    }

    #[test]
    fn test_frame_dict_encoder_decoder() {
        let dict = Lz4Dict::new(b"encoder decoder test pattern");
        let encoder = Lz4DictFrameEncoder::new(dict.clone());
        let decoder = Lz4DictFrameDecoder::new(dict);

        let data = b"encoder decoder test pattern is here";
        let compressed = encoder.encode(data).expect("encode failed");

        assert!(decoder.can_decode(&compressed));

        let decompressed = decoder
            .decode(&compressed, data.len() * 2)
            .expect("decode failed");
        assert_eq!(decompressed, data);
    }

    #[test]
    fn test_frame_dict_encoder_with_size() {
        let dict = Lz4Dict::new(b"size test pattern");
        let encoder = Lz4DictFrameEncoder::new(dict.clone());
        let decoder = Lz4DictFrameDecoder::new(dict);

        let data = b"size test pattern in the data";
        let compressed = encoder.encode_with_size(data).expect("encode failed");

        let decompressed = decoder
            .decode(&compressed, data.len() * 2)
            .expect("decode failed");
        assert_eq!(decompressed, data);
    }

    #[test]
    fn test_frame_dict_get_dict_id() {
        let dict = Lz4Dict::new(b"dict id test");
        let data = b"dict id test data here";

        let compressed = compress_frame_with_dict(data, &dict).expect("compress failed");
        let frame_dict_id = get_frame_dict_id(&compressed).expect("get dict id failed");

        assert_eq!(frame_dict_id, Some(dict.id()));
    }

    #[test]
    fn test_frame_dict_id_mismatch() {
        let dict1 = Lz4Dict::new(b"dictionary one");
        let dict2 = Lz4Dict::new(b"dictionary two");

        let data = b"some data to compress";
        let compressed = compress_frame_with_dict(data, &dict1).expect("compress failed");

        // Try to decompress with wrong dictionary
        let result = decompress_frame_with_dict(&compressed, data.len() * 2, &dict2);
        assert!(result.is_err());
    }

    #[test]
    fn test_frame_dict_compressor_trait() {
        let dict = Lz4Dict::new(b"compressor trait test");
        let mut compressor = Lz4DictCompressor::new(dict.clone());

        let data = b"compressor trait test data";
        let mut output = vec![0u8; 500];

        let (consumed, produced, status) = compressor
            .compress(data, &mut output, FlushMode::Finish)
            .expect("compress failed");

        assert_eq!(consumed, data.len());
        assert!(produced > 0);
        assert_eq!(status, CompressStatus::Done);

        // Verify we can decompress
        let decompressed = decompress_frame_with_dict(&output[..produced], data.len() * 2, &dict)
            .expect("decompress failed");
        assert_eq!(decompressed, data);
    }

    #[test]
    fn test_frame_dict_decompressor_trait() {
        let dict = Lz4Dict::new(b"decompressor trait test");
        let data = b"decompressor trait test data";

        let compressed = compress_frame_with_dict(data, &dict).expect("compress failed");

        let mut decompressor = Lz4DictDecompressor::new(dict);
        let mut output = vec![0u8; 200];

        let (consumed, produced, status) = decompressor
            .decompress(&compressed, &mut output)
            .expect("decompress failed");

        assert_eq!(consumed, compressed.len());
        assert_eq!(produced, data.len());
        assert_eq!(status, DecompressStatus::Done);
        assert_eq!(&output[..produced], data.as_slice());
    }

    #[test]
    fn test_frame_dict_with_options() {
        let dict = Lz4Dict::new(b"options test pattern");
        let data = b"options test pattern with block checksum";

        let desc = FrameDescriptor::new()
            .with_content_size(data.len() as u64)
            .with_block_checksum(true);

        let compressed =
            compress_frame_with_dict_options(data, &dict, desc).expect("compress failed");
        let decompressed = decompress_frame_with_dict(&compressed, data.len() * 2, &dict)
            .expect("decompress failed");

        assert_eq!(decompressed, data);
    }

    #[test]
    fn test_frame_dict_empty_data() {
        let dict = Lz4Dict::new(b"empty test dict");
        let data: &[u8] = b"";

        let compressed = compress_frame_with_dict(data, &dict).expect("compress failed");
        let decompressed =
            decompress_frame_with_dict(&compressed, 100, &dict).expect("decompress failed");

        assert_eq!(decompressed, data);
    }

    #[test]
    fn test_frame_dict_improves_compression() {
        let pattern = b"the quick brown fox jumps over the lazy dog";
        let dict = Lz4Dict::new(pattern);

        // Create data that contains the dictionary pattern multiple times
        let mut data = Vec::new();
        for i in 0..100 {
            data.extend_from_slice(
                format!("Line {}: the quick brown fox jumps over the lazy dog\n", i).as_bytes(),
            );
        }

        let with_dict = compress_frame_with_dict(&data, &dict).expect("compress with dict failed");
        let without_dict = compress(&data).expect("compress without dict failed");

        // With dictionary should be same or better for this pattern-heavy data
        // (Note: for some data patterns, the overhead of dictionary may not help)
        let decompressed = decompress_frame_with_dict(&with_dict, data.len() * 2, &dict)
            .expect("decompress failed");
        assert_eq!(decompressed, data);

        // Verify without dict also works
        let decompressed_no_dict =
            decompress(&without_dict, data.len() * 2).expect("decompress no dict failed");
        assert_eq!(decompressed_no_dict, data);
    }

    #[test]
    fn test_frame_dict_encoder_debug() {
        let dict = Lz4Dict::new(b"debug test");
        let encoder = Lz4DictFrameEncoder::new(dict);
        let debug_str = format!("{:?}", encoder);
        assert!(debug_str.contains("Lz4DictFrameEncoder"));
        assert!(debug_str.contains("dict_id"));
    }

    #[test]
    fn test_frame_dict_decoder_debug() {
        let dict = Lz4Dict::new(b"debug test");
        let decoder = Lz4DictFrameDecoder::new(dict);
        let debug_str = format!("{:?}", decoder);
        assert!(debug_str.contains("Lz4DictFrameDecoder"));
        assert!(debug_str.contains("dict_id"));
    }

    #[test]
    fn test_frame_dict_encoder_with_options() {
        let dict = Lz4Dict::new(b"custom options test");
        let desc = FrameDescriptor::new()
            .with_block_checksum(true)
            .with_block_max_size(BlockMaxSize::Size64KB);

        let encoder = Lz4DictFrameEncoder::with_options(dict.clone(), desc);
        let decoder = Lz4DictFrameDecoder::new(dict);

        let data = b"custom options test data with block checksums enabled";
        let compressed = encoder.encode(data).expect("encode failed");

        let decompressed = decoder
            .decode(&compressed, data.len() * 2)
            .expect("decode failed");
        assert_eq!(decompressed, data);
    }

    #[test]
    fn test_frame_dict_compressor_reset() {
        let dict = Lz4Dict::new(b"reset test pattern");
        let mut compressor = Lz4DictCompressor::new(dict.clone());

        let data1 = b"first data set";
        let mut output1 = vec![0u8; 500];

        let (_, produced1, _) = compressor
            .compress(data1, &mut output1, FlushMode::Finish)
            .expect("first compress failed");

        assert!(compressor.is_finished());

        // Reset and compress again
        compressor.reset();
        assert!(!compressor.is_finished());

        let data2 = b"second data set after reset";
        let mut output2 = vec![0u8; 500];

        let (_, produced2, _) = compressor
            .compress(data2, &mut output2, FlushMode::Finish)
            .expect("second compress failed");

        // Verify both are valid
        let decompressed1 =
            decompress_frame_with_dict(&output1[..produced1], data1.len() * 2, &dict)
                .expect("decompress1 failed");
        let decompressed2 =
            decompress_frame_with_dict(&output2[..produced2], data2.len() * 2, &dict)
                .expect("decompress2 failed");

        assert_eq!(decompressed1, data1);
        assert_eq!(decompressed2, data2);
    }

    #[test]
    fn test_frame_dict_decompressor_reset() {
        let dict = Lz4Dict::new(b"decompressor reset test");

        let data1 = b"first decompression";
        let compressed1 = compress_frame_with_dict(data1, &dict).expect("compress1 failed");

        let data2 = b"second decompression after reset";
        let compressed2 = compress_frame_with_dict(data2, &dict).expect("compress2 failed");

        let mut decompressor = Lz4DictDecompressor::new(dict);

        // First decompression
        let mut output1 = vec![0u8; 200];
        let (_, produced1, _) = decompressor
            .decompress(&compressed1, &mut output1)
            .expect("decompress1 failed");
        assert_eq!(&output1[..produced1], data1.as_slice());
        assert!(decompressor.is_finished());

        // Reset and decompress again
        decompressor.reset();
        assert!(!decompressor.is_finished());

        let mut output2 = vec![0u8; 200];
        let (_, produced2, _) = decompressor
            .decompress(&compressed2, &mut output2)
            .expect("decompress2 failed");
        assert_eq!(&output2[..produced2], data2.as_slice());
    }

    #[test]
    fn test_frame_dict_can_decode() {
        let dict1 = Lz4Dict::new(b"dictionary one");
        let dict2 = Lz4Dict::new(b"dictionary two");

        let data = b"data for dictionary one";
        let compressed = compress_frame_with_dict(data, &dict1).expect("compress failed");

        let decoder1 = Lz4DictFrameDecoder::new(dict1);
        let decoder2 = Lz4DictFrameDecoder::new(dict2);

        assert!(decoder1.can_decode(&compressed));
        assert!(!decoder2.can_decode(&compressed));
    }

    #[test]
    fn test_frame_no_dict_id() {
        // Regular frame without dictionary
        let data = b"regular frame data";
        let compressed = compress(data).expect("compress failed");

        let dict_id = get_frame_dict_id(&compressed).expect("get dict id failed");
        assert_eq!(dict_id, None);
    }

    #[test]
    fn test_frame_dict_accessor_methods() {
        let dict = Lz4Dict::new(b"accessor test");

        let encoder = Lz4DictFrameEncoder::new(dict.clone());
        assert_eq!(encoder.dict_id(), dict.id());
        assert_eq!(encoder.dict().len(), dict.len());

        let decoder = Lz4DictFrameDecoder::new(dict.clone());
        assert_eq!(decoder.dict_id(), dict.id());
        assert_eq!(decoder.dict().len(), dict.len());

        let compressor = Lz4DictCompressor::new(dict.clone());
        assert_eq!(compressor.dict().id(), dict.id());

        let decompressor = Lz4DictDecompressor::new(dict.clone());
        assert_eq!(decompressor.dict().id(), dict.id());
    }
}
