//! Brotli decompression implementation.
//!
//! Implements the Brotli decompression algorithm per RFC 7932, following
//! the decoding algorithm of Section 10 exactly.
//!
//! ## Stream Structure
//!
//! A Brotli stream consists of:
//! 1. The stream header (WBITS, Section 9.1)
//! 2. A sequence of meta-blocks, the last one marked with ISLAST=1
//!
//! Each meta-block can be:
//! - Empty last (ISLASTEMPTY=1)
//! - Metadata (MNIBBLES=0; skipped, produces no output)
//! - Uncompressed (raw bytes on a byte boundary; only when ISLAST=0)
//! - Compressed (LZ77 + prefix-coded commands, Sections 9.2/9.3)
//!
//! The decoder is strict: non-zero padding, trailing garbage, truncated
//! streams, incomplete prefix codes, invalid distances, and length overruns
//! are all rejected with an error. It never returns `Ok` with wrong bytes.

use oxiarc_core::cancel::CancellationToken;
use oxiarc_core::progress::ProgressHandle;

use crate::bit_reader::BitReader;
use crate::context::{
    ContextMap, ContextMode, NUM_DISTANCE_CONTEXTS, NUM_LITERAL_CONTEXTS, distance_context_id,
    literal_context_id,
};
use crate::dictionary;
use crate::error::{BrotliError, BrotliResult};
use crate::huffman::{HuffmanTree, read_prefix_code};
use crate::shared_dict;
use crate::tables::{BLOCK_COUNT_CODES, COPY_LENGTH_CODES, INSERT_LENGTH_CODES, decompose_command};

/// Maximum allowed output size (256 MB limit for safety against
/// decompression bombs; a documented guard, not an RFC limit).
pub(crate) const MAX_OUTPUT_SIZE: usize = 256 * 1024 * 1024;

/// The output-size cap enforced while a stream is being decoded.
///
/// Brotli declares no total uncompressed size anywhere in the stream, so the
/// only way to bound the output is to enforce a cap as the output grows.
/// Every meta-block, however, *does* declare its own exact length (MLEN,
/// Section 9.2), and a meta-block produces exactly MLEN bytes — so checking
/// `produced + MLEN` before decoding a meta-block rejects an over-budget
/// stream *before* a single byte of the offending block is decoded.
#[derive(Clone, Copy)]
struct OutputLimit {
    /// Maximum total output, in bytes.
    max: usize,
    /// Whether `max` is a caller-supplied memory budget (the bounded entry
    /// points) rather than the crate's own anti-bomb guard. This only
    /// selects the error variant, so the unbounded entry points keep their
    /// established [`BrotliError::OutputTooLarge`] semantics.
    caller_budget: bool,
}

impl OutputLimit {
    /// The crate's built-in 256 MB guard, used by the unbounded entry points.
    const fn default_guard() -> Self {
        Self {
            max: MAX_OUTPUT_SIZE,
            caller_budget: false,
        }
    }

    /// A caller-supplied memory budget (see [`decompress_with_limit`]).
    const fn budget(max: usize) -> Self {
        Self {
            max,
            caller_budget: true,
        }
    }

    /// Reject a stream that would produce `needed` total output bytes.
    fn check(&self, needed: usize) -> BrotliResult<()> {
        if needed > self.max {
            return Err(if self.caller_budget {
                BrotliError::MemoryBudgetExceeded {
                    budget: self.max,
                    requested: needed,
                }
            } else {
                BrotliError::OutputTooLarge(needed)
            });
        }
        Ok(())
    }
}

/// Decompress a Brotli-compressed byte slice.
///
/// The input must be exactly one complete Brotli stream: trailing bytes
/// after the last meta-block (or non-zero padding bits) are rejected as
/// corruption.
///
/// The output is capped at the crate's built-in 256 MB anti-bomb guard
/// ([`BrotliError::OutputTooLarge`]). For untrusted input, prefer
/// [`decompress_with_limit`], which enforces a caller-chosen budget.
pub fn decompress(data: &[u8]) -> BrotliResult<Vec<u8>> {
    decompress_with_hooks(data, None, None, None)
}

/// Decompress a Brotli stream, refusing to produce more than `max_output`
/// bytes — the decompression-bomb guard for untrusted input.
///
/// Returns [`BrotliError::MemoryBudgetExceeded`] as soon as the stream
/// declares that it will exceed the budget: the check runs per meta-block,
/// *before* the offending meta-block is decoded (a meta-block produces
/// exactly its declared MLEN bytes, so the projection is exact and there are
/// no false positives). An over-budget bomb is therefore rejected without
/// its expansion ever being allocated.
///
/// # Memory characteristics
///
/// Peak output allocation is bounded by `max_output` plus, at worst, one
/// transformed static-dictionary word (< 64 bytes) that a corrupt stream may
/// append before the meta-block length check rejects it. Vector growth
/// doubling applies on top of that as usual.
///
/// # Example
///
/// ```rust
/// use oxiarc_brotli::{compress, decompress_with_limit, BrotliError};
///
/// let data = vec![0u8; 1_000_000];
/// let compressed = compress(&data, 6).expect("compress");
///
/// // Generous budget: decodes normally.
/// let ok = decompress_with_limit(&compressed, 4 << 20).expect("decompress");
/// assert_eq!(ok, data);
///
/// // Tight budget: rejected during decoding, nothing is materialised.
/// assert!(matches!(
///     decompress_with_limit(&compressed, 64 * 1024),
///     Err(BrotliError::MemoryBudgetExceeded { .. })
/// ));
/// ```
pub fn decompress_with_limit(data: &[u8], max_output: usize) -> BrotliResult<Vec<u8>> {
    decompress_with_hooks(data, None, None, Some(max_output))
}

