//! Streaming bit reader and Huffman tree for **canonical** LZH decompression.
//!
//! These are the resumable, most-significant-bit-first (MSB-first) counterparts
//! of [`oxiarc_core::MsbBitReader`] and [`crate::huffman::LzhHuffmanTree`], the
//! types the (non-streaming) serial decoder now uses. They differ only in that
//! they operate on a **borrowed byte slice** and signal "need more input" by
//! returning [`None`]/`Ok(None)` instead of blocking or zero-padding, so the
//! streaming decoder can pause mid-symbol and resume on a later call.
//!
//! Bit order is MSB-first (the first stream bit is bit 7 of the first byte),
//! matching real `-lh4-`/`-lh5-`/`-lh6-`/`-lh7-` archives — the exact opposite
//! of DEFLATE's LSB-first packing and of this crate's former private LZH
//! format. See [`crate::encode`] for the full bitstream specification.

use crate::methods::constants::NC;
use oxiarc_core::error::{OxiArcError, Result};

/// Maximum representable Huffman code length in canonical LHA.
pub(super) const MAX_CODE_LENGTH: usize = 16;

/// Number of bits in the temp-table code-count field (lhasa `TEMP_CODE_BITS`).
const TEMP_CODE_BITS: u8 = 5;

/// Maximum number of temp-table codes (lhasa `MAX_TEMP_CODES`).
const MAX_TEMP_CODES: usize = (1 << TEMP_CODE_BITS) - 1; // 31

/// Leaf marker set in a tree node value (mirrors lhasa `TREE_NODE_LEAF`).
const TREE_NODE_LEAF: u32 = 1 << 31;

/// Return a mask selecting the low `n` bits (`n` in `0..=64`).
#[inline]
fn low_mask(n: u8) -> u64 {
    if n == 0 {
        0
    } else if n >= 64 {
        u64::MAX
    } else {
        (1u64 << n) - 1
    }
}

// ============================================================================
// Streaming Bit Reader (MSB-first, resumable)
// ============================================================================

/// A resumable, **MSB-first** bit reader over a borrowed byte slice.
///
/// Unlike [`oxiarc_core::MsbBitReader`], which reads from a `Read` and
/// zero-pads past end-of-input, this reader:
///
/// - reads from a `&[u8]` passed to each call, tracking a byte cursor
///   (`input_pos`) that persists across calls (the streaming decoder passes an
///   ever-growing accumulation buffer, so a persisting cursor keeps advancing);
/// - buffers sub-byte bits (`buffer`/`bits_in_buffer`) across calls so a code
///   that straddles a byte boundary is not lost;
/// - returns [`None`] when the requested bits are not yet available instead of
///   erroring, letting the caller retry once more input has arrived;
/// - supports [`save_state`](Self::save_state)/[`restore_state`](Self::restore_state)
///   so a caller can attempt to decode a whole symbol/header transactionally and
///   cheaply rewind the bit cursor (the underlying bytes are retained by the
///   caller) if it turns out more input is needed.
#[derive(Debug, Clone)]
pub struct StreamingBitReader {
    /// Buffered bits, right-aligned: the low `bits_in_buffer` bits are valid and
    /// the most-significant of them is the next bit to be returned (MSB-first).
    buffer: u64,
    /// Number of valid buffered bits in `buffer` (`0..=7` between reads).
    bits_in_buffer: u8,
    /// Byte cursor into the slice passed to the read methods.
    input_pos: usize,
    /// Total number of bits consumed via `read`/`skip` (peeks do not count).
    total_bits_consumed: u64,
}

impl Default for StreamingBitReader {
    fn default() -> Self {
        Self::new()
    }
}

impl StreamingBitReader {
    /// Create a new, empty streaming bit reader.
    pub fn new() -> Self {
        Self {
            buffer: 0,
            bits_in_buffer: 0,
            input_pos: 0,
            total_bits_consumed: 0,
        }
    }

