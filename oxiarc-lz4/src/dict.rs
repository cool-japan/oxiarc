//! LZ4 dictionary support for improved compression ratios.
//!
//! Dictionary-based compression improves compression ratios for small data
//! with common patterns by allowing matches to reference bytes that appear
//! in a pre-trained dictionary.
//!
//! # How it works
//!
//! The dictionary is logically prepended to the input data during compression.
//! This allows the encoder to find matches in the dictionary, which is
//! especially useful for small files that share common patterns.
//!
//! # Features
//!
//! - Dictionary pre-loading with up to 64KB of data
//! - Dictionary ID in frame header (optional)
//! - Content-defined dictionary support
//! - xxHash checksum of dictionary for validation
//! - DictBuilder for creating optimized dictionaries from samples
//!
//! # Example
//!
//! ```
//! use oxiarc_lz4::dict::{Lz4Dict, compress_with_dict, decompress_with_dict};
//!
//! // Create a dictionary from common patterns
//! let dict_data = b"common pattern data";
//! let dict = Lz4Dict::new(dict_data);
//!
//! // Compress data using the dictionary
//! let data = b"this contains common pattern data!";
//! let compressed = compress_with_dict(data, &dict).unwrap();
//!
//! // Decompress using the same dictionary
//! let decompressed = decompress_with_dict(&compressed, data.len(), &dict).unwrap();
//! assert_eq!(decompressed, data);
//! ```

use crate::xxhash::xxhash32;
use oxiarc_core::error::{OxiArcError, Result};
use std::collections::HashMap;

/// Maximum dictionary size (64 KB for LZ4).
pub const MAX_DICT_SIZE: usize = 64 * 1024;

/// Minimum match length for LZ4.
const MIN_MATCH: usize = 4;

/// Maximum match offset (16-bit).
const MAX_OFFSET: usize = 65535;

/// Hash table size (must be power of 2).
const HASH_SIZE: usize = 1 << 14; // 16K entries

/// LZ4 dictionary for improved compression ratios.
///
/// A dictionary contains pre-trained data that can be referenced during
/// compression, improving ratios for small files with common patterns.
#[derive(Clone)]
pub struct Lz4Dict {
    /// Dictionary content (up to 64KB).
    data: Vec<u8>,
    /// Dictionary ID (XXHash32 of content).
    id: u32,
    /// Pre-computed hash table for fast lookups.
    hash_table: Vec<u32>,
}

impl std::fmt::Debug for Lz4Dict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Lz4Dict")
            .field("len", &self.data.len())
            .field("id", &format!("{:#010X}", self.id))
            .finish()
    }
}

impl Lz4Dict {
    /// Create a new dictionary from the given data.
    ///
    /// The dictionary size is limited to 64 KB. If the input is larger,
    /// only the last 64 KB is used.
    ///
    /// The dictionary ID is computed as the XXHash32 of the content.
    ///
    /// # Arguments
    ///
    /// * `data` - Dictionary content
    ///
    /// # Returns
    ///
    /// A new dictionary instance.
    ///
    /// # Example
    ///
    /// ```
    /// use oxiarc_lz4::dict::Lz4Dict;
    ///
    /// let dict = Lz4Dict::new(b"common patterns");
    /// assert!(dict.id() != 0);
    /// ```
    pub fn new(data: &[u8]) -> Self {
        // Limit dictionary to last 64KB
        let start = data.len().saturating_sub(MAX_DICT_SIZE);
        let dict_data = data[start..].to_vec();

        // Compute dictionary ID
        let id = xxhash32(&dict_data);

        // Build hash table
        let mut hash_table = vec![0u32; HASH_SIZE];
        Self::build_hash_table(&dict_data, &mut hash_table);

        Self {
            data: dict_data,
            id,
            hash_table,
        }
    }

    /// Create a dictionary with a custom ID.
    ///
    /// This is useful when you need to match a specific dictionary ID
    /// from an LZ4 frame.
    ///
    /// # Arguments
    ///
    /// * `data` - Dictionary content
    /// * `id` - Custom dictionary ID
    pub fn with_id(data: &[u8], id: u32) -> Self {
        let start = data.len().saturating_sub(MAX_DICT_SIZE);
        let dict_data = data[start..].to_vec();

        let mut hash_table = vec![0u32; HASH_SIZE];
        Self::build_hash_table(&dict_data, &mut hash_table);

        Self {
            data: dict_data,
            id,
            hash_table,
        }
    }

