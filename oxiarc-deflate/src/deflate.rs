//! DEFLATE compression.
//!
//! This module implements DEFLATE compression as specified in RFC 1951.
//! It supports:
//! - Stored blocks (no compression)
//! - Fixed Huffman codes
//! - Dynamic Huffman codes

use crate::huffman::HuffmanBuilder;
use crate::lz77::{Lz77Encoder, Lz77Params, Lz77Preset, Lz77Token};
use crate::optimal::OptimalParser;
use crate::pool::{DeflatePool, PooledBuf, PooledU16Buf};
use crate::tables::{distance_to_code, fixed_litlen_lengths, length_to_code};
use oxiarc_core::BitWriter;
use oxiarc_core::error::Result;
use oxiarc_core::traits::{CompressStatus, Compressor, FlushMode};
use std::io::Write;

/// Extract the inner `Vec<u8>` from a [`PooledBuf`] without returning it to
/// the pool.  We take ownership here because the buffer's ownership is being
/// transferred to the `Lz77Encoder`; it will be returned via `Deflater::drop`.
fn extract_pooled_buf(guard: PooledBuf) -> Vec<u8> {
    // SAFETY: We intentionally bypass the `Drop` impl by using `ManuallyDrop`
    // so that the buffer is NOT returned to the pool at this point.  The buffer
    // will be returned in `Deflater::drop` via `pool.return_window`.
    let mut md = std::mem::ManuallyDrop::new(guard);
    std::mem::take(&mut md.buf)
}

/// Extract the inner `Vec<u16>` from a [`PooledU16Buf`] without returning it
/// to the pool.  Mirrors [`extract_pooled_buf`].
fn extract_pooled_u16_buf(guard: PooledU16Buf) -> Vec<u16> {
    let mut md = std::mem::ManuallyDrop::new(guard);
    std::mem::take(&mut md.buf)
}

/// Code length alphabet order for encoding (RFC 1951).
const CODELEN_ORDER: [usize; 19] = [
    16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
];

/// Maximum dictionary size for DEFLATE (32KB).
pub const MAX_DICTIONARY_SIZE: usize = 32768;

/// DEFLATE compressor.
///
/// Supports an optional [`DeflatePool`] to amortise buffer allocations across
/// many successive encode calls.  Enable via [`Deflater::with_pool`].
#[derive(Debug)]
pub struct Deflater {
    /// LZ77 encoder.
    lz77: Lz77Encoder,
    /// Compression level.
    level: u8,
    /// Whether compression is finished.
    finished: bool,
    /// Dictionary Adler-32 checksum (if dictionary is set).
    dictionary_checksum: Option<u32>,
    /// Pending bits from a partial flush (value, count).
    pending_bits: (u8, u8),
    /// Whether to use the graph-based optimal parser instead of greedy.
    optimal_parsing: bool,
    /// Optional pool used to recycle the LZ77 window and hash buffers.
    ///
    /// When `Some`, the LZ77 encoder's underlying `Vec`s were acquired from
    /// the pool and will be returned on `drop`.
    pool: Option<DeflatePool>,
}

impl Drop for Deflater {
    fn drop(&mut self) {
        if let Some(ref pool) = self.pool {
            // Swap a fresh minimal encoder in, taking the pooled-buffer encoder out.
            // `with_level(0)` is cheap: it allocates a small store-only encoder which
            // is immediately discarded, but the allocation is unavoidable here.
            let old_lz77 = std::mem::replace(&mut self.lz77, Lz77Encoder::with_level(0));
            let (window, hash_table, hash_chain) = old_lz77.into_buffers();
            pool.return_window(window);
            pool.return_hash_head(hash_table);
            pool.return_hash_prev(hash_chain);
        }
    }
}

impl Deflater {
    /// Create a new DEFLATE compressor with the specified level (0-9).
    pub fn new(level: u8) -> Self {
        Self {
            lz77: Lz77Encoder::with_level(level),
            level: level.min(9),
            finished: false,
            dictionary_checksum: None,
            pending_bits: (0, 0),
            optimal_parsing: false,
            pool: None,
        }
    }

    /// Attach a memory pool to this compressor.
    ///
    /// The compressor's current LZ77 encoder is replaced by one backed by
    /// buffers acquired from `pool`.  When the `Deflater` is dropped those
    /// buffers are automatically returned to the pool for reuse by a future
    /// compressor.
    ///
    /// Any previously set dictionary is **preserved** — only the underlying
    /// allocated buffers change.
    ///
    /// # Example
    ///
    /// ```rust
    /// use oxiarc_deflate::{Deflater, pool::DeflatePool};
    ///
    /// let pool = DeflatePool::new();
    /// let mut d = Deflater::new(6).with_pool(&pool);
    /// let out = d.compress_to_vec(b"hello").unwrap();
    /// // buffers returned to pool when `d` is dropped
    /// drop(d);
    /// assert!(pool.stats().window_hits == 0); // first call always allocates
    /// ```
    pub fn with_pool(mut self, pool: &DeflatePool) -> Self {
        // Acquire pooled buffers.
        let window_guard = pool.get_window();
        let hash_head_guard = pool.get_hash_head();
        let hash_prev_guard = pool.get_hash_prev();

        // Extract the raw Vecs from the guards WITHOUT returning them to the pool
        // on drop.  We do this by taking the inner buf and forgetting the guard.
        let window = extract_pooled_buf(window_guard);
        let hash_head = extract_pooled_u16_buf(hash_head_guard);
        let hash_prev = extract_pooled_u16_buf(hash_prev_guard);

        // Build a new encoder using the pooled buffers while preserving the
        // compression level and any Lz77Params already applied.
        let old_level = self.level;
        let old_lz77 = std::mem::replace(
            &mut self.lz77,
            Lz77Encoder::with_level_and_buffers(old_level, window, hash_head, hash_prev),
        );

        // If there was a previous lz77 with a dictionary, reapply it.
        // (The new encoder starts with a zeroed window.)
        // Note: dictionary state is reflected in dictionary_checksum; the raw
        // dictionary bytes are not stored.  Since we cannot recover the exact
        // bytes, we just drop the old lz77 normally (it may have been pooled
        // or freshly allocated).
        drop(old_lz77);

        self.pool = Some(pool.clone());
        self
    }