/// Decompress a Brotli stream whose backward references may reach into a
/// *shared* (custom LZ77) dictionary.
///
/// This is the decoding half of the reference `brotli --dictionary=FILE`
/// option and of `Content-Encoding: dcb` bodies (RFC 9842): `dictionary` is
/// content both peers already have, and the stream may reference it at
/// distances beyond everything it has itself produced — beyond its declared
/// window, in fact. See [`crate::shared_dict`] for the exact distance space,
/// which was established by measurement against `brotli 1.1.0`.
///
/// Passing an empty `dictionary` is identical to [`decompress`].
///
/// # Errors
///
/// The same errors as [`decompress`], plus
/// [`BrotliError::DictionaryError`] when `dictionary` exceeds
/// [`crate::shared_dict::MAX_SHARED_DICTIONARY`].
///
/// # Example
///
/// ```rust
/// use oxiarc_brotli::{compress_with_dictionary, decompress_with_dictionary, BrotliParams};
///
/// let dictionary = b"the quick brown fox jumps over the lazy dog".repeat(64);
/// let message = b"the quick brown fox jumps over the lazy dog, again";
///
/// let params = BrotliParams { quality: 9, ..BrotliParams::default() };
/// let compressed = compress_with_dictionary(message, &dictionary, &params).expect("compress");
/// let decoded = decompress_with_dictionary(&compressed, &dictionary).expect("decompress");
/// assert_eq!(decoded, message);
///
/// // The dictionary is not optional: the same bytes decode to something else
/// // (or not at all) without it.
/// assert_ne!(oxiarc_brotli::decompress(&compressed).ok(), Some(message.to_vec()));
/// ```
pub fn decompress_with_dictionary(data: &[u8], dictionary: &[u8]) -> BrotliResult<Vec<u8>> {
    shared_dict::check_dictionary_len(dictionary.len())?;
    decompress_instrumented(data, None, None, None, None, dictionary)
}

/// [`decompress_with_dictionary`] with the caller-chosen output budget of
/// [`decompress_with_limit`].
///
/// # Errors
///
/// The union of [`decompress_with_dictionary`]'s and
/// [`decompress_with_limit`]'s errors.
pub fn decompress_with_dictionary_and_limit(
    data: &[u8],
    dictionary: &[u8],
    max_output: usize,
) -> BrotliResult<Vec<u8>> {
    shared_dict::check_dictionary_len(dictionary.len())?;
    decompress_instrumented(data, None, None, Some(max_output), None, dictionary)
}

/// Decompress with optional per-meta-block progress and cancellation hooks
/// and an optional caller-supplied output budget.
///
/// Called by [`decompress`] / [`decompress_with_limit`] and by streaming
/// types that carry a [`ProgressHandle`], a [`CancellationToken`] or a
/// `max_output` setting. `max_output` of `None` means the crate's default
/// 256 MB guard.
pub(crate) fn decompress_with_hooks(
    data: &[u8],
    progress: Option<&ProgressHandle>,
    cancel: Option<&CancellationToken>,
    max_output: Option<usize>,
) -> BrotliResult<Vec<u8>> {
    decompress_instrumented(data, progress, cancel, max_output, None, &[])
}

/// The block-splitting and context-modeling shape of one decoded meta-block.
///
/// This is what the decoder *actually parsed*, recorded as a side effect of a
/// normal decode. Its purpose is to let tests assert that an encoder change
/// really reached the wire — "the stream round-trips" is true of a stream that
/// silently declined to split, so without this a feature test can pass
/// vacuously.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct MetaBlockShape {
    /// `NBLTYPESL`: literal block types.
    pub literal_types: u32,
    /// `NBLTYPESI`: insert-and-copy block types.
    pub insert_and_copy_types: u32,
    /// `NBLTYPESD`: distance block types.
    pub distance_types: u32,
    /// `NTREESL`: distinct literal prefix codes.
    pub literal_trees: u32,
    /// `NTREESD`: distinct distance prefix codes.
    pub distance_trees: u32,
}

/// Decompress `data`, additionally reporting the shape of every compressed
/// meta-block it contained.
///
/// Intended for tests and diagnostics; the returned bytes are exactly what
/// [`decompress`] produces.
///
/// # Errors
///
/// The same errors as [`decompress`].
pub fn decompress_reporting_shapes(data: &[u8]) -> BrotliResult<(Vec<u8>, Vec<MetaBlockShape>)> {
    let mut shapes = Vec::new();
    let output = decompress_instrumented(data, None, None, None, Some(&mut shapes), &[])?;
    Ok((output, shapes))
}