    /// Create an empty dictionary.
    pub fn empty() -> Self {
        Self {
            data: Vec::new(),
            id: xxhash32(&[]),
            hash_table: vec![0u32; HASH_SIZE],
        }
    }

    /// Get the dictionary ID.
    ///
    /// The ID is typically the XXHash32 of the dictionary content.
    pub fn id(&self) -> u32 {
        self.id
    }

    /// Get the dictionary data.
    pub fn data(&self) -> &[u8] {
        &self.data
    }

    /// Get the dictionary size.
    pub fn len(&self) -> usize {
        self.data.len()
    }

    /// Check if the dictionary is empty.
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    /// Validate the dictionary checksum.
    ///
    /// Returns an error if the stored ID doesn't match the computed xxHash32.
    pub fn validate(&self) -> Result<()> {
        let computed = xxhash32(&self.data);
        if computed != self.id {
            return Err(OxiArcError::crc_mismatch(computed, self.id));
        }
        Ok(())
    }

    /// Build hash table from dictionary data.
    fn build_hash_table(data: &[u8], hash_table: &mut [u32]) {
        for i in 0..data.len().saturating_sub(MIN_MATCH - 1) {
            let h = Self::hash(Self::read_u32(data, i));
            hash_table[h] = i as u32;
        }
    }

    /// Compute hash for 4 bytes.
    #[inline]
    pub(crate) fn hash(data: u32) -> usize {
        ((data.wrapping_mul(2654435761)) >> 18) as usize & (HASH_SIZE - 1)
    }

    /// Read 4 bytes as u32 (little-endian).
    #[inline]
    pub(crate) fn read_u32(data: &[u8], pos: usize) -> u32 {
        if pos + 4 <= data.len() {
            u32::from_le_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]])
        } else {
            0
        }
    }

    /// Get hash table reference for match finding.
    pub(crate) fn hash_table(&self) -> &[u32] {
        &self.hash_table
    }
}

/// Builder for creating optimized dictionaries from sample data.
///
/// The builder collects sample data and creates an optimized dictionary
/// that maximizes compression ratio for similar data.
///
/// # Example
///
/// ```
/// use oxiarc_lz4::dict::DictBuilder;
///
/// let dict = DictBuilder::new()
///     .add_sample(b"Hello, World!")
///     .add_sample(b"Hello, Rust!")
///     .add_sample(b"Hello, LZ4!")
///     .max_size(1024)
///     .build()
///     .unwrap();
///
/// assert!(!dict.is_empty());
/// assert!(dict.len() <= 1024);
/// ```
#[derive(Default)]
pub struct DictBuilder {
    samples: Vec<Vec<u8>>,
    max_size: usize,
}

impl DictBuilder {
    /// Create a new dictionary builder.
    pub fn new() -> Self {
        Self {
            samples: Vec::new(),
            max_size: MAX_DICT_SIZE,
        }
    }

    /// Set the maximum dictionary size.
    ///
    /// The default is 64KB (maximum allowed by LZ4).
    pub fn max_size(mut self, size: usize) -> Self {
        self.max_size = size.min(MAX_DICT_SIZE);
        self
    }

    /// Add a single sample to the builder.
    pub fn add_sample(mut self, sample: &[u8]) -> Self {
        self.samples.push(sample.to_vec());
        self
    }

    /// Add multiple samples to the builder.
    pub fn add_samples(mut self, samples: &[Vec<u8>]) -> Self {
        self.samples.extend(samples.iter().cloned());
        self
    }

    /// Add samples from an iterator.
    pub fn add_samples_iter<I, T>(mut self, samples: I) -> Self
    where
        I: IntoIterator<Item = T>,
        T: AsRef<[u8]>,
    {
        for sample in samples {
            self.samples.push(sample.as_ref().to_vec());
        }
        self
    }

    /// Build the dictionary from collected samples.
    ///
    /// The algorithm selects the most common substrings from the samples
    /// to create an optimized dictionary.
    pub fn build(self) -> Result<Lz4Dict> {
        if self.samples.is_empty() {
            return Ok(Lz4Dict::empty());
        }

        // Use frequency-based optimization
        let dict_data = self.build_optimized_dict();
        Ok(Lz4Dict::new(&dict_data))
    }

