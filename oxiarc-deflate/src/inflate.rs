//! DEFLATE decompression (inflate).
//!
//! This module implements the DEFLATE decompression algorithm as specified
//! in RFC 1951. It supports all three block types:
//! - Type 0: Stored (uncompressed)
//! - Type 1: Fixed Huffman codes
//! - Type 2: Dynamic Huffman codes

use crate::huffman::HuffmanTree;
use crate::tables::{
    CODE_LENGTH_ORDER, DISTANCE_EXTRA_BITS, LENGTH_EXTRA_BITS, decode_distance, decode_length,
    fixed_distance_tree, fixed_litlen_tree,
};
use oxiarc_core::error::{OxiArcError, Result};
use oxiarc_core::traits::{DecompressStatus, Decompressor};
use oxiarc_core::{BitReader, OutputRingBuffer};
use std::io::Read;

/// Maximum dictionary size for DEFLATE (32KB).
pub const MAX_DICTIONARY_SIZE: usize = 32768;

/// DEFLATE decompressor.
#[derive(Debug)]
pub struct Inflater {
    /// Output ring buffer.
    output: OutputRingBuffer,
    /// Whether we've seen the final block.
    final_block: bool,
    /// Whether decompression is complete.
    finished: bool,
    /// Expected dictionary checksum (if dictionary is required).
    expected_dict_checksum: Option<u32>,
    /// Set when `inflate_stored` processes a zero-length stored block (sync flush).
    last_empty_stored: bool,
}

impl Inflater {
    /// Create a new DEFLATE decompressor.
    pub fn new() -> Self {
        Self {
            output: OutputRingBuffer::with_capacity(32768, 65536),
            final_block: false,
            finished: false,
            expected_dict_checksum: None,
            last_empty_stored: false,
        }
    }

    /// Create a new DEFLATE decompressor with a preset dictionary.
    ///
    /// The dictionary must match the one used during compression.
    /// The decompressor uses the dictionary to resolve back-references
    /// that point into the dictionary content.
    ///
    /// # Arguments
    ///
    /// * `dictionary` - Dictionary data (up to 32KB). If larger, only the
    ///   last 32KB is used.
    ///
    /// # Returns
    ///
    /// A new Inflater with the dictionary preloaded.
    pub fn with_dictionary(dictionary: &[u8]) -> Self {
        let mut inflater = Self::new();
        inflater.set_dictionary(dictionary);
        inflater
    }

    /// Set a preset dictionary for decompression.
    ///
    /// # Arguments
    ///
    /// * `dictionary` - Dictionary data (up to 32KB). If larger, only the
    ///   last 32KB is used.
    ///
    /// # Returns
    ///
    /// The Adler-32 checksum of the dictionary.
    pub fn set_dictionary(&mut self, dictionary: &[u8]) -> u32 {
        self.output.preload_dictionary(dictionary);
        self.expected_dict_checksum = Some(Self::adler32(dictionary));
        self.expected_dict_checksum.unwrap_or(1)
    }

    /// Get the expected dictionary checksum.
    pub fn expected_dictionary_checksum(&self) -> Option<u32> {
        self.expected_dict_checksum
    }

    /// Check if a dictionary is currently set.
    pub fn has_dictionary(&self) -> bool {
        self.expected_dict_checksum.is_some()
    }

    /// Calculate Adler-32 checksum (for dictionary identification).
    fn adler32(data: &[u8]) -> u32 {
        const MOD_ADLER: u32 = 65521;
        const NMAX: usize = 5552;

        let mut a: u32 = 1;
        let mut b: u32 = 0;

        let mut remaining = data;

        while remaining.len() >= NMAX {
            let (chunk, rest) = remaining.split_at(NMAX);
            remaining = rest;

            for &byte in chunk {
                a += byte as u32;
                b += a;
            }

            a %= MOD_ADLER;
            b %= MOD_ADLER;
        }

        for &byte in remaining {
            a += byte as u32;
            b += a;
        }

        ((b % MOD_ADLER) << 16) | (a % MOD_ADLER)
    }