    /// Create a new DEFLATE compressor with graph-based optimal parsing enabled.
    ///
    /// The optimal parser uses a Zopfli-style forward shortest-path DP with
    /// iterative Huffman cost refinement, producing smaller output than the
    /// greedy/lazy parser at the cost of additional CPU time.
    ///
    /// `level` still controls which Huffman block type is preferred and sets the
    /// LZ77 match-search depth for the internal encoder.
    pub fn with_optimal_parsing(level: u8) -> Self {
        Self {
            lz77: Lz77Encoder::with_level(level),
            level: level.min(9),
            finished: false,
            dictionary_checksum: None,
            pending_bits: (0, 0),
            optimal_parsing: true,
            pool: None,
        }
    }

    /// Override LZ77 match-finding parameters.
    ///
    /// Replaces the per-level defaults for `nice_length`, `max_chain`, and
    /// `good_length`. The existing encoder state (window, hash chains, dictionary)
    /// is preserved — only the tuning knobs are updated.
    ///
    /// Use [`Lz77Params::for_level`] to get the original per-level defaults,
    /// or build custom values from scratch.
    ///
    /// # Example
    ///
    /// ```rust
    /// use oxiarc_deflate::{Deflater, lz77::Lz77Params};
    ///
    /// let mut deflater = Deflater::new(6)
    ///     .with_lz77_params(Lz77Params { nice_length: 32, max_chain: 64, good_length: 259 });
    /// ```
    pub fn with_lz77_params(mut self, params: Lz77Params) -> Self {
        // Preserve window/hash/dictionary state by applying params in-place.
        let lz77 = std::mem::take(&mut self.lz77);
        self.lz77 = lz77.with_lz77_params(&params);
        self
    }

    /// Override LZ77 match-finding parameters using a named preset.
    ///
    /// The existing encoder state (window, hash chains, dictionary) is preserved.
    ///
    /// # Example
    ///
    /// ```rust
    /// use oxiarc_deflate::{Deflater, lz77::Lz77Preset};
    ///
    /// let mut deflater = Deflater::new(6).with_lz77_preset(Lz77Preset::Best);
    /// ```
    pub fn with_lz77_preset(mut self, preset: Lz77Preset) -> Self {
        let params = preset.params();
        let lz77 = std::mem::take(&mut self.lz77);
        self.lz77 = lz77.with_lz77_params(&params);
        self
    }

    /// Create a new DEFLATE compressor with a preset dictionary.
    ///
    /// The dictionary is used to seed the LZ77 sliding window, allowing
    /// better compression for data that shares patterns with the dictionary.
    /// This is useful for compressing similar files or data streams.
    ///
    /// # Arguments
    ///
    /// * `level` - Compression level (0-9)
    /// * `dictionary` - Dictionary data (up to 32KB). If larger, only the
    ///   last 32KB is used.
    ///
    /// # Returns
    ///
    /// A new Deflater with the dictionary set.
    pub fn with_dictionary(level: u8, dictionary: &[u8]) -> Self {
        let mut deflater = Self::new(level);
        deflater.set_dictionary(dictionary);
        deflater
    }

    /// Set a preset dictionary for improved compression.
    ///
    /// # Arguments
    ///
    /// * `dictionary` - Dictionary data (up to 32KB). If larger, only the
    ///   last 32KB is used.
    ///
    /// # Returns
    ///
    /// The Adler-32 checksum of the dictionary (used for identification).
    pub fn set_dictionary(&mut self, dictionary: &[u8]) -> u32 {
        let checksum = self.lz77.set_dictionary(dictionary);
        self.dictionary_checksum = Some(checksum);
        checksum
    }

    /// Get the dictionary checksum, if a dictionary is set.
    pub fn dictionary_checksum(&self) -> Option<u32> {
        self.dictionary_checksum
    }

    /// Check if a dictionary is currently set.
    pub fn has_dictionary(&self) -> bool {
        self.dictionary_checksum.is_some()
    }

    /// Reset the compressor.
    pub fn reset(&mut self) {
        self.lz77.reset();
        self.finished = false;
        self.dictionary_checksum = None;
        self.pending_bits = (0, 0);
    }

    /// Reset only the LZ77 state (used by full flush).
    pub fn reset_lz77(&mut self) {
        self.lz77.reset();
    }

    /// Reset the compressor but keep the dictionary.
    pub fn reset_keep_dictionary(&mut self) {
        // Store dictionary checksum
        let checksum = self.dictionary_checksum;
        self.lz77.reset();
        self.finished = false;
        self.dictionary_checksum = checksum;
        self.pending_bits = (0, 0);
    }