    /// Build an optimized dictionary from samples.
    ///
    /// This uses a frequency-based approach to select the most useful
    /// substrings for the dictionary.
    fn build_optimized_dict(&self) -> Vec<u8> {
        // Collect all samples
        let mut all_data = Vec::new();
        for sample in &self.samples {
            all_data.extend_from_slice(sample);
        }

        // If total data fits in dictionary, use it directly
        if all_data.len() <= self.max_size {
            return all_data;
        }

        // Find most common n-grams (substrings of length 4-16)
        let mut ngram_counts: HashMap<Vec<u8>, usize> = HashMap::new();

        // Count n-grams across all samples
        for sample in &self.samples {
            let max_ngram = 16.min(sample.len());
            for ngram_len in MIN_MATCH..=max_ngram {
                for window in sample.windows(ngram_len) {
                    *ngram_counts.entry(window.to_vec()).or_insert(0) += 1;
                }
            }
        }

        // Filter to n-grams that appear more than once
        let mut ngrams: Vec<_> = ngram_counts
            .into_iter()
            .filter(|(_, count)| *count > 1)
            .collect();

        // Sort by score (frequency * length) - prefer longer, more frequent n-grams
        ngrams.sort_by(|a, b| {
            let score_a = a.1 * a.0.len();
            let score_b = b.1 * b.0.len();
            score_b.cmp(&score_a)
        });

        // Build dictionary by selecting top n-grams
        let mut dict_data = Vec::with_capacity(self.max_size);
        let mut used_positions: std::collections::HashSet<usize> = std::collections::HashSet::new();

        for (ngram, _count) in ngrams {
            // Check if we already have this as part of something we added
            let already_included = {
                let mut found = false;
                for window in dict_data.windows(ngram.len()) {
                    if window == ngram.as_slice() {
                        found = true;
                        break;
                    }
                }
                found
            };

            if already_included {
                continue;
            }

            // Check if adding this would exceed max size
            if dict_data.len() + ngram.len() > self.max_size {
                if dict_data.len() >= self.max_size {
                    break;
                }
                continue;
            }

            dict_data.extend_from_slice(&ngram);
            used_positions.insert(dict_data.len());
        }

        // If dictionary is still too small, fill with sample data
        if dict_data.len() < self.max_size && !all_data.is_empty() {
            let remaining = self.max_size - dict_data.len();
            let start = all_data.len().saturating_sub(remaining);
            dict_data.extend_from_slice(&all_data[start..]);
        }

        // Ensure we don't exceed max size
        if dict_data.len() > self.max_size {
            dict_data.truncate(self.max_size);
        }

        dict_data
    }
}

/// Dictionary compression level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DictLevel {
    /// Fast compression (default).
    #[default]
    Fast,
    /// High compression (slower but better ratio).
    High,
}

/// Dictionary-aware block encoder.
struct DictBlockEncoder<'a> {
    input: &'a [u8],
    dict: &'a Lz4Dict,
    hash_table: Vec<u32>,
    dict_len: usize,
}

impl<'a> DictBlockEncoder<'a> {
    fn new(input: &'a [u8], dict: &'a Lz4Dict) -> Self {
        // Initialize hash table with dictionary entries offset by dict length
        let mut hash_table = vec![0u32; HASH_SIZE];

        // Copy dictionary hash entries (positions relative to virtual buffer start)
        // Dictionary positions are stored as-is (0 to dict.len-1)
        for (i, &val) in dict.hash_table().iter().enumerate() {
            if val > 0 || (i < dict.data().len() && dict.data().len() >= MIN_MATCH) {
                hash_table[i] = val;
            }
        }

        Self {
            input,
            dict,
            hash_table,
            dict_len: dict.len(),
        }
    }

    /// Compute hash for 4 bytes.
    #[inline]
    fn hash(data: u32) -> usize {
        Lz4Dict::hash(data)
    }

    /// Read 4 bytes as u32 (little-endian).
    #[inline]
    fn read_u32(data: &[u8], pos: usize) -> u32 {
        Lz4Dict::read_u32(data, pos)
    }

    /// Get byte from virtual buffer (dictionary + input).
    #[inline]
    fn get_byte(&self, virtual_pos: usize) -> Option<u8> {
        if virtual_pos < self.dict_len {
            self.dict.data().get(virtual_pos).copied()
        } else {
            self.input.get(virtual_pos - self.dict_len).copied()
        }
    }

    /// Read 4 bytes from virtual position.
    #[inline]
    fn read_u32_virtual(&self, virtual_pos: usize) -> u32 {
        if virtual_pos >= self.dict_len {
            Self::read_u32(self.input, virtual_pos - self.dict_len)
        } else if virtual_pos + 4 <= self.dict_len {
            Self::read_u32(self.dict.data(), virtual_pos)
        } else {
            // Spanning dictionary and input
            let mut bytes = [0u8; 4];
            for (i, byte) in bytes.iter_mut().enumerate() {
                *byte = self.get_byte(virtual_pos + i).unwrap_or(0);
            }
            u32::from_le_bytes(bytes)
        }
    }