/// The shared decode driver. `shapes`, when present, collects one entry per
/// compressed meta-block.
fn decompress_instrumented(
    data: &[u8],
    progress: Option<&ProgressHandle>,
    cancel: Option<&CancellationToken>,
    max_output: Option<usize>,
    mut shapes: Option<&mut Vec<MetaBlockShape>>,
    shared: &[u8],
) -> BrotliResult<Vec<u8>> {
    if data.is_empty() {
        return Err(BrotliError::UnexpectedEof);
    }

    let limit = match max_output {
        Some(max) => OutputLimit::budget(max),
        None => OutputLimit::default_guard(),
    };

    let mut reader = BitReader::new(data);
    let mut output = Vec::new();

    // Stream header: WBITS. Window size is (1 << WBITS) - 16 (Section 9.1).
    let wbits = read_window_bits(&mut reader)?;
    let window_size = (1usize << wbits) - 16;

    // Ring buffer of the last four distances (Section 4): the last distance
    // is 4, then 11, 15, 16. Persists across meta-blocks.
    let mut state = DecoderState::new(window_size);

    loop {
        if let Some(token) = cancel {
            token.check().map_err(BrotliError::from)?;
        }

        let is_last = reader.read_bit()?;
        if is_last {
            let is_empty = reader.read_bit()?;
            if is_empty {
                break;
            }
        }

        // MNIBBLES (Section 9.2): 3 means a metadata meta-block.
        let mnibbles_code = reader.read_bits(2)?;
        if mnibbles_code == 3 {
            skip_metadata_block(&mut reader)?;
            if is_last {
                break;
            }
            continue;
        }

        let mnibbles = mnibbles_code + 4;
        let mlen_minus_1 = reader.read_bits(mnibbles * 4)?;
        // Reject non-minimal length encodings: with 5 or 6 nibbles the most
        // significant nibble must be non-zero.
        if mnibbles > 4 && (mlen_minus_1 >> ((mnibbles - 1) * 4)) == 0 {
            return Err(BrotliError::CorruptedData(
                "non-minimal MLEN encoding".to_string(),
            ));
        }
        let mlen = mlen_minus_1 as usize + 1;

        // A meta-block emits exactly MLEN bytes, so this rejects an
        // over-budget stream *before* the offending meta-block is decoded:
        // no part of a bomb's expansion is ever allocated.
        limit.check(output.len().saturating_add(mlen))?;

        // ISUNCOMPRESSED is only present when ISLAST is 0.
        if !is_last {
            let is_uncompressed = reader.read_bit()?;
            if is_uncompressed {
                if reader.align_to_byte()? != 0 {
                    return Err(BrotliError::CorruptedData(
                        "non-zero padding before uncompressed data".to_string(),
                    ));
                }
                reader.read_bytes_aligned(&mut output, mlen)?;
                if let Some(handle) = progress {
                    handle.on_progress(output.len() as u64, None);
                }
                continue;
            }
        }

        let shape =
            decode_compressed_meta_block(&mut reader, &mut output, mlen, &mut state, shared)?;
        if let Some(ref mut collected) = shapes {
            collected.push(shape);
        }

        if let Some(handle) = progress {
            handle.on_progress(output.len() as u64, None);
        }

        if is_last {
            break;
        }
    }

    // Section 10: the stream ends at the last meta-block; remaining bits in
    // the final byte must be zero, and no further input may follow.
    if reader.align_to_byte()? != 0 {
        return Err(BrotliError::CorruptedData(
            "non-zero padding after last meta-block".to_string(),
        ));
    }
    if reader.has_more() {
        return Err(BrotliError::CorruptedData(
            "trailing data after last meta-block".to_string(),
        ));
    }

    Ok(output)
}

/// Read the stream header WBITS field (RFC 7932 Section 9.1).
pub(crate) fn read_window_bits(reader: &mut BitReader<'_>) -> BrotliResult<u32> {
    if !reader.read_bit()? {
        return Ok(16);
    }
    let n = reader.read_bits(3)?;
    if n != 0 {
        return Ok(17 + n); // 18..=24
    }
    let m = reader.read_bits(3)?;
    match m {
        0 => Ok(17),
        1 => Err(BrotliError::InvalidWindowSize(9)), // reserved pattern
        _ => Ok(8 + m),                              // 10..=15
    }
}

/// Skip a metadata meta-block body (MNIBBLES == 0, RFC 7932 Section 9.2).
fn skip_metadata_block(reader: &mut BitReader<'_>) -> BrotliResult<()> {
    // Reserved bit must be zero.
    if reader.read_bit()? {
        return Err(BrotliError::CorruptedData(
            "reserved bit set in metadata block".to_string(),
        ));
    }
    let mskipbytes = reader.read_bits(2)?;
    let mskiplen = if mskipbytes == 0 {
        0usize
    } else {
        let mut value = 0u32;
        for i in 0..mskipbytes {
            let byte = reader.read_bits(8)?;
            // Non-minimal encodings are invalid: the last byte must be
            // non-zero when more than one byte is used.
            if i + 1 == mskipbytes && mskipbytes > 1 && byte == 0 {
                return Err(BrotliError::CorruptedData(
                    "non-minimal MSKIPLEN encoding".to_string(),
                ));
            }
            value |= byte << (i * 8);
        }
        value as usize + 1
    };
    if reader.align_to_byte()? != 0 {
        return Err(BrotliError::CorruptedData(
            "non-zero padding in metadata block".to_string(),
        ));
    }
    reader.skip_bytes_aligned(mskiplen)?;
    Ok(())
}

/// Read an NBLTYPES / NTREES count with the Section 9.2 variable-length
/// code (result in 1..=256).
pub(crate) fn read_block_type_count(reader: &mut BitReader<'_>) -> BrotliResult<u32> {
    if !reader.read_bit()? {
        return Ok(1);
    }
    let n = reader.read_bits(3)?;
    if n == 0 {
        return Ok(2);
    }
    let extra = reader.read_bits(n)?;
    Ok((1 << n) + 1 + extra)
}

/// Read a block count using the 26-symbol block-count code (Section 6).
pub(crate) fn read_block_count(
    reader: &mut BitReader<'_>,
    tree: &HuffmanTree,
) -> BrotliResult<u32> {
    let sym = tree.decode_symbol(reader)?;
    let (base, extra_bits) = *BLOCK_COUNT_CODES
        .get(sym as usize)
        .ok_or_else(|| BrotliError::CorruptedData(format!("invalid block count code {sym}")))?;
    Ok(base + reader.read_bits(extra_bits as u32)?)
}

/// Per-category block-switching state (Section 6).
pub(crate) struct BlockCategory {
    /// Number of block types (NBLTYPESx).
    pub(crate) num_types: u32,
    /// Current block type.
    pub(crate) btype: usize,
    /// Block type of the block that preceded the current one.
    pub(crate) prev_btype: usize,
    /// Remaining element count for the current block.
    pub(crate) blen: u32,
    /// Prefix code over the block type alphabet (present when >= 2 types).
    btype_tree: Option<HuffmanTree>,
    /// Prefix code over the block count alphabet (present when >= 2 types).
    blen_tree: Option<HuffmanTree>,
}

impl BlockCategory {
    /// Read the NBLTYPES header field and, when >= 2, the block type and
    /// block count prefix codes plus the first block count (Section 9.2).
    pub(crate) fn read(reader: &mut BitReader<'_>) -> BrotliResult<Self> {
        let num_types = read_block_type_count(reader)?;
        if num_types >= 2 {
            let btype_tree = read_prefix_code(reader, num_types + 2)?;
            let blen_tree = read_prefix_code(reader, 26)?;
            let blen = read_block_count(reader, &blen_tree)?;
            Ok(BlockCategory {
                num_types,
                btype: 0,
                prev_btype: 1,
                blen,
                btype_tree: Some(btype_tree),
                blen_tree: Some(blen_tree),
            })
        } else {
            Ok(BlockCategory {
                num_types: 1,
                btype: 0,
                prev_btype: 1,
                blen: u32::MAX,
                btype_tree: None,
                blen_tree: None,
            })
        }
    }

