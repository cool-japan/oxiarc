//! LZ4 dictionary-based frame compression/decompression.

use super::types::{FrameDescriptor, LZ4_FRAME_MAGIC};
use crate::dict::{
    Lz4Dict, compress_with_dict as block_compress_with_dict,
    decompress_with_dict as block_decompress_with_dict,
};
use crate::xxhash::{XxHash32, xxhash32};
use oxiarc_core::error::{OxiArcError, Result};
use oxiarc_core::traits::{CompressStatus, Compressor, DecompressStatus, Decompressor, FlushMode};

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