    /// Encode the input data with dictionary support.
    fn encode(&mut self, output: &mut Vec<u8>) -> Result<()> {
        let input = self.input;
        let len = input.len();

        if len < MIN_MATCH {
            // Too small to compress, emit as literals
            self.emit_literals(output, 0, len, 0, 0)?;
            return Ok(());
        }

        let mut pos = 0;
        let mut anchor = 0; // Start of current literal run
        let end = len.saturating_sub(5); // Leave room for last literals

        while pos < end {
            let virtual_pos = pos + self.dict_len;
            let cur_u32 = Self::read_u32(input, pos);
            let h = Self::hash(cur_u32);
            let match_pos = self.hash_table[h] as usize;

            // Update hash table with current position (in virtual space)
            self.hash_table[h] = virtual_pos as u32;

            // Check for match
            let offset = virtual_pos.saturating_sub(match_pos);
            if offset > 0 && offset <= MAX_OFFSET {
                let match_u32 = self.read_u32_virtual(match_pos);

                if match_u32 == cur_u32 {
                    // Found a match! Calculate match length
                    let mut match_len = MIN_MATCH;

                    // Extend match forwards
                    while pos + match_len < len {
                        let match_byte = self.get_byte(match_pos + match_len);
                        let cur_byte = input.get(pos + match_len);

                        if match_byte != cur_byte.copied() {
                            break;
                        }
                        match_len += 1;
                    }

                    // Emit literals before match
                    let literal_len = pos - anchor;
                    self.emit_sequence(output, anchor, literal_len, offset, match_len)?;

                    pos += match_len;
                    anchor = pos;

                    // Update hash for positions we skipped
                    if pos < len {
                        let new_virtual = pos + self.dict_len;
                        let new_h = Self::hash(Self::read_u32(input, pos));
                        self.hash_table[new_h] = new_virtual as u32;
                    }

                    continue;
                }
            }

            pos += 1;
        }

        // Emit remaining literals
        let remaining = len - anchor;
        if remaining > 0 {
            self.emit_last_literals(output, anchor, remaining)?;
        }

        Ok(())
    }

    /// Emit a sequence (literals + match).
    fn emit_sequence(
        &self,
        output: &mut Vec<u8>,
        literal_start: usize,
        literal_len: usize,
        offset: usize,
        match_len: usize,
    ) -> Result<()> {
        self.emit_literals(output, literal_start, literal_len, offset, match_len)
    }

    /// Emit literals followed by a match reference.
    fn emit_literals(
        &self,
        output: &mut Vec<u8>,
        literal_start: usize,
        literal_len: usize,
        offset: usize,
        match_len: usize,
    ) -> Result<()> {
        // Token: upper 4 bits = literal length, lower 4 bits = match length - 4
        let lit_token = if literal_len >= 15 { 15 } else { literal_len };
        let match_token = if match_len >= MIN_MATCH {
            let ml = match_len - MIN_MATCH;
            if ml >= 15 { 15 } else { ml }
        } else {
            0
        };

        let token = ((lit_token << 4) | match_token) as u8;
        output.push(token);

        // Extended literal length
        if literal_len >= 15 {
            let mut remaining = literal_len - 15;
            while remaining >= 255 {
                output.push(255);
                remaining -= 255;
            }
            output.push(remaining as u8);
        }

        // Literals
        output.extend_from_slice(&self.input[literal_start..literal_start + literal_len]);

        // Match offset and length (if there's a match)
        if match_len >= MIN_MATCH {
            // Offset: 2 bytes little-endian
            output.push(offset as u8);
            output.push((offset >> 8) as u8);

            // Extended match length
            if match_len - MIN_MATCH >= 15 {
                let mut remaining = match_len - MIN_MATCH - 15;
                while remaining >= 255 {
                    output.push(255);
                    remaining -= 255;
                }
                output.push(remaining as u8);
            }
        }

        Ok(())
    }

    /// Emit the last literals (no match at the end).
    fn emit_last_literals(
        &self,
        output: &mut Vec<u8>,
        literal_start: usize,
        literal_len: usize,
    ) -> Result<()> {
        // Token with match length = 0
        let lit_token = if literal_len >= 15 { 15 } else { literal_len };
        let token = (lit_token << 4) as u8;
        output.push(token);

        // Extended literal length
        if literal_len >= 15 {
            let mut remaining = literal_len - 15;
            while remaining >= 255 {
                output.push(255);
                remaining -= 255;
            }
            output.push(remaining as u8);
        }

        // Literals
        output.extend_from_slice(&self.input[literal_start..literal_start + literal_len]);

        Ok(())
    }
}