    /// Reset the byte cursor to the start of the next input slice, **keeping**
    /// any buffered sub-byte bits.
    ///
    /// Retained for API compatibility; the canonical streaming decoder feeds a
    /// single growing buffer and does not call this.
    pub fn reset_for_new_input(&mut self) {
        self.input_pos = 0;
    }

    /// Number of bytes touched (pulled) from the current input slice.
    pub fn bytes_consumed(&self) -> usize {
        self.input_pos
    }

    /// Drop the first `trim` bytes from the slice this reader's cursor
    /// tracks, without touching any buffered bits.
    ///
    /// The caller must have already removed the same prefix from the slice
    /// it passes to the read methods (e.g. via `Vec::drain(..trim)`) — this
    /// only rebases the cursor to match. `trim` must not exceed
    /// [`bytes_consumed`](Self::bytes_consumed): bytes at or past the cursor
    /// have not been read yet, so dropping them would corrupt decoding.
    ///
    /// This exists so a caller retaining an ever-growing buffer (the
    /// streaming LZH decoder's `carry`) can periodically compact the
    /// fully-consumed prefix instead of retaining the whole compressed
    /// stream for the life of the decode. The only caller in this crate
    /// derives `trim` from `bytes_consumed()` itself, so the invariant below
    /// is structurally guaranteed rather than merely hoped for.
    pub(crate) fn rebase(&mut self, trim: usize) {
        debug_assert!(
            trim <= self.input_pos,
            "rebase() would drop bytes not yet read"
        );
        self.input_pos = self.input_pos.saturating_sub(trim);
    }

    /// Number of sub-byte bits currently buffered.
    pub fn bits_available(&self) -> u8 {
        self.bits_in_buffer
    }

    /// Total number of bits consumed so far (for error positions / progress).
    pub fn total_bits(&self) -> u64 {
        self.total_bits_consumed
    }

    /// Ensure at least `count` bits are buffered, pulling bytes MSB-first.
    ///
    /// Returns `false` (without erroring) if `input` is exhausted first; any
    /// bytes that *were* pulled remain buffered for the next attempt.
    #[inline]
    fn fill(&mut self, input: &[u8], count: u8) -> bool {
        while self.bits_in_buffer < count {
            if self.input_pos >= input.len() {
                return false;
            }
            self.buffer = (self.buffer << 8) | u64::from(input[self.input_pos]);
            self.bits_in_buffer += 8;
            self.input_pos += 1;
        }
        true
    }

    /// Read `count` bits, most-significant bit first, and advance.
    ///
    /// Returns [`None`] if fewer than `count` bits are available.
    pub fn read_bits(&mut self, input: &[u8], count: u8) -> Option<u32> {
        debug_assert!(count <= 32, "cannot read more than 32 bits at once");
        if count == 0 {
            return Some(0);
        }
        if !self.fill(input, count) {
            return None;
        }
        self.bits_in_buffer -= count;
        let value = ((self.buffer >> self.bits_in_buffer) & low_mask(count)) as u32;
        self.buffer &= low_mask(self.bits_in_buffer);
        self.total_bits_consumed += u64::from(count);
        Some(value)
    }

    /// Peek `count` bits, most-significant bit first, **without** advancing.
    pub fn peek_bits(&mut self, input: &[u8], count: u8) -> Option<u32> {
        debug_assert!(count <= 32, "cannot peek more than 32 bits at once");
        if count == 0 {
            return Some(0);
        }
        if !self.fill(input, count) {
            return None;
        }
        Some(((self.buffer >> (self.bits_in_buffer - count)) & low_mask(count)) as u32)
    }