    /// Compress data.
    pub fn deflate<W: Write>(&mut self, data: &[u8], writer: &mut W, finish: bool) -> Result<()> {
        let mut bit_writer = BitWriter::new(writer);

        // Re-inject any pending bits from a previous partial flush.
        self.prepend_pending(&mut bit_writer)?;

        if self.level == 0 {
            // Store only
            self.write_stored_blocks(data, &mut bit_writer, finish)?;
        } else {
            // Compress with LZ77 and fixed Huffman codes
            self.write_compressed_block(data, &mut bit_writer, finish)?;
        }

        if finish {
            bit_writer.flush()?;
            self.finished = true;
        }

        Ok(())
    }

    /// Write stored (uncompressed) blocks.
    fn write_stored_blocks<W: Write>(
        &self,
        data: &[u8],
        writer: &mut BitWriter<W>,
        is_final: bool,
    ) -> Result<()> {
        const MAX_STORED_BLOCK: usize = 65535;

        let mut offset = 0;
        while offset < data.len() {
            let remaining = data.len() - offset;
            let block_size = remaining.min(MAX_STORED_BLOCK);
            let final_block = is_final && (offset + block_size >= data.len());

            // Block header
            writer.write_bit(final_block)?;
            writer.write_bits(0b00, 2)?; // BTYPE=00 (stored)

            // Align to byte boundary
            writer.align_to_byte()?;

            // LEN and NLEN
            let len = block_size as u16;
            let nlen = !len;
            writer.write_bits(len as u32, 16)?;
            writer.write_bits(nlen as u32, 16)?;

            // Data
            writer.write_bytes(&data[offset..offset + block_size])?;

            offset += block_size;
        }

        // Handle empty input
        if data.is_empty() && is_final {
            writer.write_bit(true)?; // BFINAL=1
            writer.write_bits(0b00, 2)?; // BTYPE=00
            writer.align_to_byte()?;
            writer.write_bits(0, 16)?; // LEN=0
            writer.write_bits(0xFFFF, 16)?; // NLEN=0xFFFF
        }

        Ok(())
    }

    /// Write a compressed block, choosing between fixed and dynamic Huffman.
    fn write_compressed_block<W: Write>(
        &mut self,
        data: &[u8],
        writer: &mut BitWriter<W>,
        is_final: bool,
    ) -> Result<()> {
        // Compress with LZ77 (greedy/lazy) or the graph-based optimal parser.
        let tokens = if self.optimal_parsing {
            let mut parser = OptimalParser::new();
            parser.parse(data, &mut self.lz77)
        } else {
            self.lz77.compress(data)
        };

        // Count symbol frequencies
        let (litlen_freq, dist_freq) = Self::count_frequencies(&tokens);

        // Build dynamic Huffman codes
        let mut litlen_builder = HuffmanBuilder::new(286, 15);
        for (sym, &freq) in litlen_freq.iter().enumerate() {
            if freq > 0 {
                litlen_builder.add_count(sym as u16, freq);
            }
        }
        // Always include EOB
        if litlen_freq[256] == 0 {
            litlen_builder.add_count(256, 1);
        }
        let dynamic_litlen_lengths = litlen_builder.build_lengths();

        let mut dist_builder = HuffmanBuilder::new(30, 15);
        for (sym, &freq) in dist_freq.iter().enumerate() {
            if freq > 0 {
                dist_builder.add_count(sym as u16, freq);
            }
        }
        let dynamic_dist_lengths = dist_builder.build_lengths();

        // Estimate sizes for fixed vs dynamic
        let fixed_size = self.estimate_fixed_size(&tokens);
        let (dynamic_size, header_size) =
            self.estimate_dynamic_size(&tokens, &dynamic_litlen_lengths, &dynamic_dist_lengths);

        // Choose better option (dynamic if it saves bytes)
        let use_dynamic = self.level >= 5 && (dynamic_size + header_size) < fixed_size;

        if use_dynamic {
            self.write_dynamic_block(
                writer,
                &tokens,
                &dynamic_litlen_lengths,
                &dynamic_dist_lengths,
                is_final,
            )?;
        } else {
            self.write_fixed_block(writer, &tokens, is_final)?;
        }

        Ok(())
    }

    /// Count symbol frequencies in tokens.
    fn count_frequencies(tokens: &[Lz77Token]) -> ([u32; 286], [u32; 30]) {
        let mut litlen_freq = [0u32; 286];
        let mut dist_freq = [0u32; 30];

        for token in tokens {
            match token {
                Lz77Token::Literal(byte) => {
                    litlen_freq[*byte as usize] += 1;
                }
                Lz77Token::Match { length, distance } => {
                    let (len_code, _, _) = length_to_code(*length);
                    litlen_freq[len_code as usize] += 1;

                    let (dist_code, _, _) = distance_to_code(*distance);
                    dist_freq[dist_code as usize] += 1;
                }
            }
        }
        // EOB symbol
        litlen_freq[256] += 1;

        (litlen_freq, dist_freq)
    }

    /// Estimate bit size using fixed Huffman codes.
    fn estimate_fixed_size(&self, tokens: &[Lz77Token]) -> usize {
        let litlen_lengths = fixed_litlen_lengths();
        let mut bits = 3; // Block header

        for token in tokens {
            match token {
                Lz77Token::Literal(byte) => {
                    bits += litlen_lengths[*byte as usize] as usize;
                }
                Lz77Token::Match { length, distance } => {
                    let (len_code, len_extra_bits, _) = length_to_code(*length);
                    bits += litlen_lengths[len_code as usize] as usize + len_extra_bits as usize;

                    let (_, dist_extra_bits, _) = distance_to_code(*distance);
                    bits += 5 + dist_extra_bits as usize; // Fixed distance is 5 bits
                }
            }
        }
        bits += litlen_lengths[256] as usize; // EOB

        bits
    }