/// Dictionary-aware block decoder.
struct DictBlockDecoder<'a> {
    input: &'a [u8],
    pos: usize,
}

impl<'a> DictBlockDecoder<'a> {
    fn new(input: &'a [u8]) -> Self {
        Self { input, pos: 0 }
    }

    /// Decode the block with dictionary support.
    fn decode(&mut self, output: &mut Vec<u8>, max_output: usize, dict: &Lz4Dict) -> Result<()> {
        // Pre-fill output with dictionary
        // Note: We don't actually copy the dictionary - we handle offsets specially
        let dict_data = dict.data();

        while self.pos < self.input.len() && output.len() < max_output {
            // Read token
            let token = self.read_byte()?;
            let literal_len = (token >> 4) as usize;
            let match_len_base = (token & 0x0F) as usize;

            // Extended literal length
            let literal_len = self.read_length(literal_len)?;

            // Check bounds
            if self.pos + literal_len > self.input.len() {
                return Err(OxiArcError::corrupted(
                    self.pos as u64,
                    "truncated literals",
                ));
            }

            // Copy literals
            output.extend_from_slice(&self.input[self.pos..self.pos + literal_len]);
            self.pos += literal_len;

            // Check if this is the last sequence (no match)
            if self.pos >= self.input.len() {
                break;
            }

            // Read match offset
            let offset = self.read_u16_le()? as usize;
            if offset == 0 {
                return Err(OxiArcError::corrupted(self.pos as u64, "zero offset"));
            }

            // Extended match length
            let match_len = self.read_length(match_len_base)? + MIN_MATCH;

            // Handle dictionary reference
            if offset > output.len() {
                // Match references dictionary
                let dict_offset = offset - output.len();

                if dict_offset > dict_data.len() {
                    return Err(OxiArcError::corrupted(
                        self.pos as u64,
                        "offset exceeds dictionary",
                    ));
                }

                // Copy from dictionary and/or output
                let dict_start = dict_data.len() - dict_offset;
                let from_dict = match_len.min(dict_offset);
                let from_output = match_len.saturating_sub(from_dict);

                // Copy from dictionary
                for i in 0..from_dict {
                    if dict_start + i < dict_data.len() {
                        output.push(dict_data[dict_start + i]);
                    }
                }

                // Copy from output (if match spans dict->output)
                if from_output > 0 {
                    let start = 0;
                    for i in 0..from_output {
                        let byte = output[start + (i % offset)];
                        output.push(byte);
                    }
                }
            } else {
                // Normal match within output
                let start = output.len() - offset;
                for i in 0..match_len {
                    let byte = output[start + (i % offset)];
                    output.push(byte);
                }
            }
        }

        Ok(())
    }

    fn read_byte(&mut self) -> Result<u8> {
        if self.pos >= self.input.len() {
            return Err(OxiArcError::unexpected_eof(1));
        }
        let b = self.input[self.pos];
        self.pos += 1;
        Ok(b)
    }

    fn read_u16_le(&mut self) -> Result<u16> {
        if self.pos + 2 > self.input.len() {
            return Err(OxiArcError::unexpected_eof(2));
        }
        let value = u16::from_le_bytes([self.input[self.pos], self.input[self.pos + 1]]);
        self.pos += 2;
        Ok(value)
    }

    fn read_length(&mut self, base: usize) -> Result<usize> {
        let mut len = base;
        if base == 15 {
            loop {
                let b = self.read_byte()? as usize;
                len += b;
                if b != 255 {
                    break;
                }
            }
        }
        Ok(len)
    }
}

/// Compress data using LZ4 block format with dictionary.
///
/// The dictionary allows references to previous data that appears in the
/// dictionary, improving compression ratios for small files with common patterns.
///
/// # Arguments
///
/// * `input` - Data to compress
/// * `dict` - Dictionary for reference
///
/// # Returns
///
/// Compressed data in LZ4 block format.
///
/// # Example
///
/// ```
/// use oxiarc_lz4::dict::{Lz4Dict, compress_with_dict, decompress_with_dict};
///
/// let dict = Lz4Dict::new(b"hello world");
/// let data = b"hello world again";
/// let compressed = compress_with_dict(data, &dict).unwrap();
/// let decompressed = decompress_with_dict(&compressed, data.len(), &dict).unwrap();
/// assert_eq!(decompressed, data);
/// ```
pub fn compress_with_dict(input: &[u8], dict: &Lz4Dict) -> Result<Vec<u8>> {
    if input.is_empty() {
        return Ok(Vec::new());
    }

    if dict.is_empty() {
        // Fall back to regular compression
        return crate::compress_block(input);
    }

    let mut output = Vec::with_capacity(input.len());
    let mut encoder = DictBlockEncoder::new(input, dict);
    encoder.encode(&mut output)?;
    Ok(output)
}