    /// Discard up to `count` already-buffered bits, most-significant first.
    ///
    /// Only drains bits already in the buffer (does not pull more input); a NOP
    /// if fewer than `count` bits are buffered, matching the previous API.
    pub fn skip_bits(&mut self, count: u8) {
        if count == 0 || self.bits_in_buffer < count {
            return;
        }
        self.bits_in_buffer -= count;
        self.buffer &= low_mask(self.bits_in_buffer);
        self.total_bits_consumed += u64::from(count);
    }

    /// Read a single bit (`true` == 1), most-significant first, and advance.
    pub fn read_bit(&mut self, input: &[u8]) -> Option<bool> {
        self.read_bits(input, 1).map(|b| b != 0)
    }

    /// Save the current reader state for a later [`restore_state`](Self::restore_state).
    pub fn save_state(&self) -> BitReaderState {
        BitReaderState {
            buffer: self.buffer,
            bits_in_buffer: self.bits_in_buffer,
            input_pos: self.input_pos,
            total_bits_consumed: self.total_bits_consumed,
        }
    }

    /// Restore a previously saved state (rewinds the bit cursor).
    pub fn restore_state(&mut self, state: BitReaderState) {
        self.buffer = state.buffer;
        self.bits_in_buffer = state.bits_in_buffer;
        self.input_pos = state.input_pos;
        self.total_bits_consumed = state.total_bits_consumed;
    }
}

/// Saved state of a [`StreamingBitReader`] for transactional rollback.
#[derive(Debug, Clone, Copy)]
pub struct BitReaderState {
    buffer: u64,
    bits_in_buffer: u8,
    input_pos: usize,
    total_bits_consumed: u64,
}

// ============================================================================
// Streaming Huffman Tree (canonical flat-node, MSB-first, resumable)
// ============================================================================

/// A canonical LHA Huffman decode tree stored as a flat binary-tree array,
/// exactly as [`crate::huffman::LzhHuffmanTree`] (lhasa `build_tree`).
///
/// Each slot is either a **leaf** (`TREE_NODE_LEAF | symbol`) or an **internal
/// node** holding the array index of its `0`-child (the `1`-child is the next
/// index). Decoding walks from the root reading one MSB-first bit at a time.
/// A table announced with count `n == 0` is the degenerate "single code" case
/// ([`single`](Self::single)): every decode returns that one symbol while
/// consuming **zero** bits — the fix for the desync bug the previous
/// (lookup-table) streaming tree had, which always read one bit per symbol.
#[derive(Debug, Clone)]
pub struct StreamingHuffmanTree {
    /// Flat tree array; slot 0 is the root.
    nodes: Vec<u32>,
    /// If `Some`, this table decodes to a single symbol consuming zero bits.
    single: Option<u16>,
    /// Longest code length (0 for the single-code / empty cases).
    max_length: u8,
}

impl StreamingHuffmanTree {
    /// Construct the degenerate single-code table (lhasa `set_tree_single`):
    /// every [`decode`](Self::decode) returns `symbol` and reads no bits.
    pub fn single(symbol: u16) -> Self {
        Self {
            nodes: vec![symbol as u32 | TREE_NODE_LEAF],
            single: Some(symbol),
            max_length: 0,
        }
    }