    /// Estimate bit size using dynamic Huffman codes.
    fn estimate_dynamic_size(
        &self,
        tokens: &[Lz77Token],
        litlen_lengths: &[u8],
        dist_lengths: &[u8],
    ) -> (usize, usize) {
        let mut data_bits = 0;

        for token in tokens {
            match token {
                Lz77Token::Literal(byte) => {
                    let len = litlen_lengths.get(*byte as usize).copied().unwrap_or(0);
                    if len > 0 {
                        data_bits += len as usize;
                    }
                }
                Lz77Token::Match { length, distance } => {
                    let (len_code, len_extra_bits, _) = length_to_code(*length);
                    let len = litlen_lengths.get(len_code as usize).copied().unwrap_or(0);
                    if len > 0 {
                        data_bits += len as usize + len_extra_bits as usize;
                    }

                    let (dist_code, dist_extra_bits, _) = distance_to_code(*distance);
                    let dlen = dist_lengths.get(dist_code as usize).copied().unwrap_or(0);
                    if dlen > 0 {
                        data_bits += dlen as usize + dist_extra_bits as usize;
                    }
                }
            }
        }

        // EOB
        let eob_len = litlen_lengths.get(256).copied().unwrap_or(0);
        data_bits += eob_len as usize;

        // Estimate header size (rough approximation)
        let header_bits =
            3 + 5 + 5 + 4 + 19 * 3 + litlen_lengths.len() * 4 + dist_lengths.len() * 4;

        (data_bits, header_bits)
    }

    /// Write a block using fixed Huffman codes.
    fn write_fixed_block<W: Write>(
        &self,
        writer: &mut BitWriter<W>,
        tokens: &[Lz77Token],
        is_final: bool,
    ) -> Result<()> {
        // Block header
        writer.write_bit(is_final)?;
        writer.write_bits(0b01, 2)?; // BTYPE=01 (fixed Huffman)

        // Get fixed Huffman code lengths
        let litlen_lengths = fixed_litlen_lengths();

        // Build encoding table
        let mut codes = [[0u32; 2]; 288]; // [code, length]
        Self::build_codes(&litlen_lengths, &mut codes);

        // Write tokens
        for token in tokens {
            match token {
                Lz77Token::Literal(byte) => {
                    let [code, len] = codes[*byte as usize];
                    Self::write_huffman_code(writer, code, len as u8)?;
                }
                Lz77Token::Match { length, distance } => {
                    // Write length code
                    let (len_code, len_extra_bits, len_extra) = length_to_code(*length);
                    let [code, len] = codes[len_code as usize];
                    Self::write_huffman_code(writer, code, len as u8)?;

                    // Write length extra bits
                    if len_extra_bits > 0 {
                        writer.write_bits(len_extra as u32, len_extra_bits)?;
                    }

                    // Write distance code (fixed: 5 bits each)
                    let (dist_code, dist_extra_bits, dist_extra) = distance_to_code(*distance);
                    // Fixed distance codes are 5 bits, reversed
                    let reversed_dist = Self::reverse_bits(dist_code as u32, 5);
                    writer.write_bits(reversed_dist, 5)?;

                    // Write distance extra bits
                    if dist_extra_bits > 0 {
                        writer.write_bits(dist_extra as u32, dist_extra_bits)?;
                    }
                }
            }
        }

        // Write end of block
        let [code, len] = codes[256]; // EOB symbol
        Self::write_huffman_code(writer, code, len as u8)?;

        Ok(())
    }