    /// Reset the decompressor.
    pub fn reset(&mut self) {
        self.output.clear();
        self.final_block = false;
        self.finished = false;
        self.expected_dict_checksum = None;
        self.last_empty_stored = false;
    }

    /// Reset the decompressor but keep the dictionary.
    pub fn reset_keep_dictionary(&mut self) {
        let checksum = self.expected_dict_checksum;
        self.output.clear();
        self.final_block = false;
        self.finished = false;
        self.expected_dict_checksum = checksum;
        self.last_empty_stored = false;
    }

    /// Decompress data from a reader.
    pub fn inflate_reader<R: Read>(&mut self, reader: &mut R) -> Result<Vec<u8>> {
        let mut bit_reader = BitReader::new(reader);
        self.inflate(&mut bit_reader)
    }

    /// Decompress data from a caller-owned [`BitReader`] and also report
    /// how many whole bytes of the underlying byte-stream the DEFLATE
    /// data consumed.
    ///
    /// DEFLATE is bit-aligned but this method rounds the consumed-bit
    /// count up to the next whole byte — this is the byte count needed
    /// by formats that place a byte-aligned trailer immediately after
    /// the compressed stream (e.g., ZIP's data-descriptor per APPNOTE
    /// §4.3.9).
    ///
    /// Because the `BitReader` is owned by the caller, any further reads
    /// on the SAME `BitReader` after this method returns will correctly
    /// drain its internal buffer before touching the underlying reader —
    /// no bytes are lost to the DEFLATE prefetch.
    ///
    /// On return, `reader` is aligned to the next byte boundary
    /// (intra-byte padding bits have been skipped). Reads via the
    /// `BitReader` continue from that byte boundary.
    pub fn inflate_consumed<R: Read>(
        &mut self,
        reader: &mut BitReader<R>,
    ) -> Result<(Vec<u8>, u64)> {
        let bits_before = reader.bits_read();
        let decompressed = self.inflate(reader)?;
        // Align to the byte boundary so byte-aligned structures following
        // the DEFLATE stream start at a clean offset.
        reader.align_to_byte();
        let bits_after = reader.bits_read();
        let consumed = (bits_after - bits_before).div_ceil(8);
        Ok((decompressed, consumed))
    }

    /// Decompress data from a bit reader.
    pub fn inflate<R: Read>(&mut self, reader: &mut BitReader<R>) -> Result<Vec<u8>> {
        while !self.final_block {
            self.inflate_block(reader)?;
        }

        self.finished = true;
        Ok(self.output.output().to_vec())
    }

    /// Decompress a single block.
    fn inflate_block<R: Read>(&mut self, reader: &mut BitReader<R>) -> Result<()> {
        // Read block header
        let bfinal = reader.read_bit()?;
        let btype = reader.read_bits(2)?;

        self.final_block = bfinal;

        match btype {
            0 => self.inflate_stored(reader),
            1 => self.inflate_fixed(reader),
            2 => self.inflate_dynamic(reader),
            3 => Err(OxiArcError::invalid_header("Reserved block type 3")),
            _ => unreachable!(),
        }
    }

    /// Decompress a stored (uncompressed) block.
    fn inflate_stored<R: Read>(&mut self, reader: &mut BitReader<R>) -> Result<()> {
        // Align to byte boundary
        reader.align_to_byte();

        // Read LEN and NLEN
        let len = reader.read_bits(16)? as u16;
        let nlen = reader.read_bits(16)? as u16;

        // Validate
        if len != !nlen {
            return Err(OxiArcError::corrupted(
                reader.bit_position() / 8,
                format!("LEN/NLEN mismatch: {} vs {}", len, !nlen),
            ));
        }

        // Detect sync-flush: empty stored block (LEN=0).
        self.last_empty_stored = len == 0;

        // Copy bytes
        let mut buf = vec![0u8; len as usize];
        reader.read_bytes(&mut buf)?;
        self.output.write_literals(&buf);

        Ok(())
    }