    /// Consume one element of this category, performing a block switch
    /// first when the current block is exhausted (Section 6).
    ///
    /// The overwhelmingly common outcome — one category, or a block that still
    /// has elements left — is a compare and a decrement, so it is inlined into
    /// both command loops; the switch itself is out of line. `tick` runs three
    /// times per command and was 5.6 % of the push decoder's profile purely as
    /// a call.
    #[inline(always)]
    pub(crate) fn tick(&mut self, reader: &mut BitReader<'_>) -> BrotliResult<()> {
        if self.num_types < 2 {
            return Ok(());
        }
        if self.blen > 0 {
            self.blen -= 1;
            return Ok(());
        }
        self.switch_block(reader)
    }

    /// The block switch of [`BlockCategory::tick`] (Section 6): decode the new
    /// block type and its count, then consume one element of the new block.
    #[inline(never)]
    fn switch_block(&mut self, reader: &mut BitReader<'_>) -> BrotliResult<()> {
        let (Some(btype_tree), Some(blen_tree)) = (&self.btype_tree, &self.blen_tree) else {
            return Err(BrotliError::CorruptedData(
                "missing block switch codes".to_string(),
            ));
        };
        let sym = btype_tree.decode_symbol(reader)?;
        let new_type = match sym {
            0 => self.prev_btype,
            1 => (self.btype + 1) % self.num_types as usize,
            _ => (sym - 2) as usize,
        };
        self.prev_btype = self.btype;
        self.btype = new_type;
        self.blen = read_block_count(reader, blen_tree)?;
        // The first block count from the header can legitimately be zero only
        // through a switch above, so blen > 0 is guaranteed here for valid
        // streams; treat 0 defensively.
        if self.blen == 0 {
            return Err(BrotliError::CorruptedData("zero block count".to_string()));
        }
        self.blen -= 1;
        Ok(())
    }
}

/// Distance ring buffer and window state shared across meta-blocks.
#[derive(Debug, Clone, Copy)]
pub(crate) struct DecoderState {
    /// The last four distances, most recent at `dist_ring_idx - 1`.
    pub(crate) dist_ring: [usize; 4],
    /// Write cursor into `dist_ring` (masked with 3).
    pub(crate) dist_ring_idx: usize,
    /// `(1 << WBITS) - 16`, the largest in-window backward reference.
    pub(crate) window_size: usize,
}

impl DecoderState {
    /// The initial ring state of RFC 7932 Section 4: last = 4, then 11, 15, 16.
    pub(crate) const fn new(window_size: usize) -> Self {
        DecoderState {
            dist_ring: [16, 15, 11, 4],
            dist_ring_idx: 0,
            window_size,
        }
    }

    pub(crate) fn last_distance(&self) -> usize {
        self.dist_ring[(self.dist_ring_idx.wrapping_sub(1)) & 3]
    }

    fn nth_last_distance(&self, n: usize) -> usize {
        self.dist_ring[(self.dist_ring_idx.wrapping_sub(n)) & 3]
    }

    pub(crate) fn push_distance(&mut self, distance: usize) {
        self.dist_ring[self.dist_ring_idx & 3] = distance;
        self.dist_ring_idx = self.dist_ring_idx.wrapping_add(1);
    }
}

/// Read a literal or distance context map (RFC 7932 Section 7.3).
pub(crate) fn read_context_map(
    reader: &mut BitReader<'_>,
    num_trees: u32,
    size: usize,
) -> BrotliResult<Vec<u8>> {
    // RLEMAX: one 0 bit, or four bits + 1 after a 1 bit.
    let rlemax = if reader.read_bit()? {
        reader.read_bits(4)? + 1
    } else {
        0
    };
    let cm_tree = read_prefix_code(reader, num_trees + rlemax)?;

    let mut map = Vec::with_capacity(size);
    while map.len() < size {
        let sym = cm_tree.decode_symbol(reader)? as u32;
        if sym == 0 {
            map.push(0);
        } else if sym <= rlemax {
            let extra = reader.read_bits(sym)?;
            let run = (1usize << sym) + extra as usize;
            if map.len() + run > size {
                return Err(BrotliError::InvalidContextMap(
                    "zero run exceeds context map size".to_string(),
                ));
            }
            map.resize(map.len() + run, 0);
        } else {
            map.push((sym - rlemax) as u8);
        }
    }

    if reader.read_bit()? {
        inverse_move_to_front(&mut map);
    }
    Ok(map)
}

/// Inverse move-to-front transform (RFC 7932 Section 7.3).
fn inverse_move_to_front(data: &mut [u8]) {
    let mut mtf = [0u8; 256];
    for (i, val) in mtf.iter_mut().enumerate() {
        *val = i as u8;
    }
    for v in data.iter_mut() {
        let idx = *v as usize;
        let value = mtf[idx];
        *v = value;
        for j in (1..=idx).rev() {
            mtf[j] = mtf[j - 1];
        }
        mtf[0] = value;
    }
}