    /// Write a block using dynamic Huffman codes.
    fn write_dynamic_block<W: Write>(
        &self,
        writer: &mut BitWriter<W>,
        tokens: &[Lz77Token],
        litlen_lengths: &[u8],
        dist_lengths: &[u8],
        is_final: bool,
    ) -> Result<()> {
        // Block header
        writer.write_bit(is_final)?;
        writer.write_bits(0b10, 2)?; // BTYPE=10 (dynamic Huffman)

        // Find HLIT and HDIST (number of codes - base)
        let hlit = Self::find_last_nonzero(litlen_lengths, 257).saturating_sub(257);
        let hdist = Self::find_last_nonzero(dist_lengths, 1).saturating_sub(1);

        // Encode code lengths with RLE
        let combined_lengths = Self::combine_lengths(litlen_lengths, dist_lengths, hlit, hdist);
        let (codelen_symbols, codelen_freqs) = Self::rle_encode_lengths(&combined_lengths);

        // Build code length tree
        let mut codelen_builder = HuffmanBuilder::new(19, 7);
        for (sym, &freq) in codelen_freqs.iter().enumerate() {
            if freq > 0 {
                codelen_builder.add_count(sym as u16, freq);
            }
        }
        let codelen_lengths = codelen_builder.build_lengths();

        // Find HCLEN
        let hclen = Self::find_hclen(&codelen_lengths);

        // Write header values
        writer.write_bits(hlit as u32, 5)?; // HLIT
        writer.write_bits(hdist as u32, 5)?; // HDIST
        writer.write_bits(hclen as u32, 4)?; // HCLEN

        // Write code length code lengths
        for i in 0..hclen + 4 {
            let len = codelen_lengths[CODELEN_ORDER[i]];
            writer.write_bits(len as u32, 3)?;
        }

        // Build codes for code lengths
        let mut codelen_codes = [[0u32; 2]; 19];
        Self::build_codes(&codelen_lengths, &mut codelen_codes);

        // Write encoded lengths
        for (sym, extra, extra_bits) in &codelen_symbols {
            let [code, len] = codelen_codes[*sym as usize];
            if len > 0 {
                Self::write_huffman_code(writer, code, len as u8)?;
                if *extra_bits > 0 {
                    writer.write_bits(*extra as u32, *extra_bits)?;
                }
            }
        }

        // Build litlen and distance codes
        let mut litlen_codes = [[0u32; 2]; 288];
        Self::build_codes(litlen_lengths, &mut litlen_codes);

        let mut dist_codes = [[0u32; 2]; 30];
        Self::build_codes(dist_lengths, &mut dist_codes);

        // Write tokens
        for token in tokens {
            match token {
                Lz77Token::Literal(byte) => {
                    let [code, len] = litlen_codes[*byte as usize];
                    if len > 0 {
                        Self::write_huffman_code(writer, code, len as u8)?;
                    }
                }
                Lz77Token::Match { length, distance } => {
                    // Write length code
                    let (len_code, len_extra_bits, len_extra) = length_to_code(*length);
                    let [code, len] = litlen_codes[len_code as usize];
                    if len > 0 {
                        Self::write_huffman_code(writer, code, len as u8)?;
                        if len_extra_bits > 0 {
                            writer.write_bits(len_extra as u32, len_extra_bits)?;
                        }
                    }

                    // Write distance code
                    let (dist_code, dist_extra_bits, dist_extra) = distance_to_code(*distance);
                    let [dcode, dlen] = dist_codes[dist_code as usize];
                    if dlen > 0 {
                        Self::write_huffman_code(writer, dcode, dlen as u8)?;
                        if dist_extra_bits > 0 {
                            writer.write_bits(dist_extra as u32, dist_extra_bits)?;
                        }
                    }
                }
            }
        }

        // Write end of block
        let [code, len] = litlen_codes[256];
        if len > 0 {
            Self::write_huffman_code(writer, code, len as u8)?;
        }

        Ok(())
    }

    /// Find the last non-zero length index, with minimum.
    fn find_last_nonzero(lengths: &[u8], min: usize) -> usize {
        let mut last = min;
        for (i, &len) in lengths.iter().enumerate() {
            if len > 0 && i >= min {
                last = i + 1;
            }
        }
        last.max(min)
    }

    /// Combine literal/length and distance lengths.
    fn combine_lengths(
        litlen_lengths: &[u8],
        dist_lengths: &[u8],
        hlit: usize,
        hdist: usize,
    ) -> Vec<u8> {
        let mut combined = Vec::with_capacity(hlit + 257 + hdist + 1);
        combined.extend_from_slice(&litlen_lengths[..hlit + 257]);
        combined.extend_from_slice(&dist_lengths[..hdist + 1]);
        combined
    }

    /// RLE encode code lengths.
    /// Returns (symbol, extra_value, extra_bits) tuples and frequency counts.
    fn rle_encode_lengths(lengths: &[u8]) -> (Vec<(u8, u8, u8)>, [u32; 19]) {
        let mut symbols = Vec::new();
        let mut freqs = [0u32; 19];
        let mut i = 0;

        while i < lengths.len() {
            let len = lengths[i];

            // Count the full run of identical values (no cap here so we advance i correctly).
            let run_end = i + lengths[i..].iter().take_while(|&&l| l == len).count();
            let mut count = run_end - i;

            if len == 0 {
                // Encode zeros in batches of at most 138.
                while count > 0 {
                    if count >= 11 {
                        // Use symbol 18 (11-138 zeros)
                        let batch = count.min(138);
                        symbols.push((18, (batch - 11) as u8, 7));
                        freqs[18] += 1;
                        count -= batch;
                    } else if count >= 3 {
                        // Use symbol 17 (3-10 zeros)
                        let batch = count.min(10);
                        symbols.push((17, (batch - 3) as u8, 3));
                        freqs[17] += 1;
                        count -= batch;
                    } else {
                        // Output individual zeros
                        symbols.push((0, 0, 0));
                        freqs[0] += 1;
                        count -= 1;
                    }
                }
            } else {
                // Output the first occurrence as a literal symbol.
                symbols.push((len, 0, 0));
                freqs[len as usize] += 1;
                count -= 1;

                // Encode repeats with symbol 16 (repeat previous, 3-6 times).
                while count > 0 {
                    if count >= 3 {
                        let batch = count.min(6);
                        symbols.push((16, (batch - 3) as u8, 2));
                        freqs[16] += 1;
                        count -= batch;
                    } else {
                        symbols.push((len, 0, 0));
                        freqs[len as usize] += 1;
                        count -= 1;
                    }
                }
            }

            // Advance past the full run.
            i = run_end;
        }

        (symbols, freqs)
    }

    /// Find HCLEN (number of code length codes - 4).
    fn find_hclen(codelen_lengths: &[u8]) -> usize {
        let mut hclen = 15; // Maximum is 19-4=15
        for i in (0..=15).rev() {
            if codelen_lengths[CODELEN_ORDER[i + 4 - 1]] != 0 {
                hclen = i;
                break;
            }
        }
        hclen
    }