    /// Decompress a block with fixed Huffman codes.
    fn inflate_fixed<R: Read>(&mut self, reader: &mut BitReader<R>) -> Result<()> {
        let litlen_tree = fixed_litlen_tree()?;
        let dist_tree = fixed_distance_tree()?;

        self.inflate_huffman(reader, litlen_tree, dist_tree)
    }

    /// Decompress a block with dynamic Huffman codes.
    fn inflate_dynamic<R: Read>(&mut self, reader: &mut BitReader<R>) -> Result<()> {
        // Read code counts
        let hlit = reader.read_bits(5)? as usize + 257; // literal/length codes
        let hdist = reader.read_bits(5)? as usize + 1; // distance codes
        let hclen = reader.read_bits(4)? as usize + 4; // code length codes

        // Read code length code lengths
        let mut code_length_lengths = [0u8; 19];
        for i in 0..hclen {
            code_length_lengths[CODE_LENGTH_ORDER[i]] = reader.read_bits(3)? as u8;
        }

        // Build code length tree
        let code_length_tree = HuffmanTree::from_code_lengths(&code_length_lengths)?;

        // Read literal/length and distance code lengths
        let mut all_lengths = vec![0u8; hlit + hdist];
        let mut i = 0;

        while i < all_lengths.len() {
            let code = code_length_tree.decode(reader)?;

            match code {
                0..=15 => {
                    all_lengths[i] = code as u8;
                    i += 1;
                }
                16 => {
                    // Copy previous length 3-6 times
                    if i == 0 {
                        return Err(OxiArcError::corrupted(
                            reader.bit_position() / 8,
                            "Code 16 at start of lengths",
                        ));
                    }
                    let repeat = reader.read_bits(2)? as usize + 3;
                    let prev = all_lengths[i - 1];
                    for _ in 0..repeat {
                        if i >= all_lengths.len() {
                            return Err(OxiArcError::corrupted(
                                reader.bit_position() / 8,
                                "Code length overflow",
                            ));
                        }
                        all_lengths[i] = prev;
                        i += 1;
                    }
                }
                17 => {
                    // Repeat 0 for 3-10 times
                    let repeat = reader.read_bits(3)? as usize + 3;
                    for _ in 0..repeat {
                        if i >= all_lengths.len() {
                            return Err(OxiArcError::corrupted(
                                reader.bit_position() / 8,
                                "Code length overflow",
                            ));
                        }
                        all_lengths[i] = 0;
                        i += 1;
                    }
                }
                18 => {
                    // Repeat 0 for 11-138 times
                    let repeat = reader.read_bits(7)? as usize + 11;
                    for _ in 0..repeat {
                        if i >= all_lengths.len() {
                            return Err(OxiArcError::corrupted(
                                reader.bit_position() / 8,
                                "Code length overflow",
                            ));
                        }
                        all_lengths[i] = 0;
                        i += 1;
                    }
                }
                _ => {
                    return Err(OxiArcError::invalid_huffman(reader.bit_position()));
                }
            }
        }

        // Split into literal/length and distance lengths
        let litlen_lengths = &all_lengths[..hlit];
        let dist_lengths = &all_lengths[hlit..];

        // Build trees
        let litlen_tree = HuffmanTree::from_code_lengths(litlen_lengths)?;
        let dist_tree = HuffmanTree::from_code_lengths(dist_lengths)?;

        self.inflate_huffman(reader, &litlen_tree, &dist_tree)
    }