/// Everything a compressed meta-block's header declares (RFC 7932 Section
/// 9.2), parsed as one unit.
///
/// Both decoders share this parser: the one-shot [`decompress`] path and the
/// incremental [`crate::stream::BrotliStream`], whose header tier re-parses
/// from a rolled-back bit cursor until the whole header has arrived. Sharing
/// it is what makes the two decoders read the header at bit-identical
/// positions.
pub(crate) struct MetaBlockHeader {
    /// Literal block-switch state.
    pub(crate) cat_l: BlockCategory,
    /// Insert-and-copy block-switch state.
    pub(crate) cat_i: BlockCategory,
    /// Distance block-switch state.
    pub(crate) cat_d: BlockCategory,
    /// Context mode per literal block type.
    pub(crate) context_modes: Vec<ContextMode>,
    /// Literal context map.
    pub(crate) cmapl: ContextMap,
    /// Distance context map.
    pub(crate) cmapd: ContextMap,
    /// `NTREESL` literal prefix codes.
    pub(crate) literal_trees: Vec<HuffmanTree>,
    /// `NBLTYPESI` insert-and-copy prefix codes.
    pub(crate) ic_trees: Vec<HuffmanTree>,
    /// `NTREESD` distance prefix codes.
    pub(crate) distance_trees: Vec<HuffmanTree>,
    /// `NDIRECT`, already shifted left by `NPOSTFIX`.
    pub(crate) ndirect: u32,
    /// `NPOSTFIX`.
    pub(crate) npostfix: u32,
    /// `(1 << NPOSTFIX) - 1`.
    pub(crate) postfix_mask: u32,
    /// The block-splitting shape, for [`decompress_reporting_shapes`].
    pub(crate) shape: MetaBlockShape,
}

impl MetaBlockHeader {
    /// Parse a compressed meta-block header from `reader`.
    pub(crate) fn read(reader: &mut BitReader<'_>) -> BrotliResult<Self> {
        // Block type headers, in the fixed order L, I, D.
        let cat_l = BlockCategory::read(reader)?;
        let cat_i = BlockCategory::read(reader)?;
        let cat_d = BlockCategory::read(reader)?;

        // Distance parameters.
        let npostfix = reader.read_bits(2)?;
        let ndirect = reader.read_bits(4)? << npostfix;
        let postfix_mask = (1u32 << npostfix) - 1;
        let distance_alphabet_size = 16 + ndirect + (48 << npostfix);

        // Context modes, one per literal block type.
        let mut context_modes = Vec::with_capacity(cat_l.num_types as usize);
        for _ in 0..cat_l.num_types {
            let mode_bits = reader.read_bits(2)? as u8;
            let mode = ContextMode::from_bits(mode_bits).ok_or_else(|| {
                BrotliError::InvalidContextMap(format!("invalid context mode {mode_bits}"))
            })?;
            context_modes.push(mode);
        }

        // Literal and distance context maps (always preceded by NTREES fields).
        let ntreesl = read_block_type_count(reader)?;
        let cmapl_size = cat_l.num_types as usize * NUM_LITERAL_CONTEXTS;
        let cmapl = if ntreesl >= 2 {
            let map = read_context_map(reader, ntreesl, cmapl_size)?;
            ContextMap {
                map,
                num_contexts: NUM_LITERAL_CONTEXTS,
                num_trees: ntreesl as usize,
            }
        } else {
            ContextMap::trivial(cat_l.num_types as usize, NUM_LITERAL_CONTEXTS)
        };

        let ntreesd = read_block_type_count(reader)?;
        let cmapd_size = cat_d.num_types as usize * NUM_DISTANCE_CONTEXTS;
        let cmapd = if ntreesd >= 2 {
            let map = read_context_map(reader, ntreesd, cmapd_size)?;
            ContextMap {
                map,
                num_contexts: NUM_DISTANCE_CONTEXTS,
                num_trees: ntreesd as usize,
            }
        } else {
            ContextMap::trivial(cat_d.num_types as usize, NUM_DISTANCE_CONTEXTS)
        };

        // Prefix code arrays: NTREESL literal codes, NBLTYPESI insert-and-copy
        // codes, NTREESD distance codes.
        let mut literal_trees = Vec::with_capacity(ntreesl as usize);
        for _ in 0..ntreesl {
            literal_trees.push(read_prefix_code(reader, 256)?);
        }
        let mut ic_trees = Vec::with_capacity(cat_i.num_types as usize);
        for _ in 0..cat_i.num_types {
            ic_trees.push(read_prefix_code(reader, 704)?);
        }
        let mut distance_trees = Vec::with_capacity(ntreesd as usize);
        for _ in 0..ntreesd {
            distance_trees.push(read_prefix_code(reader, distance_alphabet_size)?);
        }

        let shape = MetaBlockShape {
            literal_types: cat_l.num_types,
            insert_and_copy_types: cat_i.num_types,
            distance_types: cat_d.num_types,
            literal_trees: ntreesl,
            distance_trees: ntreesd,
        };

        Ok(MetaBlockHeader {
            cat_l,
            cat_i,
            cat_d,
            context_modes,
            cmapl,
            cmapd,
            literal_trees,
            ic_trees,
            distance_trees,
            ndirect,
            npostfix,
            postfix_mask,
            shape,
        })
    }
}