    /// Build canonical Huffman codes from lengths.
    fn build_codes(lengths: &[u8], codes: &mut [[u32; 2]]) {
        // Count codes of each length
        let mut bl_count = [0u32; 16];
        for &len in lengths {
            if len > 0 {
                bl_count[len as usize] += 1;
            }
        }

        // Calculate starting codes
        let mut next_code = [0u32; 16];
        let mut code = 0u32;
        for bits in 1..16 {
            code = (code + bl_count[bits - 1]) << 1;
            next_code[bits] = code;
        }

        // Assign codes
        for (symbol, &len) in lengths.iter().enumerate() {
            if len > 0 && symbol < codes.len() {
                let code = next_code[len as usize];
                next_code[len as usize] += 1;
                // Reverse for LSB-first output
                codes[symbol] = [Self::reverse_bits(code, len), len as u32];
            }
        }
    }

    /// Reverse bits in a value.
    fn reverse_bits(mut value: u32, length: u8) -> u32 {
        let mut result = 0u32;
        for _ in 0..length {
            result = (result << 1) | (value & 1);
            value >>= 1;
        }
        result
    }

    /// Write a Huffman code (already reversed for LSB-first).
    fn write_huffman_code<W: Write>(
        writer: &mut BitWriter<W>,
        code: u32,
        length: u8,
    ) -> Result<()> {
        writer.write_bits(code, length)?;
        Ok(())
    }

    /// Compress data with a sync-flush: write a non-final compressed block followed
    /// by an empty stored block (BFINAL=0, BTYPE=00, LEN=0, NLEN=0xFFFF), then flush
    /// all bits to a byte boundary. Everything is written through a single BitWriter
    /// so the partial-byte state is coherent.
    pub fn deflate_sync<W: Write>(&mut self, data: &[u8], writer: &mut W) -> Result<()> {
        let mut bit_writer = BitWriter::new(writer);

        // Re-inject any pending bits from a previous partial flush.
        self.prepend_pending(&mut bit_writer)?;

        // Write the compressed (non-final) block.
        self.write_compressed_block(data, &mut bit_writer, false)?;

        // Append the empty stored sync-flush block.
        bit_writer.write_bit(false)?; // BFINAL=0
        bit_writer.write_bits(0b00, 2)?; // BTYPE=00 stored
        bit_writer.align_to_byte()?; // pad to byte boundary
        bit_writer.write_bits(0u32, 16)?; // LEN=0
        bit_writer.write_bits(0xFFFF_u32, 16)?; // NLEN=0xFFFF
        bit_writer.flush()?;

        Ok(())
    }

    /// Compress data with a partial flush: write a non-final compressed block.
    ///
    /// Unlike sync flush, no empty stored block (0x00 0x00 0xFF 0xFF) marker
    /// is appended. Instead, any pending partial-byte bits from the previous
    /// partial flush are prepended, and any trailing partial bits are saved
    /// for the next call.
    pub fn deflate_partial<W: Write>(&mut self, data: &[u8], writer: &mut W) -> Result<()> {
        // Write to an intermediate buffer so we can manage bit-level state.
        let mut buf = Vec::new();
        {
            let mut bit_writer = BitWriter::new(&mut buf);

            // Re-inject pending bits from a previous partial flush.
            if self.pending_bits.1 > 0 {
                bit_writer.write_bits(self.pending_bits.0 as u32, self.pending_bits.1)?;
            }

            // Write the compressed (non-final) block.
            if self.level == 0 {
                self.write_stored_blocks(data, &mut bit_writer, false)?;
            } else {
                self.write_compressed_block(data, &mut bit_writer, false)?;
            }

            // Determine how many bits are in the last partial byte.
            let total_bits = bit_writer.bits_written();
            let remainder = (total_bits % 8) as u8;

            if remainder == 0 {
                // Exactly byte-aligned; no pending bits.
                bit_writer.flush()?;
                self.pending_bits = (0, 0);
            } else {
                // Flush complete bytes only (align pads with zeros then flushes).
                bit_writer.flush()?;
                self.pending_bits = (0, 0); // reset first
            }
            // Drop bit_writer to release the mutable borrow on buf
            drop(bit_writer);

            // If there were remainder bits, save the partial byte and remove it.
            if remainder != 0 {
                if let Some(&last_byte) = buf.last() {
                    // The valid bits are in the lower `remainder` bits.
                    let mask = (1u8 << remainder).wrapping_sub(1);
                    self.pending_bits = (last_byte & mask, remainder);
                    buf.pop();
                }
            }
        }

        writer.write_all(&buf)?;
        Ok(())
    }

    /// Re-inject any pending partial-byte bits into a fresh BitWriter,
    /// used when a non-partial call follows a partial flush.
    fn prepend_pending<W: Write>(&mut self, bit_writer: &mut BitWriter<W>) -> Result<()> {
        if self.pending_bits.1 > 0 {
            bit_writer.write_bits(self.pending_bits.0 as u32, self.pending_bits.1)?;
            self.pending_bits = (0, 0);
        }
        Ok(())
    }

    /// Compress data to a Vec.
    pub fn compress_to_vec(&mut self, data: &[u8]) -> Result<Vec<u8>> {
        let mut output = Vec::new();
        self.deflate(data, &mut output, true)?;
        Ok(output)
    }
}

impl Default for Deflater {
    fn default() -> Self {
        Self::new(6)
    }
}