    /// Build a canonical decode tree from per-symbol code lengths.
    ///
    /// `tree_capacity` bounds the flat array (lhasa sizes it `num_symbols * 2`).
    /// This mirrors [`crate::huffman::LzhHuffmanTree::from_code_lengths`]
    /// (lhasa's `build_tree`): symbols are placed shortest-length-first, and
    /// within a length in ascending symbol order, yielding standard canonical
    /// codes.
    pub fn from_code_lengths(code_lengths: &[u8], tree_capacity: usize) -> Result<Self> {
        let num = code_lengths.len();
        let max_length = code_lengths.iter().copied().max().unwrap_or(0);
        if max_length as usize > MAX_CODE_LENGTH {
            return Err(OxiArcError::invalid_huffman(0));
        }

        // All slots start as leaves (symbol 0), matching lhasa `init_tree`.
        let mut nodes = vec![TREE_NODE_LEAF; tree_capacity.max(1)];

        let mut next_entry: usize = 0;
        let mut tree_allocated: usize = 1;
        let mut code_len: u8 = 0;

        loop {
            // expand_queue: give every queued node two children.
            let new_nodes = (tree_allocated - next_entry) * 2;
            if tree_allocated + new_nodes <= nodes.len() {
                let end_offset = tree_allocated;
                while next_entry < end_offset {
                    nodes[next_entry] = tree_allocated as u32;
                    tree_allocated += 2;
                    next_entry += 1;
                }
            }

            code_len += 1;

            // add_codes_with_length: attach every symbol whose length matches.
            let mut codes_remaining = false;
            for (symbol, &len) in code_lengths.iter().enumerate().take(num) {
                if len == code_len {
                    if next_entry < tree_allocated {
                        let node = next_entry;
                        next_entry += 1;
                        nodes[node] = symbol as u32 | TREE_NODE_LEAF;
                    }
                } else if len > code_len {
                    codes_remaining = true;
                }
            }

            if !codes_remaining || code_len >= MAX_CODE_LENGTH as u8 {
                break;
            }
        }

        Ok(Self {
            nodes,
            single: None,
            max_length,
        })
    }

    /// Build a tree from code lengths, sizing the flat array from the alphabet.
    ///
    /// Retained for source-compatibility with the previous streaming tree's
    /// signature; the second argument is advisory (the canonical pointer tree
    /// has no lookup-table width) and is ignored.
    pub fn from_lengths(lengths: &[u8], _table_bits: u8) -> Result<Self> {
        Self::from_code_lengths(lengths, lengths.len().max(1) * 2)
    }

    /// The longest code length in this table (0 for the single-code / empty
    /// degenerate cases).
    pub fn max_length(&self) -> u8 {
        self.max_length
    }

    /// Decode one symbol, MSB-first (lhasa `read_from_tree`), resumably.
    ///
    /// * `Ok(Some(sym))` — a symbol was decoded.
    /// * `Ok(None)` — a bit was not yet available; the caller should restore its
    ///   saved reader state and retry with more input.
    /// * `Err(..)` — the table is malformed (out-of-range child / over-long
    ///   code): genuine corruption, distinct from "need more input".
    pub fn decode(&self, reader: &mut StreamingBitReader, input: &[u8]) -> Result<Option<u16>> {
        if let Some(sym) = self.single {
            return Ok(Some(sym));
        }

        let mut code = self.nodes[0];
        let mut steps = 0u32;
        while code & TREE_NODE_LEAF == 0 {
            let bit = match reader.read_bit(input) {
                Some(b) => usize::from(b),
                None => return Ok(None),
            };
            let idx = code as usize + bit;
            if idx >= self.nodes.len() {
                return Err(OxiArcError::invalid_huffman(reader.total_bits()));
            }
            code = self.nodes[idx];
            steps += 1;
            if steps > MAX_CODE_LENGTH as u32 {
                // Guards against a malformed (cyclic) table.
                return Err(OxiArcError::invalid_huffman(reader.total_bits()));
            }
        }
        Ok(Some((code & !TREE_NODE_LEAF) as u16))
    }
}

// ============================================================================
// Per-block table readers (resumable mirrors of `crate::huffman`)
// ============================================================================

/// Read a length value: 3 bits, extended by a unary run of 1-bits terminated by
/// a 0-bit when the base value is 7 (lhasa `read_length_value`).
///
/// Returns `Ok(None)` if input is exhausted mid-value.
fn read_length_value(reader: &mut StreamingBitReader, input: &[u8]) -> Result<Option<u8>> {
    let mut len = match reader.read_bits(input, 3) {
        Some(v) => v as u8,
        None => return Ok(None),
    };
    if len == 7 {
        loop {
            match reader.read_bit(input) {
                Some(true) => {
                    len += 1;
                    if len >= 32 {
                        // A conformant stream never approaches this; bail
                        // defensively rather than loop forever.
                        break;
                    }
                }
                Some(false) => break,
                None => return Ok(None),
            }
        }
    }
    Ok(Some(len))
}