/// Decode one compressed meta-block (RFC 7932 Sections 9.2/9.3), reporting the
/// block-splitting shape its header declared.
fn decode_compressed_meta_block(
    reader: &mut BitReader<'_>,
    output: &mut Vec<u8>,
    mlen: usize,
    state: &mut DecoderState,
    shared: &[u8],
) -> BrotliResult<MetaBlockShape> {
    let block_start = output.len();
    let target_len = block_start + mlen;

    let MetaBlockHeader {
        mut cat_l,
        mut cat_i,
        mut cat_d,
        context_modes,
        cmapl,
        cmapd,
        literal_trees,
        ic_trees,
        distance_trees,
        ndirect,
        npostfix,
        postfix_mask,
        shape,
    } = MetaBlockHeader::read(reader)?;

    // ── Command loop ─────────────────────────────────────────────────────
    while output.len() < target_len {
        // Insert-and-copy command symbol.
        cat_i.tick(reader)?;
        let Some(ic_tree) = ic_trees.get(cat_i.btype) else {
            return Err(BrotliError::InvalidBlockType(cat_i.btype as u8));
        };
        let ic_symbol = ic_tree.decode_symbol(reader)?;
        if ic_symbol >= 704 {
            return Err(BrotliError::CorruptedData(format!(
                "invalid insert-and-copy symbol {ic_symbol}"
            )));
        }
        let (ins_code, copy_code, implicit_zero) = decompose_command(ic_symbol);
        let (ins_base, ins_extra_bits) = INSERT_LENGTH_CODES[ins_code as usize];
        let insert_length = (ins_base + reader.read_bits(ins_extra_bits as u32)?) as usize;
        let (copy_base, copy_extra_bits) = COPY_LENGTH_CODES[copy_code as usize];
        let copy_length = (copy_base + reader.read_bits(copy_extra_bits as u32)?) as usize;

        // Literal insertion.
        if output.len() + insert_length > target_len {
            return Err(BrotliError::CorruptedData(
                "insert length exceeds meta-block length".to_string(),
            ));
        }
        for _ in 0..insert_length {
            cat_l.tick(reader)?;
            let Some(&mode) = context_modes.get(cat_l.btype) else {
                return Err(BrotliError::InvalidBlockType(cat_l.btype as u8));
            };
            let p1 = output.last().copied().unwrap_or(0);
            let p2 = if output.len() >= 2 {
                output[output.len() - 2]
            } else {
                0
            };
            let ctx = literal_context_id(mode, p1, p2);
            let tree_idx = cmapl.tree_index(cat_l.btype, ctx);
            let tree = literal_trees.get(tree_idx).ok_or_else(|| {
                BrotliError::InvalidContextMap(format!("literal tree {tree_idx} out of range"))
            })?;
            let literal = tree.decode_symbol(reader)? as u8;
            output.push(literal);
        }

        // If the insert part completed the meta-block, the copy part of the
        // last command is ignored (Section 9.3).
        if output.len() == target_len {
            break;
        }

        // Distance.
        let max_distance = state.window_size.min(output.len());
        let (distance, is_code_zero) = if implicit_zero {
            (state.last_distance(), true)
        } else {
            cat_d.tick(reader)?;
            let ctx = distance_context_id(copy_length);
            let tree_idx = cmapd.tree_index(cat_d.btype, ctx);
            let tree = distance_trees.get(tree_idx).ok_or_else(|| {
                BrotliError::InvalidContextMap(format!("distance tree {tree_idx} out of range"))
            })?;
            let dsym = tree.decode_symbol(reader)? as u32;
            decode_distance(reader, dsym, state, ndirect, npostfix, postfix_mask)?
        };

        match shared_dict::classify_distance(distance, max_distance, shared.len()) {
            shared_dict::DistanceSource::Output => {
                // Backward reference into the sliding window.
                if !is_code_zero {
                    state.push_distance(distance);
                }
                if output.len() + copy_length > target_len {
                    return Err(BrotliError::CorruptedData(
                        "copy length exceeds meta-block length".to_string(),
                    ));
                }
                // Overlapping copies are well-defined byte-by-byte.
                for _ in 0..copy_length {
                    let byte = output[output.len() - distance];
                    output.push(byte);
                }
            }
            shared_dict::DistanceSource::Shared { offset, available } => {
                // A shared-dictionary reference is an ordinary backward
                // reference into the extended history, so — unlike a static
                // dictionary word — it does go onto the distance ring.
                if !is_code_zero {
                    state.push_distance(distance);
                }
                if output.len() + copy_length > target_len {
                    return Err(BrotliError::CorruptedData(
                        "copy length exceeds meta-block length".to_string(),
                    ));
                }
                if copy_length > available {
                    return Err(dictionary_overrun(distance, copy_length, available));
                }
                output.extend_from_slice(&shared[offset..offset + copy_length]);
            }
            shared_dict::DistanceSource::Static { word_id } => {
                // Static dictionary reference (Section 8). Never pushed to the
                // distance ring buffer.
                if !(dictionary::MIN_DICTIONARY_WORD_LENGTH
                    ..=dictionary::MAX_DICTIONARY_WORD_LENGTH)
                    .contains(&copy_length)
                {
                    return Err(BrotliError::InvalidDistance {
                        distance,
                        max_distance,
                    });
                }
                let ndbits = dictionary::NDBITS[copy_length] as u64;
                let index = (word_id & ((1 << ndbits) - 1)) as u32;
                let transform_id = (word_id >> ndbits) as usize;
                if transform_id >= dictionary::NUM_TRANSFORMS {
                    return Err(BrotliError::InvalidDistance {
                        distance,
                        max_distance,
                    });
                }
                let word = dictionary::lookup_word(copy_length, index)?;
                dictionary::apply_transform_to(word, transform_id, output)?;
                if output.len() > target_len {
                    return Err(BrotliError::CorruptedData(
                        "dictionary word exceeds meta-block length".to_string(),
                    ));
                }
            }
        }
    }

    Ok(shape)
}

/// The error for a shared-dictionary copy that runs past the end of the
/// dictionary.
///
/// A shared-dictionary reference addresses `dict_len - (distance -
/// max_backward)` and may take at most the bytes from there to the end of the
/// dictionary: the dictionary is a *compound* history block, not a prefix
/// glued to the sliding window, so a copy cannot walk out of it and continue in
/// the produced output. `brotli 1.1.0` rejects such a stream ("corrupt input")
/// — verified in `tests/brotli_oracle.rs::
/// test_oracle_reference_rejects_a_copy_past_the_dictionary_end`, which feeds
/// the reference two streams differing only in one copy length.
///
/// Kept out of line and `#[cold]` deliberately. The backward-reference loop
/// this sits next to is the hottest loop in this decoder, and its speed turned
/// out to depend on the exact shape of the code around it: inlining a rare
/// continuation into the same function tripled the decode time of a
/// copy-dominated stream (measured: 616 us -> 2.10 ms on a 1 MiB repetitive
/// payload). Out of line, the hot loop's code generation cannot be perturbed
/// by it at all.
#[cold]
#[inline(never)]
pub(crate) fn dictionary_overrun(
    distance: usize,
    copy_length: usize,
    available: usize,
) -> BrotliError {
    BrotliError::CorruptedData(format!(
        "shared-dictionary copy of {copy_length} bytes at distance {distance} runs {} bytes \
         past the end of the dictionary (only {available} available)",
        copy_length - available
    ))
}