    /// Decompress using Huffman codes.
    fn inflate_huffman<R: Read>(
        &mut self,
        reader: &mut BitReader<R>,
        litlen_tree: &HuffmanTree,
        dist_tree: &HuffmanTree,
    ) -> Result<()> {
        loop {
            let code = litlen_tree.decode(reader)?;

            if code < 256 {
                // Literal byte
                self.output.write_literal(code as u8);
            } else if code == 256 {
                // End of block
                break;
            } else if code <= 285 {
                // Length code
                let length_idx = (code - 257) as usize;
                let extra_bits = LENGTH_EXTRA_BITS[length_idx];
                let extra = reader.read_bits(extra_bits)? as u16;
                let length = decode_length(code, extra);

                // Read distance
                let dist_code = dist_tree.decode(reader)?;
                if dist_code >= 30 {
                    return Err(OxiArcError::corrupted(
                        reader.bit_position() / 8,
                        format!("Invalid distance code: {}", dist_code),
                    ));
                }

                let dist_extra_bits = DISTANCE_EXTRA_BITS[dist_code as usize];
                let dist_extra = reader.read_bits(dist_extra_bits)? as u16;
                let distance = decode_distance(dist_code, dist_extra);

                // Copy from history
                self.output.copy_match(distance as usize, length as usize)?;
            } else {
                return Err(OxiArcError::corrupted(
                    reader.bit_position() / 8,
                    format!("Invalid literal/length code: {}", code),
                ));
            }
        }

        Ok(())
    }

    /// Get the decompressed output.
    pub fn output(&self) -> &[u8] {
        self.output.output()
    }

    /// Take ownership of the decompressed output.
    pub fn into_output(self) -> Vec<u8> {
        self.output.into_output()
    }

