//! Canonical LZH/LHA compression (`-lh4-`/`-lh5-`/`-lh6-`/`-lh7-`).
//!
//! This is the exact inverse of `decode.rs` / `huffman.rs`, which were
//! translated from the reference `lhasa` decoder (`fragglet/lhasa`,
//! `lib/lh_new_decoder.c`, `lib/tree_decode.c`, `lib/bit_stream_reader.c`).
//! Bits are written **most-significant-bit-first** via
//! [`oxiarc_core::MsbBitWriter`] — the opposite of DEFLATE's LSB-first
//! packing, and of this crate's own former (non-canonical) LZH format, which
//! bit-reversed every Huffman code to fake MSB semantics on top of an
//! LSB-first writer. No bit reversal remains anywhere in this path.
//!
//! # Canonical format specification (verified against lhasa source)
//!
//! ## Block structure
//!
//! The compressed stream is a sequence of **blocks**, read/written until the
//! expected uncompressed byte count has been produced. Each block is:
//!
//! ```text
//! [16-bit command count] [temp table] [code table] [offset table] [commands...]
//! ```
//!
//! The 16-bit field is a **count of commands** (literal bytes *or* copy
//! operations), **not** a byte count — a block of `N` long copies can cover far
//! more than `N` output bytes. (The prior format treated this field as a byte
//! count; that is a divergence from canonical, fixed here.) The three tables
//! are always present, in this exact order, even when one or more of them is
//! empty/degenerate — a reader unconditionally parses all three before the
//! first command.
//!
//! Each **command** is one C-tree symbol:
//! * `0..256`: a literal byte value.
//! * `256..NC`: a copy of `symbol - 256 + 3` bytes (minimum match length 3)
//!   from a distance given by the offset tree (below).
//!
//! ## The three per-block tables
//!
//! 1. **Temp table** (a.k.a. PT-tree, ≤ 31 symbols, `TEMP_CODE_BITS = 5`):
//!    used only to Huffman-decode the *code table*'s length list (§2). Format:
//!    `n` (5 bits); if `n == 0`, a single fixed symbol follows (5 bits) and the
//!    table is degenerate (every decode consumes 0 bits and returns that
//!    symbol). Otherwise, `n` code lengths follow, each raw-encoded as a
//!    ["length value"](#length-value-encoding), with one extra wrinkle: right
//!    after the length of **temp-table index 2** is written, a 2-bit field
//!    gives a count (0-3) of how many of the *following* temp-table indices
//!    (3, 4, 5) are skipped (implicitly length 0, no `length value` sent for
//!    them). This encoder always emits `0` here — skip is never used, which
//!    is always valid since a conformant decoder accepts any 0-3 value — but a
//!    decoder must still implement it to read third-party archives.
//!
//! 2. **Code table** (C-tree, `NC = 510` symbols, 9-bit count field): `n` (9
//!    bits); if `n == 0`, a single fixed symbol follows directly (9 bits, temp
//!    table not consulted). Otherwise, `n` code lengths are Huffman-decoded
//!    *through the temp tree*: each decoded temp-symbol `v` means:
//!    * `v == 0`: one code length of 0 (a 1-position skip).
//!    * `v == 1`: `get_bits(4) + 3` zero-length positions (a 3-18 skip).
//!    * `v == 2`: `get_bits(9) + 20` zero-length positions (a 20+ skip).
//!    * `v >= 3`: exactly one code length, value `v - 2`.
//!
//!    A skip only ever *advances the position counter*; it carries no length
//!    value of its own (those positions are 0/unused). Zero-run counts of
//!    exactly **2** and **19** have no single matching primitive (available
//!    primitives are 1, 3..=18, and 20..=531) and must be split across two
//!    consecutive skip instructions (e.g. 19 = an 18-run + a 1-run); this
//!    encoder's `c_length_program` greedily packs the largest usable
//!    primitive first, which naturally produces exactly that decomposition
//!    with no special-casing required.
//!
//!    **Divergence fixed here:** the prior format used temp-symbol `v - 3` as
//!    the code length (reserving `v == 3` as an always-unused "skip" slot that
//!    does not exist in the real format) instead of the canonical `v - 2`, and
//!    conflated the code table's zero-run mechanism with the temp table's
//!    unrelated index-2 skip-count field. They are two independent mechanisms
//!    operating at different structural levels; see [`huffman`](crate::huffman)
//!    module docs.
//!
//! 3. **Offset table** (P-tree, up to `(1 << offset_bits) - 1` symbols; 4 bits
//!    for `-lh4-`/`-lh5-`, 5 bits for `-lh6-`/`-lh7-`): `n` (count-field width
//!    bits); if `n == 0`, a single fixed symbol follows (count-field width
//!    bits). Otherwise, `n` code lengths follow, each raw-encoded as a
//!    ["length value"](#length-value-encoding) — no skip mechanism at all, one
//!    length per symbol unconditionally.
//!
//! ### Length-value encoding
//!
//! Shared by the temp table and offset table's own length lists (**not** used
//! for the code table, which is Huffman-coded instead — see above): 3 bits; if
//! the value is `7`, extended by unary `1`-bits terminated by a `0`-bit (e.g.
//! `7,1,1,0` reads as length `9`).
//!
//! ## Huffman canonicalization
//!
//! Given a set of per-symbol code lengths, codes are assigned in the standard
//! canonical order: process lengths from shortest to longest; within a length,
//! assign consecutive code values to symbols in ascending symbol-index order
//! (the textbook `bl_count`/`next_code` algorithm, identical to DEFLATE's).
//! [`huffman::LzhHuffmanTree::from_code_lengths`](crate::huffman::LzhHuffmanTree::from_code_lengths)
//! builds the matching decode tree directly from lengths (lhasa's
//! `build_tree`), so any correct canonical length assignment — this encoder
//! reuses a standard greedy Huffman-merge plus Kraft-based length limiting —
//! decodes correctly; the specific length-assignment algorithm is an encoder
//! implementation freedom, not part of the wire format.
//!
//! ## Position/distance encoding
//!
//! **Divergence fixed here:** the prior format encoded a match distance `d`
//! (1-based; `d == 1` means "the immediately preceding byte") as `p =
//! floor(log2(d))` extra-bit count with `d == (1 << p) + extra`. Canonical LHA
//! instead classifies `offset = d - 1` (0-based) by its **bit length**:
//!
//! * `offset == 0` (i.e. `d == 1`) → offset-tree symbol `0`, zero extra bits.
//! * `offset == 1` (i.e. `d == 2`) → offset-tree symbol `1`, zero extra bits.
//! * otherwise → symbol `bits = 32 - offset.leading_zeros()` (the bit length
//!   of `offset`, `>= 2`), followed by `bits - 1` extra bits holding `offset -
//!   (1 << (bits - 1))`.
//!
//! See `encode_offset` (the precise inverse of `decode::LzhDecoder`'s
//! `decode_offset`) and [`decode`](crate::decode) module docs.
//!
//! Per-method `offset_bits`/history-buffer sizing lives in
//! [`methods::LzhMethod`](crate::methods::LzhMethod) (`offset_bits`,
//! `history_bits`, `max_offset_codes`), verified against the lhasa
//! `lh{5,6,7}_decoder.c` wrappers. The LZSS match-finder's window size
//! (`LzhMethod::window_size`) is intentionally left smaller than the
//! canonical history-ring size in some cases (e.g. lh5: 8192-byte match
//! window vs. a 16384-byte canonical ring) — this is safe (any distance the
//! encoder can produce is always well within the decoder's larger ring) and
//! deliberately unchanged, since only the distance-to-symbol *convention* was
//! wrong, not the match-finding algorithm itself.