/// Convert a distance symbol into a distance (RFC 7932 Section 4).
///
/// Returns `(distance, is_code_zero)`; `is_code_zero` distances are not
/// pushed onto the ring buffer.
pub(crate) fn decode_distance(
    reader: &mut BitReader<'_>,
    dsym: u32,
    state: &DecoderState,
    ndirect: u32,
    npostfix: u32,
    postfix_mask: u32,
) -> BrotliResult<(usize, bool)> {
    if dsym == 0 {
        return Ok((state.last_distance(), true));
    }
    if dsym < 4 {
        return Ok((state.nth_last_distance(dsym as usize + 1), false));
    }
    if dsym < 16 {
        // Codes 4..9 modify the last distance; 10..15 the second-to-last.
        let (ref_n, delta) = match dsym {
            4 => (1usize, -1i64),
            5 => (1, 1),
            6 => (1, -2),
            7 => (1, 2),
            8 => (1, -3),
            9 => (1, 3),
            10 => (2, -1),
            11 => (2, 1),
            12 => (2, -2),
            13 => (2, 2),
            14 => (2, -3),
            _ => (2, 3),
        };
        let base = state.nth_last_distance(ref_n) as i64;
        let distance = base + delta;
        if distance <= 0 {
            return Err(BrotliError::InvalidDistance {
                distance: 0,
                max_distance: state.window_size,
            });
        }
        return Ok((distance as usize, false));
    }
    if dsym < 16 + ndirect {
        return Ok(((dsym - 16 + 1) as usize, false));
    }

    // Distances with extra bits.
    let d = dsym - ndirect - 16;
    let ndistbits = 1 + (d >> (npostfix + 1));
    if ndistbits > 24 {
        return Err(BrotliError::CorruptedData(format!(
            "invalid distance symbol {dsym}"
        )));
    }
    let hcode = d >> npostfix;
    let lcode = d & postfix_mask;
    let offset = ((2u64 + (hcode & 1) as u64) << ndistbits) - 4;
    let dextra = reader.read_bits(ndistbits)? as u64;
    let distance = ((offset + dextra) << npostfix) + lcode as u64 + ndirect as u64 + 1;
    Ok((distance as usize, false))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_inverse_move_to_front() {
        let mut data = vec![0, 0, 0, 0];
        inverse_move_to_front(&mut data);
        assert_eq!(data, vec![0, 0, 0, 0]);

        // MTF: reading index 1 repeatedly alternates the top two values.
        let mut data = vec![1, 1, 1];
        inverse_move_to_front(&mut data);
        assert_eq!(data, vec![1, 0, 1]);
    }

    #[test]
    fn test_read_window_bits_full_tree() {
        // Helper: build a bit stream from LSB-first bit strings.
        fn decode_bits(bits: &[u8]) -> BrotliResult<u32> {
            let mut byte = 0u8;
            let mut all = Vec::new();
            for (i, &b) in bits.iter().enumerate() {
                byte |= b << (i % 8);
                if i % 8 == 7 {
                    all.push(byte);
                    byte = 0;
                }
            }
            if bits.len() % 8 != 0 {
                all.push(byte);
            }
            let mut reader = BitReader::new(&all);
            read_window_bits(&mut reader)
        }
        // WBITS = 16: single 0 bit.
        assert_eq!(decode_bits(&[0]).ok(), Some(16));
        // WBITS = 18..24: 1 then 3 bits of (wbits - 17).
        for w in 18..=24u32 {
            let n = w - 17;
            assert_eq!(
                decode_bits(&[1, (n & 1) as u8, ((n >> 1) & 1) as u8, ((n >> 2) & 1) as u8]).ok(),
                Some(w)
            );
        }
        // WBITS = 17: 1 000 000.
        assert_eq!(decode_bits(&[1, 0, 0, 0, 0, 0, 0]).ok(), Some(17));
        // WBITS = 10..15: 1 000 then 3 bits of (wbits - 8).
        for w in 10..=15u32 {
            let m = w - 8;
            assert_eq!(
                decode_bits(&[
                    1,
                    0,
                    0,
                    0,
                    (m & 1) as u8,
                    ((m >> 1) & 1) as u8,
                    ((m >> 2) & 1) as u8
                ])
                .ok(),
                Some(w)
            );
        }
        // Reserved pattern 0010001 must be rejected.
        assert!(decode_bits(&[1, 0, 0, 0, 1, 0, 0]).is_err());
    }

    #[test]
    fn test_block_type_count_vlc() {
        // RFC example: pattern 0110111 (right-to-left 1,1,1,0,1,1,0) is 12.
        let data = [0b0011_0111u8];
        // bits LSB-first: 1,1,1,0,1,1,0 — first bit 1, n = 0b011 = 3,
        // extra = 0b011 = 3 -> (1<<3)+1+3 = 12.
        let mut reader = BitReader::new(&data);
        assert_eq!(read_block_type_count(&mut reader).ok(), Some(12));
        // Single 0 bit -> 1.
        let data = [0x00];
        let mut reader = BitReader::new(&data);
        assert_eq!(read_block_type_count(&mut reader).ok(), Some(1));
        // 1 then n=0 -> 2.
        let data = [0b0000_0001];
        let mut reader = BitReader::new(&data);
        assert_eq!(read_block_type_count(&mut reader).ok(), Some(2));
    }

    #[test]
    fn test_distance_ring_semantics() {
        // Initial ring: last=4, second=11, third=15, fourth=16.
        let state = DecoderState {
            dist_ring: [16, 15, 11, 4],
            dist_ring_idx: 0,
            window_size: 1 << 22,
        };
        assert_eq!(state.last_distance(), 4);
        assert_eq!(state.nth_last_distance(2), 11);
        assert_eq!(state.nth_last_distance(3), 15);
        assert_eq!(state.nth_last_distance(4), 16);

        let empty = [0u8; 4];
        let mut reader = BitReader::new(&empty);
        // Code 0: last distance, not pushed.
        assert_eq!(
            decode_distance(&mut reader, 0, &state, 0, 0, 0).ok(),
            Some((4, true))
        );
        // Code 4: last - 1 = 3. Code 5: last + 1 = 5.
        assert_eq!(
            decode_distance(&mut reader, 4, &state, 0, 0, 0).ok(),
            Some((3, false))
        );
        assert_eq!(
            decode_distance(&mut reader, 5, &state, 0, 0, 0).ok(),
            Some((5, false))
        );
        // Code 10: second-to-last - 1 = 10.
        assert_eq!(
            decode_distance(&mut reader, 10, &state, 0, 0, 0).ok(),
            Some((10, false))
        );
        // A push moves everything down.
        let mut state2 = state;
        state2.push_distance(100);
        assert_eq!(state2.last_distance(), 100);
        assert_eq!(state2.nth_last_distance(2), 4);
    }

    #[test]
    fn test_distance_short_code_underflow_rejected() {
        // last distance is 4; code 8 is last-3 = 1 (ok), but with a ring
        // value of 1, last-3 would be negative.
        let state = DecoderState {
            dist_ring: [16, 15, 11, 1],
            dist_ring_idx: 0,
            window_size: 1 << 22,
        };
        let empty = [0u8; 4];
        let mut reader = BitReader::new(&empty);
        assert!(decode_distance(&mut reader, 4, &state, 0, 0, 0).is_err()); // 1-1=0
        assert!(decode_distance(&mut reader, 8, &state, 0, 0, 0).is_err()); // 1-3<0
    }

    #[test]
    fn test_decode_distance_extra_bits_formula() {
        // npostfix=0, ndirect=0: dcode 16 -> ndistbits=1, offset=0,
        // distance = extra + 1 -> 1..2.
        let state = DecoderState {
            dist_ring: [16, 15, 11, 4],
            dist_ring_idx: 0,
            window_size: 1 << 22,
        };
        let zeros = [0u8; 4];
        let mut reader = BitReader::new(&zeros);
        assert_eq!(
            decode_distance(&mut reader, 16, &state, 0, 0, 0).ok(),
            Some((1, false))
        );
        let ones = [0xFFu8; 4];
        let mut reader = BitReader::new(&ones);
        assert_eq!(
            decode_distance(&mut reader, 16, &state, 0, 0, 0).ok(),
            Some((2, false))
        );
    }

    #[test]
    fn test_empty_stream_decodes_empty() {
        // RFC 11.1 trivial compressor: empty input is the single byte 6.
        assert_eq!(decompress(&[6]).ok(), Some(Vec::new()));
    }

    #[test]
    fn test_trivial_stored_stream() {
        // RFC 11.1 trivial compressor output for "abc":
        // byte 12 (WBITS=22 hmm — byte 12 encodes lgwin), then stored block.
        // Build it exactly as the RFC function does.
        let u = b"abc";
        let mut c = Vec::new();
        c.push(12u8);
        let r = u.len() - 1;
        c.push(((r & 31) << 3) as u8);
        c.push((r >> 5) as u8);
        c.push(8 + (r >> 13) as u8);
        c.extend_from_slice(u);
        c.push(3u8);
        assert_eq!(decompress(&c).ok(), Some(u.to_vec()));
    }

    #[test]
    fn test_trailing_garbage_rejected() {
        let mut stream = vec![6u8]; // valid empty stream
        stream.push(0xFF);
        assert!(decompress(&stream).is_err());
    }

    #[test]
    fn test_nonzero_padding_rejected() {
        // Empty stream byte 6 = bits 0,1,1 then five zero pad bits. Set a
        // pad bit: byte 6 | 0x40 has a non-zero fill bit.
        assert!(decompress(&[6 | 0x40]).is_err());
    }

    #[test]
    fn test_truncated_stream_rejected() {
        assert!(decompress(&[]).is_err());
        // Stored-block stream cut short.
        let u = b"hello world";
        let mut c = Vec::new();
        c.push(12u8);
        let r = u.len() - 1;
        c.push(((r & 31) << 3) as u8);
        c.push((r >> 5) as u8);
        c.push(8 + (r >> 13) as u8);
        c.extend_from_slice(&u[..4]); // truncate payload
        assert!(decompress(&c).is_err());
    }

    #[test]
    fn test_metadata_block_skipped() {
        // Metadata block (ISLAST=0, MNIBBLES=0 pattern 11, reserved=0,
        // MSKIPBYTES=1, MSKIPLEN-1=2 -> skip 3 bytes), then empty last.
        // Bits: 0 (ISLAST), 11 (MNIBBLES=0), 0 (reserved), 01 (MSKIPBYTES=1),
        // 00000010 (MSKIPLEN-1 = 2), pad to byte, 3 metadata bytes, then
        // byte 3 = ISLAST=1, ISLASTEMPTY=1.
        // Assemble bit-precisely: header byte 12 = WBITS 22.
        let mut bits: Vec<u8> = Vec::new();
        // WBITS 22: pattern 1011 LSB-first -> bits 1,1,0,1.
        bits.extend_from_slice(&[1, 1, 0, 1]);
        bits.push(0); // ISLAST = 0
        bits.extend_from_slice(&[1, 1]); // MNIBBLES code 3
        bits.push(0); // reserved
        bits.extend_from_slice(&[1, 0]); // MSKIPBYTES = 1
        bits.extend_from_slice(&[0, 1, 0, 0, 0, 0, 0, 0]); // MSKIPLEN-1 = 2
        // Pad to byte boundary with zeros (currently at 19 bits -> 5 pad).
        while bits.len() % 8 != 0 {
            bits.push(0);
        }
        let mut bytes = Vec::new();
        for chunk in bits.chunks(8) {
            let mut b = 0u8;
            for (i, &bit) in chunk.iter().enumerate() {
                b |= bit << i;
            }
            bytes.push(b);
        }
        bytes.extend_from_slice(&[0xAA, 0xBB, 0xCC]); // 3 metadata bytes
        bytes.push(0b11); // ISLAST=1, ISLASTEMPTY=1
        assert_eq!(decompress(&bytes).ok(), Some(Vec::new()));
    }
}