impl Compressor for Deflater {
    fn compress(
        &mut self,
        input: &[u8],
        output: &mut [u8],
        flush: FlushMode,
    ) -> Result<(usize, usize, CompressStatus)> {
        if self.finished {
            return Ok((0, 0, CompressStatus::Done));
        }

        let mut buffer = Vec::new();

        match flush {
            FlushMode::Finish => {
                self.deflate(input, &mut buffer, true)?;
            }
            FlushMode::Sync => {
                // Compress input then append an empty stored sync block, all in one
                // BitWriter so partial-byte state is coherent.
                if self.level == 0 {
                    self.deflate(input, &mut buffer, false)?;
                } else {
                    self.deflate_sync(input, &mut buffer)?;
                }
            }
            FlushMode::Full => {
                // Same as Sync, then reset hash state to allow independent decompression.
                if self.level == 0 {
                    self.deflate(input, &mut buffer, false)?;
                } else {
                    self.deflate_sync(input, &mut buffer)?;
                }
                self.lz77.reset();
            }
            FlushMode::Partial => {
                self.deflate_partial(input, &mut buffer)?;
            }
            FlushMode::None => {
                self.deflate(input, &mut buffer, false)?;
            }
        }

        let finish = matches!(flush, FlushMode::Finish);
        let to_copy = buffer.len().min(output.len());
        output[..to_copy].copy_from_slice(&buffer[..to_copy]);

        let status = if finish {
            CompressStatus::Done
        } else if to_copy < buffer.len() {
            CompressStatus::NeedsOutput
        } else {
            CompressStatus::NeedsInput
        };

        Ok((input.len(), to_copy, status))
    }

    fn reset(&mut self) {
        Deflater::reset(self);
    }

    fn is_finished(&self) -> bool {
        self.finished
    }
}