use crate::lzss::{LzssEncoder, LzssToken};
use crate::methods::LzhMethod;
use crate::methods::constants::{NC, NT};
use crate::optimal::LzssOptimalParser;
use oxiarc_core::MsbBitWriter;
use oxiarc_core::error::{OxiArcError, Result};
use oxiarc_core::progress::ProgressHandle;
use oxiarc_core::traits::{CompressStatus, Compressor, FlushMode};
use std::io::Write;

/// Maximum Huffman code length for the code table (C-tree). Temp-table
/// symbols encode code lengths as `value - 2`; the largest temp symbol (18,
/// since `MAX_TEMP_CODES` comfortably covers it) therefore denotes length 16.
const MAX_CODE_LEN: usize = 16;

/// Maximum number of commands (literal-or-copy operations) in a single block.
///
/// The command-count field is 16 bits wide (max 65535); this cap is chosen
/// well below that limit purely to keep the per-block Huffman tables
/// reasonably fresh/adapted to local statistics. Any value up to 65535 would
/// remain within the wire format's limit.
const MAX_COMMANDS_PER_BLOCK: usize = 0x4000;

/// Number of bits in the temp-table code-count field (lhasa `TEMP_CODE_BITS`).
const TEMP_CODE_BITS: u8 = 5;

/// Maximum number of temp-table codes (lhasa `MAX_TEMP_CODES`).
const MAX_TEMP_CODES: usize = (1 << TEMP_CODE_BITS) - 1; // 31

/// Maximum code length used when length-limiting the temp/offset auxiliary
/// tables. These alphabets are small (<= 31 symbols), so a generous cap here
/// never meaningfully costs compression ratio while safely bounding the
/// unary-extension length-value encoding.
const AUX_MAX_CODE_LEN: usize = 7;

/// LZH encoder.
pub struct LzhEncoder {
    /// Compression method.
    method: LzhMethod,
    /// LZSS encoder.
    lzss: LzssEncoder,
    /// Whether encoding is finished.
    finished: bool,
    /// Optional progress sink for reporting encode progress at block boundaries.
    progress: Option<ProgressHandle>,
    /// Whether to use the optimal (2-pass DP) parser instead of greedy.
    use_optimal: bool,
}

impl std::fmt::Debug for LzhEncoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LzhEncoder")
            .field("method", &self.method)
            .field("finished", &self.finished)
            .field("use_optimal", &self.use_optimal)
            .field(
                "progress",
                &self.progress.as_ref().map(|_| "<ProgressHandle>"),
            )
            .finish()
    }
}

impl LzhEncoder {
    /// Create a new LZH encoder.
    pub fn new(method: LzhMethod) -> Self {
        let window_size = method.window_size().max(256);
        let min_match = method.min_match();
        let max_match = method.max_match();

        Self {
            method,
            lzss: LzssEncoder::new(window_size, min_match, max_match),
            finished: false,
            progress: None,
            use_optimal: false,
        }
    }

    /// Create a default encoder (lh5).
    pub fn lh5() -> Self {
        Self::new(LzhMethod::Lh5)
    }

    /// Construct an encoder pre-loaded with a custom dictionary.
    ///
    /// The dictionary is written into the sliding window and hash chains before
    /// any user data is processed.  Compressed output produced by this encoder
    /// must be decompressed with an [`LzhDecoder`](crate::decode::LzhDecoder)
    /// initialised with the same dictionary via
    /// [`LzhDecoder::with_dictionary`](crate::decode::LzhDecoder::with_dictionary).
    ///
    /// If `dict` is larger than the window, only the last `window_size` bytes
    /// are used.
    pub fn with_dictionary(method: LzhMethod, dict: &[u8]) -> Self {
        let mut enc = Self::new(method);
        enc.set_dictionary(dict);
        enc
    }

    /// Preload a custom dictionary into the sliding window and hash chains.
    ///
    /// Equivalent to constructing with [`with_dictionary`](Self::with_dictionary)
    /// but usable after construction. Must be called before any data is encoded.
    pub fn set_dictionary(&mut self, dict: &[u8]) {
        self.lzss.preload_dictionary(dict);
    }

    /// Enable the optimal (two-pass DP) LZSS parser.
    ///
    /// When set, compression uses a Zopfli-style forward dynamic-programming
    /// parser that minimises the estimated total bit-cost of the token stream
    /// over two passes rather than the default greedy/lazy strategy.  The
    /// result is always bit-for-bit compatible with the standard LZH format.
    ///
    /// Optimal parsing is slower than greedy parsing but typically produces
    /// equal or smaller compressed output.
    pub fn with_optimal(mut self) -> Self {
        self.use_optimal = true;
        self
    }

    /// Attach a progress sink to this encoder.
    ///
    /// The sink will be called with `on_progress(input_consumed, None)` at
    /// each block boundary during encoding. `input_consumed` is the cumulative
    /// number of uncompressed bytes processed up to that block boundary.
    pub fn with_progress(mut self, handle: ProgressHandle) -> Self {
        self.progress = Some(handle);
        self
    }

    /// Reset the encoder.
    pub fn reset(&mut self) {
        self.lzss.reset();
        self.finished = false;
    }

    /// Encode data.
    pub fn encode<W: Write>(&mut self, data: &[u8], writer: &mut W, finish: bool) -> Result<()> {
        if self.method.is_stored() {
            // lh0 / lhd: just copy data (lhd entries carry no data at all)
            writer.write_all(data)?;
            if finish {
                self.finished = true;
            }
            return Ok(());
        }

        if self.method == LzhMethod::Lh1 {
            // lh1 uses an adaptive coder whose state cannot be resumed
            // across calls in this API — require single-shot encoding.
            if !finish {
                return Err(OxiArcError::unsupported_method(
                    "lh1 encoding requires a single call with finish=true",
                ));
            }
            let encoded = crate::lh1::encode_lh1(data);
            writer.write_all(&encoded)?;
            self.finished = true;
            return Ok(());
        }

        if let LzhMethod::Unknown(id) = self.method {
            return Err(OxiArcError::unsupported_method(
                String::from_utf8_lossy(&id).into_owned(),
            ));
        }

        let mut bit_writer = MsbBitWriter::new(writer);

        // Get LZSS tokens (greedy/lazy or optimal depending on configuration).
        let tokens = if self.use_optimal {
            let mut parser = LzssOptimalParser::new();
            parser.parse(data, &mut self.lzss)
        } else {
            self.lzss.encode(data)
        };

        self.encode_tokens(&tokens, &mut bit_writer)?;

        if finish {
            bit_writer.flush()?;
            self.finished = true;
        }

        Ok(())
    }