/// Read the skip/zero-run count encoded by temp-table value `v` in the C-tree
/// length list (lhasa `read_skip_count`): `0 -> 1`, `1 -> 4-bit + 3`,
/// `2 -> 9-bit + 20`.
fn read_skip_count(reader: &mut StreamingBitReader, input: &[u8], v: u16) -> Result<Option<usize>> {
    Ok(Some(match v {
        0 => 1,
        1 => match reader.read_bits(input, 4) {
            Some(x) => x as usize + 3,
            None => return Ok(None),
        },
        _ => match reader.read_bits(input, 9) {
            Some(x) => x as usize + 20,
            None => return Ok(None),
        },
    }))
}

/// Read the temp table (PT-tree) that in turn encodes the C-tree code lengths
/// (lhasa `read_temp_table`). Resumable mirror of `crate::huffman::read_temp_tree`.
pub(super) fn read_temp_tree(
    reader: &mut StreamingBitReader,
    input: &[u8],
) -> Result<Option<StreamingHuffmanTree>> {
    let n = match reader.read_bits(input, TEMP_CODE_BITS) {
        Some(v) => v as usize,
        None => return Ok(None),
    };

    if n == 0 {
        let code = match reader.read_bits(input, 5) {
            Some(v) => v as u16,
            None => return Ok(None),
        };
        return Ok(Some(StreamingHuffmanTree::single(code)));
    }

    let n = n.min(MAX_TEMP_CODES);
    let mut lengths = [0u8; MAX_TEMP_CODES];

    let mut i = 0usize;
    while i < n {
        lengths[i] = match read_length_value(reader, input)? {
            Some(v) => v,
            None => return Ok(None),
        };

        // After the length of symbol index 2, a 2-bit field says how many of the
        // next symbols (3, 4, 5) are skipped (length 0). This is the temp
        // table's own mechanism — distinct from the C-tree's zero-run coding.
        if i == 2 {
            let skip = match reader.read_bits(input, 2) {
                Some(v) => v as usize,
                None => return Ok(None),
            };
            for _ in 0..skip {
                i += 1;
                if i < lengths.len() {
                    lengths[i] = 0;
                }
            }
        }

        i += 1;
    }

    Ok(Some(StreamingHuffmanTree::from_code_lengths(
        &lengths[..n],
        MAX_TEMP_CODES * 2,
    )?))
}

/// Read the C-tree (character/length codes), whose lengths are Huffman-encoded
/// using `temp_tree` (lhasa `read_code_table`).
pub(super) fn read_code_tree(
    reader: &mut StreamingBitReader,
    input: &[u8],
    temp_tree: &StreamingHuffmanTree,
) -> Result<Option<StreamingHuffmanTree>> {
    let n = match reader.read_bits(input, 9) {
        Some(v) => v as usize,
        None => return Ok(None),
    };

    if n == 0 {
        let code = match reader.read_bits(input, 9) {
            Some(v) => v as u16,
            None => return Ok(None),
        };
        return Ok(Some(StreamingHuffmanTree::single(code)));
    }

    let n = n.min(NC);
    let mut lengths = vec![0u8; NC];

    let mut i = 0usize;
    while i < n {
        let v = match temp_tree.decode(reader, input)? {
            Some(v) => v,
            None => return Ok(None),
        };
        if v <= 2 {
            let skip = match read_skip_count(reader, input, v)? {
                Some(s) => s,
                None => return Ok(None),
            };
            for _ in 0..skip {
                if i >= n {
                    break;
                }
                lengths[i] = 0;
                i += 1;
            }
        } else {
            // Temp value v (>= 3) denotes C-tree code length v - 2.
            lengths[i] = (v - 2) as u8;
            i += 1;
        }
    }

    Ok(Some(StreamingHuffmanTree::from_code_lengths(
        &lengths[..n],
        NC * 2,
    )?))
}