    /// Try to decompress one RFC 4978 sync-flushed unit from `input`.
    ///
    /// Processes DEFLATE blocks one at a time until an empty stored block
    /// (sync flush, `LEN=0`/`NLEN=0xFFFF`) is encountered. Detection is done at
    /// the **bit level** (correct) — no byte-pattern scanning (which would give
    /// false positives in Huffman-encoded data).
    ///
    /// # Returns
    ///
    /// - `Ok(Some((bytes, consumed)))` — decompressed bytes and number of bytes
    ///   consumed from `input`. The LZ77 sliding window is advanced.
    /// - `Ok(None)` — more input bytes are needed; the inflater state is **fully
    ///   restored** to what it was before this call (snapshot/restore), so the
    ///   caller can safely retry with a larger buffer.
    /// - `Err(e)` — unrecoverable parse error; discard this inflater.
    pub fn try_decompress_sync_unit(
        &mut self,
        input: &[u8],
    ) -> oxiarc_core::error::Result<Option<(Vec<u8>, usize)>> {
        // Snapshot before any work so we can roll back on partial-delivery EOF.
        let ring_snap = self.output.ring_snapshot();
        let out_len_before = self.output.output_len();

        let cursor = std::io::Cursor::new(input);
        let mut br = oxiarc_core::BitReader::new(cursor);

        loop {
            self.last_empty_stored = false;
            match self.inflate_block(&mut br) {
                Ok(()) => {
                    if self.last_empty_stored {
                        // Sync flush complete — align and report bytes consumed.
                        br.align_to_byte();
                        let bytes_consumed = usize::try_from(br.bits_read())
                            .unwrap_or(usize::MAX)
                            .div_ceil(8);
                        let decompressed = self.output.drain_output();
                        return Ok(Some((decompressed, bytes_consumed)));
                    }
                    // Non-empty block; continue to next block.
                }
                Err(oxiarc_core::error::OxiArcError::Io(ref io_err))
                    if io_err.kind() == std::io::ErrorKind::UnexpectedEof =>
                {
                    // Input exhausted before sync flush — need more data.
                    // Fully restore to pre-call state so the caller can retry.
                    self.output.restore_ring(&ring_snap, out_len_before);
                    self.last_empty_stored = false;
                    return Ok(None);
                }
                Err(oxiarc_core::error::OxiArcError::UnexpectedEof { .. }) => {
                    self.output.restore_ring(&ring_snap, out_len_before);
                    self.last_empty_stored = false;
                    return Ok(None);
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// Decompress one complete sync-flushed chunk (convenience wrapper).
    ///
    /// `input` must be a complete sync-flush unit (all bytes from after the
    /// previous boundary up to and including the sync-flush empty stored block).
    /// Returns the decompressed bytes; LZ77 window is preserved for the next call.
    pub fn decompress_sync_chunk(&mut self, input: &[u8]) -> oxiarc_core::error::Result<Vec<u8>> {
        match self.try_decompress_sync_unit(input)? {
            Some((out, _)) => Ok(out),
            None => Err(oxiarc_core::error::OxiArcError::UnexpectedEof { expected: 1 }),
        }
    }
}

impl Default for Inflater {
    fn default() -> Self {
        Self::new()
    }
}

impl Decompressor for Inflater {
    fn decompress(
        &mut self,
        input: &[u8],
        output: &mut [u8],
    ) -> Result<(usize, usize, DecompressStatus)> {
        // Simple implementation: decompress all at once
        if self.finished {
            return Ok((0, 0, DecompressStatus::Done));
        }

        let mut cursor = std::io::Cursor::new(input);
        let result = self.inflate_reader(&mut cursor)?;

        let consumed = cursor.position() as usize;
        let to_copy = result.len().min(output.len());
        output[..to_copy].copy_from_slice(&result[..to_copy]);

        self.finished = true;

        Ok((consumed, to_copy, DecompressStatus::Done))
    }

    fn reset(&mut self) {
        Inflater::reset(self);
    }

    fn is_finished(&self) -> bool {
        self.finished
    }
}

/// Decompress DEFLATE data.
pub fn inflate(data: &[u8]) -> Result<Vec<u8>> {
    let mut inflater = Inflater::new();
    let mut cursor = std::io::Cursor::new(data);
    inflater.inflate_reader(&mut cursor)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_inflate_stored() {
        // Stored block: BFINAL=1, BTYPE=00, then aligned LEN=5, NLEN=!5, "Hello"
        // Header: 0b00000001 (BFINAL=1, BTYPE=00)
        // LEN: 0x05, 0x00
        // NLEN: 0xFA, 0xFF
        // Data: "Hello"
        let compressed = vec![
            0x01, // BFINAL=1, BTYPE=00, padding
            0x05, 0x00, // LEN=5
            0xFA, 0xFF, // NLEN=65530
            b'H', b'e', b'l', b'l', b'o',
        ];

        let result = inflate(&compressed).expect("inflate of stored block should succeed");
        assert_eq!(result, b"Hello");
    }

    #[test]
    fn test_inflate_empty() {
        // Empty stored block
        let compressed = vec![
            0x01, // BFINAL=1, BTYPE=00
            0x00, 0x00, // LEN=0
            0xFF, 0xFF, // NLEN
        ];

        let result = inflate(&compressed).expect("inflate of empty stored block should succeed");
        assert!(result.is_empty());
    }

    #[test]
    fn test_inflate_consumed_stored() -> Result<()> {
        // A stored block followed by trailing bytes that must remain
        // readable via the same BitReader after inflate completes.
        // Block: BFINAL=1, BTYPE=00, LEN=5, NLEN=!5, "Hello" = 10 bytes.
        let mut data = vec![
            0x01, // BFINAL=1, BTYPE=00, padding
            0x05, 0x00, // LEN=5
            0xFA, 0xFF, // NLEN
            b'H', b'e', b'l', b'l', b'o',
        ];
        data.extend_from_slice(&[0xAA, 0xBB, 0xCC, 0xDD]);

        let cursor = std::io::Cursor::new(&data);
        let mut bit_reader = BitReader::new(cursor);
        let mut inflater = Inflater::new();
        let (decompressed, consumed) = inflater.inflate_consumed(&mut bit_reader)?;
        assert_eq!(decompressed, b"Hello");
        // Stored block wire length: 1 header + 2 LEN + 2 NLEN + 5 data = 10 bytes
        assert_eq!(consumed, 10);

        // The 4 trailer bytes must be readable via the BitReader
        // (they drain from its buffer first, then the underlying cursor).
        let mut trailer = [0u8; 4];
        bit_reader.read_bytes(&mut trailer)?;
        assert_eq!(&trailer, &[0xAA, 0xBB, 0xCC, 0xDD]);
        Ok(())
    }

    // Note: More comprehensive tests would require generating valid
    // compressed data with fixed/dynamic Huffman codes

    #[test]
    fn test_try_decompress_sync_unit_roundtrip() {
        use crate::deflate::Deflater;
        let mut d = Deflater::new(6);
        let mut inflater = Inflater::new();

        let plain1 = b"Hello, IMAP COMPRESS=DEFLATE!";
        let plain2 = b"Second chunk, back-refs possible.";

        let mut comp1 = Vec::new();
        d.deflate_sync(plain1, &mut comp1)
            .expect("deflate_sync of first chunk should succeed");
        let mut comp2 = Vec::new();
        d.deflate_sync(plain2, &mut comp2)
            .expect("deflate_sync of second chunk should succeed");

        let (dec1, consumed1) = inflater
            .try_decompress_sync_unit(&comp1)
            .expect("try_decompress_sync_unit should not error on valid input")
            .expect("complete sync unit should be decompressed");
        assert_eq!(dec1, plain1);
        assert_eq!(consumed1, comp1.len());

        let (dec2, consumed2) = inflater
            .try_decompress_sync_unit(&comp2)
            .expect("try_decompress_sync_unit should not error on valid second chunk")
            .expect("complete second sync unit should be decompressed");
        assert_eq!(dec2, plain2);
        assert_eq!(consumed2, comp2.len());
    }

    #[test]
    fn test_try_decompress_sync_unit_needs_more_data() {
        use crate::deflate::Deflater;
        let mut d = Deflater::new(6);
        let mut inflater = Inflater::new();

        let plain = b"ABCDEFGHIJ";
        let mut comp = Vec::new();
        d.deflate_sync(plain, &mut comp)
            .expect("deflate_sync should succeed");

        // Feed only a partial chunk — should return None (needs more data)
        // and leave inflater state unchanged.
        let partial = &comp[..comp.len() / 2];
        let result = inflater
            .try_decompress_sync_unit(partial)
            .expect("try_decompress_sync_unit should not error on partial valid input");
        assert!(result.is_none(), "expected None for partial chunk");

        // Now feed the full chunk — should succeed.
        let (dec, consumed) = inflater
            .try_decompress_sync_unit(&comp)
            .expect("try_decompress_sync_unit should not error on full valid input")
            .expect("full sync unit should be decompressed");
        assert_eq!(dec, plain);
        assert_eq!(consumed, comp.len());
    }

    #[test]
    fn test_decompress_sync_chunk_lz77_preserved() {
        use crate::deflate::Deflater;
        let mut d = Deflater::new(6);
        let mut inflater = Inflater::new();

        let plain1 = b"ABCDEFGH";
        let plain2 = b"ABCDEFGHABCDEFGH"; // back-references into first chunk

        let mut c1 = Vec::new();
        d.deflate_sync(plain1, &mut c1)
            .expect("deflate_sync of first lz77 chunk should succeed");
        let mut c2 = Vec::new();
        d.deflate_sync(plain2, &mut c2)
            .expect("deflate_sync of second lz77 chunk should succeed");

        assert_eq!(
            inflater
                .decompress_sync_chunk(&c1)
                .expect("decompress_sync_chunk of first chunk should succeed"),
            plain1
        );
        assert_eq!(
            inflater
                .decompress_sync_chunk(&c2)
                .expect("decompress_sync_chunk of second chunk with back-refs should succeed"),
            plain2
        );
    }
}