    /// Split tokens into blocks (each capped at [`MAX_COMMANDS_PER_BLOCK`]
    /// commands, comfortably within the wire format's 16-bit limit) and
    /// encode each in turn.
    fn encode_tokens<W: Write>(
        &mut self,
        tokens: &[LzssToken],
        writer: &mut MsbBitWriter<W>,
    ) -> Result<()> {
        if tokens.is_empty() {
            return Ok(());
        }

        let offset_bits = self.method.offset_bits();
        let max_offset_codes = self.method.max_offset_codes();

        // Cumulative uncompressed bytes consumed across all blocks so far
        // (for progress reporting only; independent of the wire-format
        // command count written per block).
        let mut total_input_consumed: u64 = 0;

        let mut pos = 0;
        while pos < tokens.len() {
            let block_end = tokens.len().min(pos + MAX_COMMANDS_PER_BLOCK);
            let block_tokens = &tokens[pos..block_end];

            encode_block(block_tokens, writer, offset_bits, max_offset_codes)?;

            if let Some(ref sink) = self.progress {
                let block_bytes: u64 = block_tokens
                    .iter()
                    .map(|t| match t {
                        LzssToken::Literal(_) => 1u64,
                        LzssToken::Match { length, .. } => u64::from(*length),
                    })
                    .sum();
                total_input_consumed += block_bytes;
                sink.on_progress(total_input_consumed, None);
            }

            pos = block_end;
        }

        Ok(())
    }

    /// Compress data to a Vec.
    pub fn compress_to_vec(&mut self, data: &[u8]) -> Result<Vec<u8>> {
        let mut output = Vec::new();
        self.encode(data, &mut output, true)?;
        Ok(output)
    }

    /// Get the compression method.
    pub fn method(&self) -> LzhMethod {
        self.method
    }
}

impl Default for LzhEncoder {
    fn default() -> Self {
        Self::lh5()
    }
}

impl Compressor for LzhEncoder {
    fn compress(
        &mut self,
        input: &[u8],
        output: &mut [u8],
        flush: FlushMode,
    ) -> Result<(usize, usize, CompressStatus)> {
        if self.finished {
            return Ok((0, 0, CompressStatus::Done));
        }

        let finish = matches!(flush, FlushMode::Finish);

        let mut buffer = Vec::new();
        self.encode(input, &mut buffer, finish)?;

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
        LzhEncoder::reset(self);
    }

    fn is_finished(&self) -> bool {
        self.finished
    }
}

/// Compress data using LZH.
pub fn encode_lzh(data: &[u8], method: LzhMethod) -> Result<Vec<u8>> {
    let mut encoder = LzhEncoder::new(method);
    encoder.compress_to_vec(data)
}

// ---------------------------------------------------------------------------
// Block encoding
// ---------------------------------------------------------------------------

/// Encode a single block of tokens: command count, the three tables, then the
/// command stream itself.
fn encode_block<W: Write>(
    tokens: &[LzssToken],
    writer: &mut MsbBitWriter<W>,
    offset_bits: u8,
    max_offset_codes: usize,
) -> Result<()> {
    debug_assert!(
        !tokens.is_empty(),
        "a block must contain at least one command"
    );

    let mut c_freq = vec![0u32; NC];
    let mut p_freq = vec![0u32; max_offset_codes.max(1)];

    for token in tokens {
        match token {
            LzssToken::Literal(b) => c_freq[*b as usize] += 1,
            LzssToken::Match { length, distance } => {
                c_freq[length_to_csym(*length)] += 1;
                let (sym, _, _) = encode_offset(*distance);
                if (sym as usize) < p_freq.len() {
                    p_freq[sym as usize] += 1;
                }
            }
        }
    }

    let c_lengths = build_code_lengths(&c_freq, MAX_CODE_LEN);
    let p_lengths = build_code_lengths(&p_freq, MAX_CODE_LEN);
    let c_codes = build_codes(&c_lengths);
    let p_codes = build_codes(&p_lengths);

    writer.put_bits(16, tokens.len() as u32)?;
    // Degenerate (single fixed symbol) tables consume zero bits per decode()
    // call — per-token symbol bits must be skipped entirely in that case, or
    // the bitstream desynchronizes by one code's worth of bits per token.
    let c_degenerate = write_code_table(writer, &c_lengths)?;
    let p_degenerate = write_offset_table(writer, &p_lengths, offset_bits)?;

    for token in tokens {
        match token {
            LzssToken::Literal(b) => {
                let sym = *b as usize;
                if !c_degenerate {
                    writer.put_bits(c_lengths[sym], c_codes[sym])?;
                }
            }
            LzssToken::Match { length, distance } => {
                let csym = length_to_csym(*length);
                if !c_degenerate {
                    writer.put_bits(c_lengths[csym], c_codes[csym])?;
                }

                let (sym, extra_bits, extra_value) = encode_offset(*distance);
                let sym = sym as usize;
                if sym >= p_lengths.len() {
                    return Err(OxiArcError::encoding_error(format!(
                        "offset symbol {sym} exceeds table size {} (distance {distance})",
                        p_lengths.len()
                    )));
                }
                if !p_degenerate {
                    writer.put_bits(p_lengths[sym], p_codes[sym])?;
                }
                if extra_bits > 0 {
                    writer.put_bits(extra_bits, extra_value)?;
                }
            }
        }
    }

    Ok(())
}

/// Map a match length (3-based) to its C-tree symbol index.
#[inline]
fn length_to_csym(length: u16) -> usize {
    ((length as usize).saturating_sub(3) + 256).min(NC - 1)
}

/// Map a match distance (1-based) to its canonical offset-tree symbol and
/// extra-bits payload. Exact inverse of `decode::LzhDecoder::decode_offset`.
///
/// Returns `(symbol, extra_bit_count, extra_value)`. `extra_bit_count` is 0
/// for `symbol` in `{0, 1}`.
fn encode_offset(distance: u16) -> (u8, u8, u32) {
    let offset = u32::from(distance) - 1; // 0-based; distance >= 1 always.
    if offset == 0 {
        (0, 0, 0)
    } else if offset == 1 {
        (1, 0, 0)
    } else {
        let bits = 32 - offset.leading_zeros(); // bit length of offset, >= 2.
        let extra_bits = (bits - 1) as u8;
        let extra_value = offset - (1u32 << (bits - 1));
        (bits as u8, extra_bits, extra_value)
    }
}