/// Read the offset/position tree (lhasa `read_offset_table`).
///
/// `offset_bits` is the method's count-field width and `max_codes` is
/// `(1 << offset_bits) - 1`.
pub(super) fn read_offset_tree(
    reader: &mut StreamingBitReader,
    input: &[u8],
    offset_bits: u8,
    max_codes: usize,
) -> Result<Option<StreamingHuffmanTree>> {
    let n = match reader.read_bits(input, offset_bits) {
        Some(v) => v as usize,
        None => return Ok(None),
    };

    if n == 0 {
        let code = match reader.read_bits(input, offset_bits) {
            Some(v) => v as u16,
            None => return Ok(None),
        };
        return Ok(Some(StreamingHuffmanTree::single(code)));
    }

    let n = n.min(max_codes);
    let mut lengths = vec![0u8; max_codes.max(1)];
    for length in lengths.iter_mut().take(n) {
        *length = match read_length_value(reader, input)? {
            Some(v) => v,
            None => return Ok(None),
        };
    }

    Ok(Some(StreamingHuffmanTree::from_code_lengths(
        &lengths[..n],
        max_codes.max(1) * 2,
    )?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn msb_first_read_bits() {
        // 0xB0 == 1011_0000: first 3 bits == 101, next 1 == 1, next 4 == 0000.
        let data = [0xB0u8, 0x00];
        let mut r = StreamingBitReader::new();
        assert_eq!(r.read_bits(&data, 3), Some(0b101));
        assert_eq!(r.read_bits(&data, 1), Some(0b1));
        assert_eq!(r.read_bits(&data, 4), Some(0b0000));
    }

    #[test]
    fn msb_first_sixteen_is_big_endian() {
        let data = [0x12u8, 0x34];
        let mut r = StreamingBitReader::new();
        assert_eq!(r.read_bits(&data, 16), Some(0x1234));
        assert_eq!(r.bytes_consumed(), 2);
    }

    #[test]
    fn single_tree_consumes_no_bits() {
        let tree = StreamingHuffmanTree::single(42);
        let data = [0u8; 4];
        let mut r = StreamingBitReader::new();
        assert_eq!(tree.decode(&mut r, &data).expect("decode"), Some(42));
        assert_eq!(r.total_bits(), 0, "single code must read zero bits");
    }

    #[test]
    fn canonical_tree_roundtrips_msb_first() {
        // Symbols 0..=3 with lengths [2,1,3,3]: canonical codes are
        // 1 -> 0, 0 -> 10, 2 -> 110, 3 -> 111.  Encode "1 0 2 3" MSB-first:
        // 0 10 110 111 == 0101_1011_1... -> bytes 0x5B, 0x80.
        let lengths = [2u8, 1, 3, 3];
        let tree = StreamingHuffmanTree::from_code_lengths(&lengths, lengths.len() * 2).expect("t");
        let data = [0b0101_1011u8, 0b1000_0000];
        let mut r = StreamingBitReader::new();
        for sym in [1u16, 0, 2, 3] {
            assert_eq!(tree.decode(&mut r, &data).expect("decode"), Some(sym));
        }
    }

    #[test]
    fn decode_needs_more_input_returns_none() {
        // A 3-bit code alphabet but zero input bytes -> Ok(None) (need input).
        let lengths = [2u8, 1, 3, 3];
        let tree = StreamingHuffmanTree::from_code_lengths(&lengths, lengths.len() * 2).expect("t");
        let empty: [u8; 0] = [];
        let mut r = StreamingBitReader::new();
        assert_eq!(tree.decode(&mut r, &empty).expect("decode"), None);
    }
}