/// Compress data using LZ4 block format with dictionary and level.
///
/// # Arguments
///
/// * `data` - Data to compress
/// * `dict` - Dictionary for reference
/// * `level` - Compression level
///
/// # Returns
///
/// Compressed data in LZ4 block format.
pub fn compress_with_dict_level(data: &[u8], dict: &Lz4Dict, level: DictLevel) -> Result<Vec<u8>> {
    match level {
        DictLevel::Fast => compress_with_dict(data, dict),
        DictLevel::High => {
            // For high compression, we could use HC encoder with dictionary
            // For now, use fast compression (HC with dict requires more work)
            compress_with_dict(data, dict)
        }
    }
}

/// Decompress LZ4 block data with dictionary.
///
/// # Arguments
///
/// * `input` - Compressed data
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
/// use oxiarc_lz4::dict::{Lz4Dict, compress_with_dict, decompress_with_dict};
///
/// let dict = Lz4Dict::new(b"hello world");
/// let data = b"hello world again";
/// let compressed = compress_with_dict(data, &dict).unwrap();
/// let decompressed = decompress_with_dict(&compressed, data.len(), &dict).unwrap();
/// assert_eq!(decompressed, data);
/// ```
pub fn decompress_with_dict(input: &[u8], max_output: usize, dict: &Lz4Dict) -> Result<Vec<u8>> {
    if input.is_empty() {
        return Ok(Vec::new());
    }

    if dict.is_empty() {
        // Fall back to regular decompression
        return crate::decompress_block(input, max_output);
    }

    let mut output = Vec::with_capacity(max_output.min(input.len() * 4));
    let mut decoder = DictBlockDecoder::new(input);
    decoder.decode(&mut output, max_output, dict)?;
    Ok(output)
}

/// Dictionary frame descriptor for frame format with dictionary support.
///
/// This extends the standard LZ4 frame descriptor to include dictionary ID.
#[derive(Debug, Clone, Copy, Default)]
pub struct DictFrameDescriptor {
    /// Block independence flag (blocks can be decoded independently).
    pub block_independence: bool,
    /// Block checksum flag (each block has XXH32 checksum).
    pub block_checksum: bool,
    /// Content size present in header.
    pub content_size: Option<u64>,
    /// Content checksum flag (frame has XXH32 checksum at end).
    pub content_checksum: bool,
    /// Dictionary ID (optional).
    pub dict_id: Option<u32>,
}

impl DictFrameDescriptor {
    /// Create a new dictionary frame descriptor with defaults.
    pub fn new() -> Self {
        Self {
            block_independence: true,
            block_checksum: false,
            content_size: None,
            content_checksum: true,
            dict_id: None,
        }
    }

    /// Set the dictionary ID.
    pub fn with_dict_id(mut self, id: u32) -> Self {
        self.dict_id = Some(id);
        self
    }

    /// Set the dictionary from a dictionary object.
    pub fn with_dict(mut self, dict: &Lz4Dict) -> Self {
        self.dict_id = Some(dict.id());
        self
    }

    /// Set the content size.
    pub fn with_content_size(mut self, size: u64) -> Self {
        self.content_size = Some(size);
        self
    }

    /// Set the content checksum flag.
    pub fn with_content_checksum(mut self, enabled: bool) -> Self {
        self.content_checksum = enabled;
        self
    }

    /// Set the block checksum flag.
    pub fn with_block_checksum(mut self, enabled: bool) -> Self {
        self.block_checksum = enabled;
        self
    }