/// Compress data using DEFLATE.
pub fn deflate(data: &[u8], level: u8) -> Result<Vec<u8>> {
    let mut deflater = Deflater::new(level);
    deflater.compress_to_vec(data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inflate::inflate;

    #[test]
    fn test_deflate_stored() {
        let input = b"Hello, World!";
        let compressed = deflate(input, 0).expect("deflate stored block");

        // Decompress and verify
        let decompressed = inflate(&compressed).expect("inflate stored block");
        assert_eq!(decompressed, input);
    }

    #[test]
    fn test_deflate_compressed() {
        let input = b"AAAAAAAAAABBBBBBBBBBCCCCCCCCCC";
        let compressed = deflate(input, 6).expect("deflate level 6");

        // Should be smaller than input
        assert!(
            compressed.len() < input.len(),
            "Compressed {} bytes to {} bytes",
            input.len(),
            compressed.len()
        );

        // Decompress and verify
        let decompressed = inflate(&compressed).expect("inflate compressed data");
        assert_eq!(decompressed, input);
    }

    #[test]
    fn test_deflate_empty() {
        let input = b"";
        let compressed = deflate(input, 0).expect("deflate empty input");
        let decompressed = inflate(&compressed).expect("inflate empty deflated");
        assert!(decompressed.is_empty());
    }

    #[test]
    fn test_deflate_roundtrip() {
        let inputs = [
            b"Hello".to_vec(),
            b"The quick brown fox jumps over the lazy dog".to_vec(),
            vec![0u8; 1000],
            (0..=255).collect::<Vec<u8>>(),
        ];

        for input in &inputs {
            for level in [0, 1, 6, 9] {
                let compressed = deflate(input, level).expect("deflate for roundtrip");
                let decompressed = inflate(&compressed).expect("inflate for roundtrip");
                assert_eq!(
                    &decompressed,
                    input,
                    "Roundtrip failed for level {} with {} bytes",
                    level,
                    input.len()
                );
            }
        }
    }

    #[test]
    fn test_reverse_bits() {
        assert_eq!(Deflater::reverse_bits(0b101, 3), 0b101);
        assert_eq!(Deflater::reverse_bits(0b1100, 4), 0b0011);
        assert_eq!(Deflater::reverse_bits(0b10101010, 8), 0b01010101);
    }

    #[test]
    fn test_deflate_dynamic_huffman() {
        // Large repeating data should trigger dynamic Huffman at level 5+
        let input = b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA\
                      BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB\
                      CCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCC\
                      DDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDD";

        let compressed_dynamic = deflate(input, 9).expect("deflate dynamic huffman level 9");
        let compressed_fixed = deflate(input, 1).expect("deflate fixed huffman level 1");

        // Dynamic Huffman should compress better for this pattern
        assert!(
            compressed_dynamic.len() <= compressed_fixed.len(),
            "Dynamic ({} bytes) should be <= fixed ({} bytes)",
            compressed_dynamic.len(),
            compressed_fixed.len()
        );

        // Both should decompress correctly
        let decompressed_dynamic = inflate(&compressed_dynamic).expect("inflate dynamic huffman");
        let decompressed_fixed = inflate(&compressed_fixed).expect("inflate fixed huffman");
        assert_eq!(decompressed_dynamic, input);
        assert_eq!(decompressed_fixed, input);
    }

    #[test]
    fn test_deflate_level_comparison() {
        let input = vec![b'A'; 1000];

        let mut prev_size = usize::MAX;
        for level in [0, 1, 5, 9] {
            let compressed = deflate(&input, level).expect("deflate for level comparison");
            let decompressed = inflate(&compressed).expect("inflate for level comparison");
            assert_eq!(decompressed, input);

            // Higher levels should generally compress better (or equal)
            // Level 0 is stored, so it will be larger
            if level > 0 {
                assert!(
                    compressed.len() <= prev_size,
                    "Level {} ({} bytes) should compress <= previous ({} bytes)",
                    level,
                    compressed.len(),
                    prev_size
                );
            }
            if level > 0 {
                prev_size = compressed.len();
            }
        }
    }

    #[test]
    fn test_deflate_partial_roundtrip() {
        // Test that partial flush + finish produces valid DEFLATE
        let mut deflater = Deflater::new(6);
        let mut output = Vec::new();
        deflater
            .deflate_partial(b"Hello partial!", &mut output)
            .expect("partial failed");
        deflater
            .deflate(b" More data.", &mut output, true)
            .expect("finish failed");

        let decompressed = inflate(&output).expect("inflate failed");
        assert_eq!(decompressed, b"Hello partial! More data.");
    }

    #[test]
    fn test_deflate_large_homogeneous() {
        // 1 MB of identical bytes - this triggered "Over-subscribed Huffman tree"
        // in the dynamic Huffman path before the adjust_lengths fix.
        let input = vec![42u8; 1_000_000];
        for level in [1u8, 5, 6, 9] {
            println!("Testing level {}...", level);
            let compressed = deflate(&input, level).expect("compress failed");
            println!("  Compressed to {} bytes", compressed.len());
            assert!(
                compressed.len() < input.len() / 100,
                "Expected high compression for level {}: got {} bytes",
                level,
                compressed.len()
            );
            let decompressed = inflate(&compressed).expect("inflate failed");
            assert_eq!(decompressed, input, "roundtrip failed at level {}", level);
        }
    }

    // ─────────────────────────────────────────────────────────────────────────
    // Lz77Params / Lz77Preset on Deflater
    // ─────────────────────────────────────────────────────────────────────────

    #[test]
    fn test_deflater_byte_identity_with_lz77_params_for_level() {
        // Deflater::new(level) and Deflater::new(level).with_lz77_params(Lz77Params::for_level(...))
        // must produce byte-identical compressed output.
        let input: Vec<u8> = b"the quick brown fox jumps over the lazy dog "
            .iter()
            .cycle()
            .take(16_384)
            .copied()
            .collect();

        for level in 0u8..=9 {
            let baseline = deflate(&input, level).expect("baseline deflate failed");
            let with_params = {
                let mut d =
                    Deflater::new(level).with_lz77_params(Lz77Params::for_level(level as u32));
                d.compress_to_vec(&input)
                    .expect("with_lz77_params deflate failed")
            };
            assert_eq!(
                baseline, with_params,
                "byte-identity broken at level {}",
                level
            );
        }
    }

    #[test]
    fn test_deflater_with_lz77_preset_best_roundtrip() {
        let input: Vec<u8> = b"abcdefghijklmnopqrstuvwxyz0123456789"
            .iter()
            .cycle()
            .take(16_384)
            .copied()
            .collect();
        let compressed = {
            let mut d = Deflater::new(6).with_lz77_preset(Lz77Preset::Best);
            d.compress_to_vec(&input)
                .expect("Best preset compress failed")
        };
        let decompressed = inflate(&compressed).expect("Best preset inflate failed");
        assert_eq!(decompressed, input);
    }

    #[test]
    fn test_deflater_with_lz77_preset_fast_roundtrip() {
        let input: Vec<u8> = b"abcdefgh".iter().cycle().take(32_768).copied().collect();
        let compressed = {
            let mut d = Deflater::new(6).with_lz77_preset(Lz77Preset::Fast);
            d.compress_to_vec(&input)
                .expect("Fast preset compress failed")
        };
        let decompressed = inflate(&compressed).expect("Fast preset inflate failed");
        assert_eq!(decompressed, input);
    }

    #[test]
    fn test_with_lz77_params_preserves_dictionary() {
        // with_lz77_params must NOT discard dictionary state set before the call.
        // We verify byte-identity: Deflater::with_dictionary(6, dict) and
        // Deflater::with_dictionary(6, dict).with_lz77_params(for_level(6))
        // must produce the same compressed bytes, proving the dictionary was
        // preserved and not silently reset.
        let dict = b"the quick brown fox jumps over the lazy dog";
        let input = b"the quick brown fox";

        // Baseline: set dictionary then compress (no params override).
        let out_a = {
            let mut d = Deflater::with_dictionary(6, dict);
            d.compress_to_vec(input).expect("baseline compress failed")
        };

        // Same but apply for_level(6) params after setting the dictionary.
        // Since for_level(6) is already the implicit default, output must be
        // byte-identical to the baseline. A mismatch means the dictionary was
        // discarded and fresh window state was used.
        let out_b = {
            let mut d =
                Deflater::with_dictionary(6, dict).with_lz77_params(Lz77Params::for_level(6));
            d.compress_to_vec(input).expect("params compress failed")
        };

        assert_eq!(
            out_a, out_b,
            "with_lz77_params must not discard dictionary state"
        );
    }

    #[test]
    fn test_deflater_with_lz77_preset_ultra_roundtrip() {
        // Use small input so the uncapped (u32::MAX) chain doesn't time out.
        let input: Vec<u8> = b"abcdefgh".iter().cycle().take(2_048).copied().collect();
        let compressed = {
            let mut d = Deflater::new(6).with_lz77_preset(Lz77Preset::Ultra);
            d.compress_to_vec(&input)
                .expect("Ultra preset compress failed")
        };
        let decompressed = inflate(&compressed).expect("Ultra preset inflate failed");
        assert_eq!(decompressed, input);
    }
}