// ---------------------------------------------------------------------------
// Length-value primitive (shared raw encoding for temp/offset table lengths)
// ---------------------------------------------------------------------------

/// Write a raw code length via the shared "length value" format (mirrors
/// `huffman::read_length_value`): 3 bits; if that value is 7, extended by
/// unary 1-bits terminated by a 0-bit.
fn write_length_value<W: Write>(writer: &mut MsbBitWriter<W>, len: u8) -> Result<()> {
    if len < 7 {
        writer.put_bits(3, u32::from(len))?;
    } else {
        writer.put_bits(3, 7)?;
        for _ in 0..(len - 7) {
            writer.put_bit(true)?;
        }
        writer.put_bit(false)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Temp table (PT-tree) writing
// ---------------------------------------------------------------------------

/// Write a degenerate (single-code) temp table whose content will never
/// actually be consulted by the decoder (used when the code table itself
/// bypasses the temp tree via its own `n == 0` degenerate path). Any valid
/// 5-bit value works.
fn write_temp_dummy<W: Write>(writer: &mut MsbBitWriter<W>) -> Result<()> {
    writer.put_bits(TEMP_CODE_BITS, 0)?;
    writer.put_bits(TEMP_CODE_BITS, 0)?;
    Ok(())
}

/// Write a degenerate (single-code) temp table fixed to `symbol`.
fn write_temp_single<W: Write>(writer: &mut MsbBitWriter<W>, symbol: u16) -> Result<()> {
    writer.put_bits(TEMP_CODE_BITS, 0)?;
    writer.put_bits(TEMP_CODE_BITS, u32::from(symbol))?;
    Ok(())
}

/// Write a full (non-degenerate) temp table: `n` (count of used slots,
/// `pt_lengths`'s trimmed trailing-zero form), then each length raw via
/// [`write_length_value`]. The index-2 skip field is always sent as `0`
/// (never used) — a conformant decoder accepts any value in `0..=3`.
fn write_temp_full<W: Write>(writer: &mut MsbBitWriter<W>, pt_lengths: &[u8]) -> Result<()> {
    let n_temp = pt_lengths
        .iter()
        .rposition(|&l| l > 0)
        .map_or(0, |p| p + 1)
        .min(MAX_TEMP_CODES);
    writer.put_bits(TEMP_CODE_BITS, n_temp as u32)?;
    for (i, &len) in pt_lengths.iter().take(n_temp).enumerate() {
        write_length_value(writer, len)?;
        if i == 2 {
            writer.put_bits(2, 0)?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Code table (C-tree) writing — the length list is itself Huffman-coded via
// a temp tree built for this purpose.
// ---------------------------------------------------------------------------

/// One instruction for transmitting a single position (or run of positions)
/// of the code table's length list, via the temp-tree alphabet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TempOp {
    /// Temp-symbol 0: exactly one zero-length position.
    ZeroRun1,
    /// Temp-symbol 1 with a 4-bit extra field: `extra + 3` zero-length
    /// positions (covers run lengths 3..=18).
    ZeroRunShort(u8),
    /// Temp-symbol 2 with a 9-bit extra field: `extra + 20` zero-length
    /// positions (covers run lengths 20..=531).
    ZeroRunLong(u16),
    /// Temp-symbol `len + 2`: a single non-zero code length (`len` in
    /// `1..=16`).
    Length(u8),
}

impl TempOp {
    /// The temp-tree symbol value this instruction is encoded as.
    fn symbol(self) -> u16 {
        match self {
            TempOp::ZeroRun1 => 0,
            TempOp::ZeroRunShort(_) => 1,
            TempOp::ZeroRunLong(_) => 2,
            TempOp::Length(len) => u16::from(len) + 2,
        }
    }
}

/// Decompose the code table's length array `lengths[0..n)` into a sequence of
/// [`TempOp`]s. Zero-runs are greedily packed into the largest single
/// available primitive (1, 3..=18, or 20..=531); run lengths of exactly 2 or
/// 19 have no single matching primitive and are naturally split across two
/// consecutive instructions by this same greedy rule (2 = 1+1; 19 = 18+1),
/// which is the canonical zero-run convention (no special-casing needed).
fn c_length_program(lengths: &[u8], n: usize) -> Vec<TempOp> {
    let mut ops = Vec::new();
    let mut i = 0usize;
    while i < n {
        if lengths[i] == 0 {
            let mut run = 1usize;
            while i + run < n && lengths[i + run] == 0 {
                run += 1;
            }
            let mut remaining = run;
            while remaining > 0 {
                if remaining >= 20 {
                    let take = remaining.min(531);
                    ops.push(TempOp::ZeroRunLong((take - 20) as u16));
                    remaining -= take;
                } else if remaining >= 3 {
                    let take = remaining.min(18);
                    ops.push(TempOp::ZeroRunShort((take - 3) as u8));
                    remaining -= take;
                } else {
                    ops.push(TempOp::ZeroRun1);
                    remaining -= 1;
                }
            }
            i += run;
        } else {
            ops.push(TempOp::Length(lengths[i]));
            i += 1;
        }
    }
    ops
}

/// Count temp-symbol frequencies across an op sequence (used to build the
/// temp tree's own Huffman lengths).
fn build_pt_freq(ops: &[TempOp]) -> Vec<u32> {
    let mut freq = vec![0u32; NT];
    for op in ops {
        let sym = op.symbol() as usize;
        if sym < NT {
            freq[sym] += 1;
        }
    }
    freq
}

/// Write the code table: `n` (9 bits); degenerate single-code path if only
/// one symbol has nonzero length; otherwise the temp tree followed by the
/// Huffman-coded length-list program.
///
/// Returns `true` if the code table was written in its degenerate
/// (`n == 0`, single fixed symbol) form. A degenerate table's decode tree
/// (`LzhHuffmanTree::single`) consumes **zero bits** per symbol — the
/// caller (`encode_block`) must therefore skip emitting any per-token C-tree
/// symbol bits when this returns `true`; emitting them anyway (as an earlier
/// version of this function's caller did) desynchronizes the bitstream by
/// one code's worth of bits per token, corrupting every subsequent command.
fn write_code_table<W: Write>(writer: &mut MsbBitWriter<W>, c_lengths: &[u8]) -> Result<bool> {
    let n = c_lengths.iter().rposition(|&l| l > 0).map_or(0, |p| p + 1);
    let used_count = c_lengths.iter().filter(|&&l| l > 0).count();

    if n == 0 {
        // No symbols at all — still must emit the (unconditionally-read)
        // temp table, then a degenerate code table.
        write_temp_dummy(writer)?;
        writer.put_bits(9, 0)?;
        writer.put_bits(9, 0)?;
        return Ok(true);
    }

    if used_count == 1 {
        let sym = c_lengths.iter().position(|&l| l > 0).unwrap_or(0);
        write_temp_dummy(writer)?;
        writer.put_bits(9, 0)?;
        writer.put_bits(9, sym as u32)?;
        return Ok(true);
    }

    let ops = c_length_program(c_lengths, n);
    let pt_freq = build_pt_freq(&ops);
    let pt_lengths = build_code_lengths(&pt_freq, AUX_MAX_CODE_LEN);
    let pt_used = pt_freq.iter().filter(|&&f| f > 0).count();

    if pt_used <= 1 {
        // Every op shares one temp-symbol: since a Length op always occurs at
        // least once (position n-1 is nonzero by construction of `n`), this
        // means the whole length list is one repeated identical nonzero
        // length with no zero-runs at all. The temp tree is degenerate and
        // every position is read for free (0 bits), so no op bits are sent.
        let sym = pt_freq.iter().position(|&f| f > 0).unwrap_or(0);
        write_temp_single(writer, sym as u16)?;
        writer.put_bits(9, n as u32)?;
        return Ok(false);
    }

    let pt_codes = build_codes(&pt_lengths);
    write_temp_full(writer, &pt_lengths)?;
    writer.put_bits(9, n as u32)?;
    for op in &ops {
        let sym = op.symbol() as usize;
        writer.put_bits(pt_lengths[sym], pt_codes[sym])?;
        match *op {
            TempOp::ZeroRunShort(extra) => writer.put_bits(4, u32::from(extra))?,
            TempOp::ZeroRunLong(extra) => writer.put_bits(9, u32::from(extra))?,
            TempOp::ZeroRun1 | TempOp::Length(_) => {}
        }
    }
    Ok(false)
}

// ---------------------------------------------------------------------------
// Offset table (P-tree) writing — lengths sent raw, no skip mechanism.
// ---------------------------------------------------------------------------

/// Write the offset table: `n` (count-field-width bits); degenerate
/// single-code path if only one symbol has nonzero length; otherwise `n` raw
/// length values, one per used symbol slot.
///
/// Returns `true` if the offset table was written in its degenerate
/// (`n == 0`, single fixed symbol) form — see [`write_code_table`]'s doc for
/// why the caller must skip per-token symbol bits in that case.
fn write_offset_table<W: Write>(
    writer: &mut MsbBitWriter<W>,
    p_lengths: &[u8],
    offset_bits: u8,
) -> Result<bool> {
    let n = p_lengths.iter().rposition(|&l| l > 0).map_or(0, |p| p + 1);
    let used_count = p_lengths.iter().filter(|&&l| l > 0).count();

    if n == 0 {
        writer.put_bits(offset_bits, 0)?;
        writer.put_bits(offset_bits, 0)?;
        return Ok(true);
    }

    if used_count == 1 {
        let sym = p_lengths.iter().position(|&l| l > 0).unwrap_or(0);
        writer.put_bits(offset_bits, 0)?;
        writer.put_bits(offset_bits, sym as u32)?;
        return Ok(true);
    }

    writer.put_bits(offset_bits, n as u32)?;
    for &len in p_lengths.iter().take(n) {
        write_length_value(writer, len)?;
    }
    Ok(false)
}

// ---------------------------------------------------------------------------
// Canonical Huffman length assignment (bit-order-agnostic; reused unchanged
// from the combinatorial algorithm, only the bitstream layer changed).
// ---------------------------------------------------------------------------

/// Build Huffman code lengths from frequencies using a standard greedy
/// tree-merge, then length-limit to `max_len` while preserving a valid
/// (Kraft-satisfying) prefix code.
fn build_code_lengths(freqs: &[u32], max_len: usize) -> Vec<u8> {
    let n = freqs.len();
    if n == 0 {
        return Vec::new();
    }

    let symbols: Vec<(usize, u32)> = freqs
        .iter()
        .enumerate()
        .filter(|&(_, f)| *f > 0)
        .map(|(i, f)| (i, *f))
        .collect();

    let mut lengths = vec![0u8; n];

    if symbols.is_empty() {
        return lengths;
    }
    if symbols.len() == 1 {
        lengths[symbols[0].0] = 1;
        return lengths;
    }
    if symbols.len() == 2 {
        lengths[symbols[0].0] = 1;
        lengths[symbols[1].0] = 1;
        return lengths;
    }

    // Each node is (combined frequency, symbols contained).
    let mut nodes: Vec<(u64, Vec<usize>)> = symbols
        .iter()
        .map(|&(sym, freq)| (u64::from(freq), vec![sym]))
        .collect();
    nodes.sort_by_key(|&(freq, _)| freq);

    while nodes.len() > 1 {
        let (freq1, syms1) = nodes.remove(0);
        let (freq2, syms2) = nodes.remove(0);

        let combined_freq = freq1 + freq2;
        let mut combined_syms = syms1;
        combined_syms.extend(syms2);

        for &sym in &combined_syms {
            lengths[sym] += 1;
        }

        let pos = nodes
            .iter()
            .position(|&(f, _)| f > combined_freq)
            .unwrap_or(nodes.len());
        nodes.insert(pos, (combined_freq, combined_syms));
    }

    limit_code_lengths(&mut lengths, max_len);
    lengths
}

/// Limit code lengths to `max_len` while maintaining the Kraft inequality
/// (needed for the resulting lengths to form a valid, uniquely-decodable
/// prefix code).
fn limit_code_lengths(lengths: &mut [u8], max_len: usize) {
    let max_len = max_len as u8;

    if !lengths.iter().any(|&l| l > max_len) {
        return;
    }

    let mut items: Vec<(usize, u8)> = lengths
        .iter()
        .enumerate()
        .filter(|&(_, l)| *l > 0)
        .map(|(i, l)| (i, *l))
        .collect();
    items.sort_by_key(|b| std::cmp::Reverse(b.1));

    for &mut (sym, ref mut len) in &mut items {
        if *len > max_len {
            *len = max_len;
            lengths[sym] = max_len;
        }
    }

    loop {
        let scale = 1u64 << max_len;
        let kraft_sum: u64 = lengths
            .iter()
            .filter(|&&l| l > 0)
            .map(|&l| scale >> l)
            .sum();

        if kraft_sum <= scale {
            break;
        }

        let mut increased = false;
        for len in lengths.iter_mut() {
            if *len > 0 && *len < max_len {
                *len += 1;
                increased = true;
                break;
            }
        }

        if !increased {
            break;
        }
    }
}

/// Build canonical Huffman codes from lengths (standard `bl_count`/
/// `next_code` algorithm; the same one `LzhHuffmanTree::from_code_lengths`
/// assumes when it walks the equivalent tree structure).
fn build_codes(lengths: &[u8]) -> Vec<u32> {
    let n = lengths.len();
    let mut codes = vec![0u32; n];
    if n == 0 {
        return codes;
    }

    let max_len = *lengths.iter().max().unwrap_or(&0) as usize;
    let mut bl_count = vec![0u32; max_len + 1];
    for &len in lengths {
        if len > 0 {
            bl_count[len as usize] += 1;
        }
    }

    let mut next_code = vec![0u32; max_len + 1];
    let mut code = 0u32;
    for bits in 1..=max_len {
        code = (code + bl_count[bits - 1]) << 1;
        next_code[bits] = code;
    }

    for (sym, &len) in lengths.iter().enumerate() {
        if len > 0 {
            codes[sym] = next_code[len as usize];
            next_code[len as usize] += 1;
        }
    }

    codes
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode::decode_lzh;

    // -------------------------------------------------------------------------
    // White-box test: offset table round-trip (2 symbols, non-degenerate)
    // -------------------------------------------------------------------------

    #[test]
    fn offset_table_roundtrip_two_symbols() {
        use crate::huffman::read_offset_tree;
        use oxiarc_core::MsbBitReader;

        let mut p_lengths = vec![0u8; 15]; // lh5: max_offset_codes = 15
        p_lengths[2] = 1;
        p_lengths[3] = 1;
        let p_codes = build_codes(&p_lengths);

        let mut buf = Vec::new();
        {
            let mut w = MsbBitWriter::new(&mut buf);
            let degenerate = write_offset_table(&mut w, &p_lengths, 4).expect("write");
            assert!(!degenerate, "two distinct symbols must not be degenerate");
            // Now write both symbols' codes, as encode_block would.
            w.put_bits(p_lengths[2], p_codes[2]).expect("put sym2");
            w.put_bits(p_lengths[3], p_codes[3]).expect("put sym3");
            w.flush().expect("flush");
        }

        let mut r = MsbBitReader::new(std::io::Cursor::new(buf));
        let tree = read_offset_tree(&mut r, 4, 15).expect("read");
        let d2 = tree.decode(&mut r).expect("decode sym2");
        let d3 = tree.decode(&mut r).expect("decode sym3");
        assert_eq!(d2, 2, "first written symbol must decode back to 2");
        assert_eq!(d3, 3, "second written symbol must decode back to 3");
    }

    // -------------------------------------------------------------------------
    // White-box tests: c_length_program decomposition
    // -------------------------------------------------------------------------

    #[test]
    fn c_length_program_covers_exact_19_zero_run() {
        // lengths: [5, <19 zeros>, 3]; n = 21 (last nonzero at index 20).
        let mut lengths = vec![0u8; 21];
        lengths[0] = 5;
        lengths[20] = 3;
        let ops = c_length_program(&lengths, 21);

        // Expect: Length(5), then a decomposition of 19 zeros, then Length(3).
        assert_eq!(ops[0], TempOp::Length(5));
        assert_eq!(*ops.last().expect("non-empty ops"), TempOp::Length(3));

        // The zero-run instructions between them must sum to exactly 19.
        let mut total = 0usize;
        for op in &ops[1..ops.len() - 1] {
            total += match *op {
                TempOp::ZeroRun1 => 1,
                TempOp::ZeroRunShort(extra) => extra as usize + 3,
                TempOp::ZeroRunLong(extra) => extra as usize + 20,
                TempOp::Length(_) => panic!("unexpected Length op inside zero run"),
            };
        }
        assert_eq!(total, 19, "19-zero-run must decompose to an exact total");
        // No single primitive covers 19 directly; must be >= 2 instructions.
        assert!(
            ops.len() - 2 >= 2,
            "count 19 has no single-instruction primitive"
        );
    }

    #[test]
    fn c_length_program_covers_exact_2_zero_run() {
        let mut lengths = vec![0u8; 4];
        lengths[0] = 1;
        lengths[3] = 1;
        let ops = c_length_program(&lengths, 4);
        assert_eq!(ops[0], TempOp::Length(1));
        assert_eq!(*ops.last().expect("non-empty ops"), TempOp::Length(1));
        let total: usize = ops[1..ops.len() - 1]
            .iter()
            .map(|op| match *op {
                TempOp::ZeroRun1 => 1,
                TempOp::ZeroRunShort(extra) => extra as usize + 3,
                TempOp::ZeroRunLong(extra) => extra as usize + 20,
                TempOp::Length(_) => panic!("unexpected Length op"),
            })
            .sum();
        assert_eq!(total, 2);
    }

    #[test]
    fn c_length_program_large_run_uses_long_primitive() {
        let mut lengths = vec![0u8; 600];
        lengths[599] = 4;
        let ops = c_length_program(&lengths, 600);
        // 599 zeros then a Length(4); must decompose using ZeroRunLong chunks.
        assert!(matches!(ops[0], TempOp::ZeroRunLong(_)));
        let total: usize = ops[..ops.len() - 1]
            .iter()
            .map(|op| match *op {
                TempOp::ZeroRun1 => 1,
                TempOp::ZeroRunShort(extra) => extra as usize + 3,
                TempOp::ZeroRunLong(extra) => extra as usize + 20,
                TempOp::Length(_) => panic!("unexpected Length op"),
            })
            .sum();
        assert_eq!(total, 599);
    }

    #[test]
    fn encode_offset_matches_decode_offset_inverse() {
        // Round-trip encode_offset -> reconstruct distance, for a spread of
        // representative distances (including boundary values around each
        // bit-length transition).
        let candidates: Vec<u16> = (1u16..=20)
            .chain([31, 32, 33, 63, 64, 65, 255, 256, 257, 8191, 8192])
            .collect();
        for distance in candidates {
            let (sym, extra_bits, extra_value) = encode_offset(distance);
            let offset = if sym == 0 {
                0u32
            } else if sym == 1 {
                1u32
            } else {
                (1u32 << (sym - 1)) + extra_value
            };
            assert_eq!(
                offset + 1,
                u32::from(distance),
                "distance {distance} -> symbol {sym} extra_bits {extra_bits} must invert exactly"
            );
        }
    }

    // -------------------------------------------------------------------------
    // Public API smoke tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_encode_stored() {
        let data = b"Hello, World!";
        let encoded = encode_lzh(data, LzhMethod::Lh0).expect("compression/encoding failed");
        assert_eq!(encoded, data);
    }

    #[test]
    fn test_encoder_creation() {
        let encoder = LzhEncoder::new(LzhMethod::Lh5);
        assert_eq!(encoder.method(), LzhMethod::Lh5);
        assert!(!encoder.finished);
    }

    #[test]
    fn test_encoder_reset() {
        let mut encoder = LzhEncoder::new(LzhMethod::Lh5);
        let _ = encoder.compress_to_vec(b"test");
        encoder.reset();
        assert!(!encoder.finished);
    }

    #[test]
    fn test_lh5_roundtrip_simple() {
        let data = b"Hello, World!";
        let encoded = encode_lzh(data, LzhMethod::Lh5).expect("compression/encoding failed");
        let decoded =
            decode_lzh(&encoded, LzhMethod::Lh5, data.len() as u64).expect("decompression failed");
        assert_eq!(decoded, data);
    }

    #[test]
    fn test_lh5_roundtrip_repeated() {
        let data = b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
        let encoded = encode_lzh(data, LzhMethod::Lh5).expect("compression/encoding failed");
        let decoded =
            decode_lzh(&encoded, LzhMethod::Lh5, data.len() as u64).expect("decompression failed");
        assert_eq!(decoded, data);
    }

    #[test]
    fn test_lh5_roundtrip_pattern() {
        let data = b"abcabcabcabcabcabcabc";
        let encoded = encode_lzh(data, LzhMethod::Lh5).expect("compression/encoding failed");
        let decoded =
            decode_lzh(&encoded, LzhMethod::Lh5, data.len() as u64).expect("decompression failed");
        assert_eq!(decoded, data);
    }

    // -------------------------------------------------------------------------
    // Degenerate-case tests (task-required)
    // -------------------------------------------------------------------------

    #[test]
    fn test_degenerate_single_literal_symbol() {
        // A single one-byte input: exactly one C-tree symbol used overall
        // (the code table's `used_count == 1` degenerate path).
        let data = b"Z";
        let encoded = encode_lzh(data, LzhMethod::Lh5).expect("compress failed");
        let decoded =
            decode_lzh(&encoded, LzhMethod::Lh5, data.len() as u64).expect("decode failed");
        assert_eq!(decoded, data);
    }

    #[test]
    fn test_degenerate_single_offset_symbol() {
        // Regression test for a real bug found during development: "AB"
        // repeated many times produces many Match tokens that all reuse the
        // *same* offset-tree symbol (distance 4 throughout, since the hash
        // chain always resolves to the first occurrence 4 bytes back),
        // driving the offset table's `used_count == 1` degenerate path. A
        // degenerate table's decode tree consumes zero bits per symbol, but
        // an earlier version of `encode_block` unconditionally wrote
        // `p_lengths[sym]` bits per match regardless of degeneracy, injecting
        // one spurious bit per match token and desynchronizing the
        // bitstream after the first block — corrupting every subsequent
        // command. `write_code_table`/`write_offset_table` now report
        // degeneracy back to the caller so per-token bits are correctly
        // skipped in that case.
        let data: Vec<u8> = b"AB".iter().cycle().take(4000).copied().collect();
        let encoded = encode_lzh(&data, LzhMethod::Lh5).expect("compress failed");
        let decoded =
            decode_lzh(&encoded, LzhMethod::Lh5, data.len() as u64).expect("decode failed");
        assert_eq!(decoded, data);
    }

    #[test]
    fn test_degenerate_single_code_symbol_with_many_tokens() {
        // Same bug class as `test_degenerate_single_offset_symbol` but for the
        // *code* (C-tree) table: many tokens (not just one) all sharing the
        // same single C-tree symbol. A run of the same byte long enough that
        // every resulting LZSS match shares an identical length code (and no
        // literal ever recurs) would trigger `write_code_table`'s
        // `used_count == 1` path across multiple per-token emissions.
        // Constructed directly against `encode_block` to guarantee a single
        // repeated C-tree symbol regardless of how the LZSS matcher happens
        // to tokenize this particular crate version's greedy parser.
        use crate::huffman::{read_code_tree, read_offset_tree, read_temp_tree};
        use oxiarc_core::MsbBitReader;

        let mut buf = Vec::new();
        {
            let mut w = MsbBitWriter::new(&mut buf);
            let tokens = vec![LzssToken::Literal(b'Q'); 50];
            encode_block(&tokens, &mut w, 4, 15).expect("encode_block");
            w.flush().expect("flush");
        }
        let mut r = MsbBitReader::new(std::io::Cursor::new(buf));
        // Manually decode: 16-bit command count, then all three tables via
        // the huffman module's readers, mirroring decode.rs::decode_compressed
        // for a single block covering the whole (degenerate) token stream.
        let count = r.get_bits(16).expect("count");
        assert_eq!(count, 50);
        let temp_tree = read_temp_tree(&mut r).expect("temp tree");
        let code_tree = read_code_tree(&mut r, &temp_tree).expect("code tree");
        let _offset_tree = read_offset_tree(&mut r, 4, 15).expect("offset tree");
        for _ in 0..50 {
            let sym = code_tree.decode(&mut r).expect("decode literal");
            assert_eq!(
                sym,
                u16::from(b'Q'),
                "every command must decode back to 'Q'"
            );
        }
    }

    #[test]
    fn test_sparse_symbols_exercise_zero_run_gaps() {
        // Byte values 0x01 and 0xF0 only: a wide gap (238 zero-length
        // positions) in the C-tree's length list between them.
        let mut data = Vec::new();
        for i in 0..500u32 {
            data.push(if i % 7 == 0 { 0xF0u8 } else { 0x01u8 });
        }
        let encoded = encode_lzh(&data, LzhMethod::Lh5).expect("compress failed");
        let decoded =
            decode_lzh(&encoded, LzhMethod::Lh5, data.len() as u64).expect("decode failed");
        assert_eq!(decoded, data);
    }

    #[test]
    fn test_all_256_byte_values_roundtrip() {
        let data: Vec<u8> = (0u16..=255).map(|v| v as u8).collect();
        for method in [
            LzhMethod::Lh4,
            LzhMethod::Lh5,
            LzhMethod::Lh6,
            LzhMethod::Lh7,
        ] {
            let encoded = encode_lzh(&data, method).expect("compress failed");
            let decoded = decode_lzh(&encoded, method, data.len() as u64).expect("decode failed");
            assert_eq!(decoded, data, "roundtrip failed for {method}");
        }
    }

    #[test]
    fn test_empty_input_roundtrip() {
        let data: Vec<u8> = Vec::new();
        let encoded = encode_lzh(&data, LzhMethod::Lh5).expect("compress failed");
        assert!(encoded.is_empty(), "empty input must produce empty output");
        let decoded = decode_lzh(&encoded, LzhMethod::Lh5, 0).expect("decode failed");
        assert_eq!(decoded, data);
    }

    /// A simple progress sink for test purposes.
    struct CountingSink {
        calls: std::sync::atomic::AtomicU64,
        last_processed: std::sync::atomic::AtomicU64,
    }

    impl CountingSink {
        fn new() -> Self {
            Self {
                calls: std::sync::atomic::AtomicU64::new(0),
                last_processed: std::sync::atomic::AtomicU64::new(0),
            }
        }

        fn call_count(&self) -> u64 {
            self.calls.load(std::sync::atomic::Ordering::SeqCst)
        }

        fn last_processed(&self) -> u64 {
            self.last_processed
                .load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    impl oxiarc_core::progress::ProgressSink for CountingSink {
        fn on_progress(&self, processed: u64, _total: Option<u64>) {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.last_processed
                .store(processed, std::sync::atomic::Ordering::SeqCst);
        }
    }

    // -------------------------------------------------------------------------
    // Custom dictionary tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_lzh_with_dictionary_roundtrip() {
        let dict: Vec<u8> = (0u8..=255).collect();
        let data: Vec<u8> = b"hello dictionary world hello dictionary world"
            .iter()
            .cycle()
            .take(512)
            .copied()
            .collect();

        let mut encoder = LzhEncoder::with_dictionary(LzhMethod::Lh5, &dict);
        let compressed = encoder
            .compress_to_vec(&data)
            .expect("encode with dictionary failed");

        let mut decoder =
            crate::decode::LzhDecoder::with_dictionary(LzhMethod::Lh5, data.len() as u64, &dict);
        let mut cursor = std::io::Cursor::new(&compressed);
        let decoded = decoder
            .decode(&mut cursor)
            .expect("decode with dictionary failed");

        assert_eq!(decoded, data, "dictionary roundtrip must be lossless");
    }

    #[test]
    fn test_lzh_dictionary_improves_ratio() {
        let dict: Vec<u8> = b"the quick brown fox jumps over the lazy dog "
            .iter()
            .cycle()
            .take(512)
            .copied()
            .collect();

        let data: Vec<u8> = b"the quick brown fox the quick brown fox jumps over the lazy dog "
            .iter()
            .cycle()
            .take(1024)
            .copied()
            .collect();

        let mut enc_with = LzhEncoder::with_dictionary(LzhMethod::Lh5, &dict);
        let size_with = enc_with
            .compress_to_vec(&data)
            .expect("encode with dict failed")
            .len();

        let mut enc_without = LzhEncoder::new(LzhMethod::Lh5);
        let size_without = enc_without
            .compress_to_vec(&data)
            .expect("encode without dict failed")
            .len();

        assert!(
            size_with < size_without,
            "expected dictionary to improve compression ({size_with} < {size_without})"
        );
    }

    #[test]
    fn test_lzh_empty_dictionary_is_noop() {
        let data: Vec<u8> = b"abcdefghijklmnopqrstuvwxyz"
            .iter()
            .cycle()
            .take(256)
            .copied()
            .collect();

        let mut enc_empty_dict = LzhEncoder::with_dictionary(LzhMethod::Lh5, b"");
        let with_empty = enc_empty_dict
            .compress_to_vec(&data)
            .expect("encode with empty dict failed");

        let mut enc_no_dict = LzhEncoder::new(LzhMethod::Lh5);
        let without = enc_no_dict
            .compress_to_vec(&data)
            .expect("encode without dict failed");

        assert_eq!(
            with_empty, without,
            "empty dictionary must produce identical output to no dictionary"
        );
    }

    #[test]
    fn test_lzh_dictionary_mismatch_no_panic() {
        let dict_a: Vec<u8> = b"alpha_prefix".iter().cycle().take(128).copied().collect();
        let dict_b: Vec<u8> = b"beta_prefix".iter().cycle().take(128).copied().collect();

        let data: Vec<u8> = b"some data to compress with dict_a"
            .iter()
            .cycle()
            .take(128)
            .copied()
            .collect();

        let mut encoder = LzhEncoder::with_dictionary(LzhMethod::Lh5, &dict_a);
        let compressed = encoder
            .compress_to_vec(&data)
            .expect("encode with dict_a failed");

        let mut decoder =
            crate::decode::LzhDecoder::with_dictionary(LzhMethod::Lh5, data.len() as u64, &dict_b);
        let mut cursor = std::io::Cursor::new(&compressed);
        let _ = decoder.decode(&mut cursor);
    }

    #[test]
    fn test_lzh_set_dictionary_after_construction() {
        let dict = b"shared_prefix_data";
        let data: Vec<u8> = b"shared_prefix_data and more shared_prefix_data here"
            .iter()
            .cycle()
            .take(256)
            .copied()
            .collect();

        let mut enc_a = LzhEncoder::with_dictionary(LzhMethod::Lh5, dict);
        let out_a = enc_a
            .compress_to_vec(&data)
            .expect("with_dictionary encode failed");

        let mut enc_b = LzhEncoder::new(LzhMethod::Lh5);
        enc_b.set_dictionary(dict);
        let out_b = enc_b
            .compress_to_vec(&data)
            .expect("set_dictionary encode failed");

        assert_eq!(
            out_a, out_b,
            "with_dictionary and new+set_dictionary must produce identical output"
        );
    }

    #[test]
    fn test_lzh_dictionary_with_lh5_lh6_lh7() {
        let dict = b"shared_prefix";
        let data: Vec<u8> = b"shared_prefix hello shared_prefix world"
            .iter()
            .cycle()
            .take(128)
            .copied()
            .collect();

        for method in [LzhMethod::Lh5, LzhMethod::Lh6, LzhMethod::Lh7] {
            let mut encoder = LzhEncoder::with_dictionary(method, dict);
            let compressed = encoder
                .compress_to_vec(&data)
                .expect("encode failed for method");

            let mut decoder =
                crate::decode::LzhDecoder::with_dictionary(method, data.len() as u64, dict);
            let mut cursor = std::io::Cursor::new(&compressed);
            let decoded = decoder
                .decode(&mut cursor)
                .expect("decode failed for method");

            assert_eq!(decoded, data, "roundtrip failed for method {:?}", method);
        }
    }

    #[test]
    fn test_progress_callbacks_encode() {
        use std::sync::Arc;

        let input: Vec<u8> = vec![b'A'; 40];
        let input_size = input.len();

        let sink = Arc::new(CountingSink::new());
        let handle: oxiarc_core::progress::ProgressHandle = sink.clone();

        let mut encoder = LzhEncoder::new(LzhMethod::Lh5).with_progress(handle);
        let encoded = encoder.compress_to_vec(&input).expect("encode failed");

        let decoded =
            decode_lzh(&encoded, LzhMethod::Lh5, input_size as u64).expect("decode failed");
        assert_eq!(decoded, input, "decoded output must match original input");

        assert!(
            sink.call_count() >= 1,
            "on_progress must be called at least once; calls = {}",
            sink.call_count()
        );

        assert_eq!(
            sink.last_processed(),
            input_size as u64,
            "last processed ({}) should equal input size ({})",
            sink.last_processed(),
            input_size
        );
    }
}