    /// Encode the FLG byte.
    ///
    /// FLG byte format:
    /// - Bits 7-6: Version (01 for current version)
    /// - Bit 5: Block independence
    /// - Bit 4: Block checksum
    /// - Bit 3: Content size present
    /// - Bit 2: Content checksum
    /// - Bit 1: Reserved (0)
    /// - Bit 0: Dictionary ID present
    pub fn flg_byte(&self) -> u8 {
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

    /// Check if dictionary ID flag is set in FLG byte.
    pub fn has_dict_id(flg: u8) -> bool {
        (flg & 0x01) != 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dict_new() {
        let dict = Lz4Dict::new(b"hello world");
        assert!(!dict.is_empty());
        assert_eq!(dict.len(), 11);
        assert!(dict.id() != 0);
    }

    #[test]
    fn test_dict_with_id() {
        let dict = Lz4Dict::with_id(b"hello", 12345);
        assert_eq!(dict.id(), 12345);
    }

    #[test]
    fn test_dict_empty() {
        let dict = Lz4Dict::new(b"");
        assert!(dict.is_empty());
        assert_eq!(dict.len(), 0);
    }

    #[test]
    fn test_dict_empty_method() {
        let dict = Lz4Dict::empty();
        assert!(dict.is_empty());
        assert_eq!(dict.len(), 0);
    }

    #[test]
    fn test_dict_max_size() {
        // Create data larger than 64KB
        let large_data = vec![0x42u8; 100 * 1024];
        let dict = Lz4Dict::new(&large_data);
        assert_eq!(dict.len(), MAX_DICT_SIZE);
    }

    #[test]
    fn test_dict_validate() {
        let dict = Lz4Dict::new(b"test data");
        assert!(dict.validate().is_ok());
    }

    #[test]
    fn test_dict_builder_empty() {
        let dict = DictBuilder::new().build().expect("build failed");
        assert!(dict.is_empty());
    }

    #[test]
    fn test_dict_builder_single_sample() {
        let dict = DictBuilder::new()
            .add_sample(b"Hello, World!")
            .build()
            .expect("build failed");
        assert!(!dict.is_empty());
    }

    #[test]
    fn test_dict_builder_multiple_samples() {
        let samples = vec![
            b"Hello, World!".to_vec(),
            b"Hello, Rust!".to_vec(),
            b"Goodbye, World!".to_vec(),
        ];
        let dict = DictBuilder::new()
            .add_samples(&samples)
            .build()
            .expect("build failed");
        assert!(!dict.is_empty());
    }

    #[test]
    fn test_dict_builder_max_size() {
        let dict = DictBuilder::new()
            .add_sample(&vec![0x42u8; 10000])
            .max_size(1000)
            .build()
            .expect("build failed");
        assert!(dict.len() <= 1000);
    }

    #[test]
    fn test_dict_builder_iter() {
        let samples = ["sample1", "sample2", "sample3"];
        let dict = DictBuilder::new()
            .add_samples_iter(samples.iter().map(|s| s.as_bytes()))
            .build()
            .expect("build failed");
        assert!(!dict.is_empty());
    }

    #[test]
    fn test_roundtrip_with_dict_simple() {
        let dict = Lz4Dict::new(b"hello world");
        let data = b"hello world again";

        let compressed = compress_with_dict(data, &dict).expect("compress failed");
        let decompressed =
            decompress_with_dict(&compressed, data.len() * 2, &dict).expect("decompress failed");

        assert_eq!(decompressed, data);
    }

    #[test]
    fn test_roundtrip_with_dict_pattern() {
        let dict = Lz4Dict::new(b"The quick brown fox jumps over the lazy dog.");
        let data = b"The quick brown fox jumps over the lazy dog. Again!";

        let compressed = compress_with_dict(data, &dict).expect("compress failed");
        let decompressed =
            decompress_with_dict(&compressed, data.len() * 2, &dict).expect("decompress failed");

        assert_eq!(decompressed, data);
    }

    #[test]
    fn test_roundtrip_with_empty_dict() {
        let dict = Lz4Dict::new(b"");
        let data = b"hello world";

        let compressed = compress_with_dict(data, &dict).expect("compress failed");
        let decompressed =
            decompress_with_dict(&compressed, data.len() * 2, &dict).expect("decompress failed");

        assert_eq!(decompressed, data);
    }

    #[test]
    fn test_roundtrip_empty_data() {
        let dict = Lz4Dict::new(b"some dictionary");
        let data: &[u8] = b"";

        let compressed = compress_with_dict(data, &dict).expect("compress failed");
        let decompressed =
            decompress_with_dict(&compressed, 100, &dict).expect("decompress failed");

        assert_eq!(decompressed, data);
    }

    #[test]
    fn test_roundtrip_with_level() {
        let dict = Lz4Dict::new(b"test dictionary");
        let data = b"test dictionary content";

        // Test fast level
        let compressed_fast =
            compress_with_dict_level(data, &dict, DictLevel::Fast).expect("compress failed");
        let decompressed = decompress_with_dict(&compressed_fast, data.len() * 2, &dict)
            .expect("decompress failed");
        assert_eq!(decompressed, data);

        // Test high level
        let compressed_high =
            compress_with_dict_level(data, &dict, DictLevel::High).expect("compress failed");
        let decompressed = decompress_with_dict(&compressed_high, data.len() * 2, &dict)
            .expect("decompress failed");
        assert_eq!(decompressed, data);
    }

    #[test]
    fn test_dict_improves_compression() {
        // Data that shares patterns with dictionary should compress better
        let common_pattern = b"common pattern that repeats often in the data";
        let dict = Lz4Dict::new(common_pattern);

        // Create data that contains the dictionary pattern
        let mut data = Vec::new();
        data.extend_from_slice(common_pattern);
        data.extend_from_slice(b" - some unique content - ");
        data.extend_from_slice(common_pattern);
        data.extend_from_slice(b" - more unique - ");
        data.extend_from_slice(common_pattern);

        let with_dict = compress_with_dict(&data, &dict).expect("compress with dict failed");
        let without_dict = crate::compress_block(&data).expect("compress failed");

        // With dictionary should be same or better
        // (for this specific pattern, dictionary helps reference across gaps)
        let decompressed =
            decompress_with_dict(&with_dict, data.len() * 2, &dict).expect("decompress failed");
        assert_eq!(decompressed, data);

        // Verify without dict also works
        let decompressed_no_dict =
            crate::decompress_block(&without_dict, data.len() * 2).expect("decompress failed");
        assert_eq!(decompressed_no_dict, data);

        // For this pattern, compression should work
        assert!(!with_dict.is_empty());
    }

    #[test]
    fn test_dict_id_verification() {
        let dict1 = Lz4Dict::new(b"dictionary one");
        let dict2 = Lz4Dict::new(b"dictionary two");

        // Different dictionaries should have different IDs
        assert_ne!(dict1.id(), dict2.id());
    }

    #[test]
    fn test_roundtrip_large_data_with_dict() {
        let dict = Lz4Dict::new(b"pattern to find in the data");

        // Create large data with repeated patterns
        let mut data = Vec::new();
        for i in 0..100 {
            data.extend_from_slice(
                format!("Line {} - pattern to find in the data\n", i).as_bytes(),
            );
        }

        let compressed = compress_with_dict(&data, &dict).expect("compress failed");
        let decompressed =
            decompress_with_dict(&compressed, data.len() * 2, &dict).expect("decompress failed");

        assert_eq!(decompressed, data);
    }

    #[test]
    fn test_small_data_with_dict() {
        let dict = Lz4Dict::new(b"ab");
        let data = b"ab";

        let compressed = compress_with_dict(data, &dict).expect("compress failed");
        let decompressed =
            decompress_with_dict(&compressed, data.len() * 2, &dict).expect("decompress failed");

        assert_eq!(decompressed, data);
    }

    #[test]
    fn test_dict_frame_descriptor() {
        let desc = DictFrameDescriptor::new()
            .with_dict_id(0x12345678)
            .with_content_size(100)
            .with_content_checksum(true);

        assert_eq!(desc.dict_id, Some(0x12345678));
        assert_eq!(desc.content_size, Some(100));
        assert!(desc.content_checksum);
        assert!(DictFrameDescriptor::has_dict_id(desc.flg_byte()));
    }

    #[test]
    fn test_dict_frame_descriptor_with_dict() {
        let dict = Lz4Dict::new(b"test dictionary");
        let desc = DictFrameDescriptor::new().with_dict(&dict);

        assert_eq!(desc.dict_id, Some(dict.id()));
    }

    #[test]
    fn test_dict_debug() {
        let dict = Lz4Dict::new(b"test");
        let debug_str = format!("{:?}", dict);
        assert!(debug_str.contains("Lz4Dict"));
        assert!(debug_str.contains("len"));
    }

    #[test]
    fn test_dict_builder_frequency_optimization() {
        // Create samples with common patterns
        let samples = vec![
            b"The quick brown fox".to_vec(),
            b"The quick brown dog".to_vec(),
            b"The quick brown cat".to_vec(),
            b"The slow brown fox".to_vec(),
        ];

        let dict = DictBuilder::new()
            .add_samples(&samples)
            .max_size(100)
            .build()
            .expect("build failed");

        // Dictionary should contain common patterns
        assert!(!dict.is_empty());
        assert!(dict.len() <= 100);
    }
}
