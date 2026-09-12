//! The resumable DEFLATE state machine that [`crate::stream::InflateStream`]
//! drives.
//!
//! Split out of `stream.rs` along the seam the design already required: the
//! symbol loop needs `&HuffmanTree` live across `&mut` calls on the decoder
//! state, so the trees ([`Trees`]) and the mutable state ([`InflateCore`])
//! are separate structs and the loops are free functions rather than
//! methods. Keeping them in their own module makes that boundary explicit
//! and keeps both files well inside the 1500-line target.
//!
//! Nothing here is public: `InflateStream` is the only way in.

use oxiarc_core::BitCache;
use oxiarc_core::error::{OxiArcError, Result};
use oxiarc_core::traits::FlushMode;

mod fast;

use crate::decode_table::{self, DecodeTable, TableKind, fixed_distance_table, fixed_litlen_table};
use crate::huffman::HuffmanTree;
use crate::sink::InflateSink;
use crate::stream::InflateStatus;
use crate::tables::CODE_LENGTH_ORDER;

/// Largest number of code lengths a dynamic block can declare
/// (`HLIT_max` 288 + `HDIST_max` 32).
const MAX_CODE_LENGTHS: usize = 320;

// ---------------------------------------------------------------------------
// Fault latch
// ---------------------------------------------------------------------------

/// A decode error remembered so every later call reports it again.
///
/// [`OxiArcError`] is not `Clone` (it wraps `std::io::Error`), so the latch
/// stores enough to rebuild an equivalent error rather than the error
/// itself. Every variant the decoder can raise is represented exactly.
#[derive(Debug, Clone)]
pub(crate) enum Fault {
    Corrupted {
        offset: u64,
        message: String,
    },
    InvalidHeader(String),
    InvalidHuffman {
        bit_position: u64,
    },
    UnexpectedEof {
        expected: usize,
    },
    InvalidDistance {
        distance: usize,
        history_size: usize,
    },
    MemoryBudget {
        budget: usize,
        requested: usize,
    },
    Bomb {
        ratio: f64,
        threshold: f64,
    },
    BufferTooSmall {
        needed: usize,
        available: usize,
    },
    /// Raised by the framing layer, not the DEFLATE core: a container magic
    /// that does not match.
    InvalidMagic {
        expected: Vec<u8>,
        found: Vec<u8>,
    },
    /// Raised by the framing layer: an unsupported container compression
    /// method, or a missing zlib preset dictionary.
    UnsupportedMethod(String),
    /// Raised by the framing layer: a trailer or `FHCRC` checksum that does
    /// not match what was decoded.
    CrcMismatch {
        expected: u32,
        computed: u32,
    },
    Other(String),
}

impl Fault {
    /// Summarise an error so it can be replayed by [`Fault::to_error`].
    ///
    /// Used by [`crate::wrapper::WrappedInflate`] as well as the core, so a
    /// replayed framing error keeps its variant instead of collapsing into
    /// `CorruptedData`.
    pub(crate) fn from_error(error: &OxiArcError) -> Self {
        match error {
            OxiArcError::CorruptedData { offset, message } => Fault::Corrupted {
                offset: *offset,
                message: message.clone(),
            },
            OxiArcError::InvalidHeader { message } => Fault::InvalidHeader(message.clone()),
            OxiArcError::InvalidHuffmanCode { bit_position } => Fault::InvalidHuffman {
                bit_position: *bit_position,
            },
            OxiArcError::UnexpectedEof { expected } => Fault::UnexpectedEof {
                expected: *expected,
            },
            OxiArcError::InvalidDistance {
                distance,
                history_size,
            } => Fault::InvalidDistance {
                distance: *distance,
                history_size: *history_size,
            },
            OxiArcError::MemoryBudgetExceeded { budget, requested } => Fault::MemoryBudget {
                budget: *budget,
                requested: *requested,
            },
            OxiArcError::ZipBomb { ratio, threshold } => Fault::Bomb {
                ratio: *ratio,
                threshold: *threshold,
            },
            OxiArcError::BufferTooSmall { needed, available } => Fault::BufferTooSmall {
                needed: *needed,
                available: *available,
            },
            OxiArcError::InvalidMagic { expected, found } => Fault::InvalidMagic {
                expected: expected.clone(),
                found: found.clone(),
            },
            OxiArcError::UnsupportedMethod { method } => Fault::UnsupportedMethod(method.clone()),
            OxiArcError::CrcMismatch { expected, computed } => Fault::CrcMismatch {
                expected: *expected,
                computed: *computed,
            },
            other => Fault::Other(other.to_string()),
        }
    }

    /// Rebuild the error this fault stands for.
    pub(crate) fn to_error(&self) -> OxiArcError {
        match self {
            Fault::Corrupted { offset, message } => {
                OxiArcError::corrupted(*offset, message.clone())
            }
            Fault::InvalidHeader(message) => OxiArcError::invalid_header(message.clone()),
            Fault::InvalidHuffman { bit_position } => OxiArcError::invalid_huffman(*bit_position),
            Fault::UnexpectedEof { expected } => OxiArcError::unexpected_eof(*expected),
            Fault::InvalidDistance {
                distance,
                history_size,
            } => OxiArcError::invalid_distance(*distance, *history_size),
            Fault::MemoryBudget { budget, requested } => {
                OxiArcError::memory_budget_exceeded(*budget, *requested)
            }
            Fault::Bomb { ratio, threshold } => OxiArcError::zip_bomb(*ratio, *threshold),
            Fault::BufferTooSmall { needed, available } => {
                OxiArcError::buffer_too_small(*needed, *available)
            }
            Fault::InvalidMagic { expected, found } => {
                OxiArcError::invalid_magic(expected.clone(), found.clone())
            }
            Fault::UnsupportedMethod(method) => OxiArcError::unsupported_method(method.clone()),
            Fault::CrcMismatch { expected, computed } => {
                OxiArcError::crc_mismatch(*expected, *computed)
            }
            Fault::Other(message) => OxiArcError::corrupted(0, message.clone()),
        }
    }
}

// ---------------------------------------------------------------------------
// State machine
// ---------------------------------------------------------------------------

/// The 14 resumption points of RFC 1951 decoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InflateState {
    /// `BFINAL` + `BTYPE` (3 bits).
    BlockHeader,
    /// Byte alignment plus `LEN`/`NLEN` (32 bits) of a stored block.
    StoredLen,
    /// Copying a stored block's payload.
    StoredCopy,
    /// `HLIT`/`HDIST`/`HCLEN` (14 bits).
    TableSizes,
    /// The `HCLEN` × 3-bit code-length code lengths.
    CodeLenLengths,
    /// One code-length symbol.
    CodeLens,
    /// The 2/3/7 extra bits of a code-length repeat (16/17/18).
    CodeLensRepeat,
    /// One literal/length symbol.
    Symbol,
    /// The 0..=5 extra bits of a length code.
    LengthExtra,
    /// One distance symbol.
    DistSymbol,
    /// The 0..=13 extra bits of a distance code.
    DistExtra,
    /// Copying a back-reference, possibly across several calls.
    CopyMatch,
    /// The final block's end-of-block marker has been decoded.
    Done,
    /// A fault is latched; every call replays it.
    Bad,
}

/// Which pair of Huffman trees the symbol loop is reading.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ActiveTrees {
    /// RFC 1951 §3.2.6 fixed codes (`&'static`, never rebuilt).
    Fixed,
    /// Codes read from the current dynamic block's header.
    Dynamic,
}

/// Everything the symbol loop only reads.
///
/// Split away from [`InflateCore`] so the hot loop can hold `&HuffmanTree`
/// across `&mut InflateCore` calls without fighting the borrow checker,
/// which is also why the loops are free functions rather than methods.
#[derive(Debug)]
pub(crate) struct Trees {
    pub(crate) which: ActiveTrees,
    /// The 19-symbol code-length alphabet, read once per dynamic header.
    /// Still a [`HuffmanTree`]: it decodes a few dozen symbols per block,
    /// so the packed layout would buy nothing.
    codelen: HuffmanTree,
    litlen: DecodeTable,
    dist: DecodeTable,
}

impl Trees {
    pub(crate) fn new() -> Self {
        Self {
            which: ActiveTrees::Fixed,
            codelen: HuffmanTree::degenerate(),
            litlen: DecodeTable::new(TableKind::LitLen),
            dist: DecodeTable::new(TableKind::Distance),
        }
    }

    /// Resolve to plain shared references once, at the call site, so the
    /// `'static` fixed tables and the owned dynamic ones never have to
    /// unify their lifetimes inside the loop.
    ///
    /// The fixed tables are built once per process, never per block.
    fn active(&self) -> (&DecodeTable, &DecodeTable) {
        match self.which {
            ActiveTrees::Fixed => (fixed_litlen_table(), fixed_distance_table()),
            ActiveTrees::Dynamic => (&self.litlen, &self.dist),
        }
    }
}

/// Everything the symbol loop mutates.
#[derive(Debug)]
pub(crate) struct InflateCore {
    // ── bit accumulator (carried across calls, and across members) ────────
    pub(crate) cache: BitCache,
    /// Bits of the DEFLATE stream actually used (not merely absorbed).
    pub(crate) bits_used: u64,
    /// `cache.consumed()` at the start of the current `run`, so the delta
    /// can be folded into `bits_used`.
    pub(crate) bits_base: u64,

    // ── control ──────────────────────────────────────────────────────────
    pub(crate) state: InflateState,
    pub(crate) last_block: bool,
    pub(crate) sync_flush: bool,
    pub(crate) fault: Option<Fault>,

    // ── stored block ─────────────────────────────────────────────────────
    stored_remaining: u16,

    // ── dynamic header construction (no heap allocation) ─────────────────
    hlit: u16,
    hdist: u16,
    hclen: u8,
    codelen_lengths: [u8; 19],
    codelen_index: u8,
    lengths: [u8; MAX_CODE_LENGTHS],
    lengths_filled: u16,
    repeat_symbol: u8,

    // ── pending symbol / match ───────────────────────────────────────────
    /// Base length of a length code whose extra bits are still outstanding.
    pending_length_base: u16,
    /// Number of extra bits that length code still needs.
    pending_length_extra: u8,
    /// Fully decoded match length, waiting for the distance.
    pending_length: u16,
    /// Base distance of a distance code whose extra bits are outstanding.
    pending_dist_base: u16,
    /// Number of extra bits that distance code still needs.
    pending_dist_extra: u8,
    match_distance: usize,
    match_remaining: usize,

    // ── accounting / limits ──────────────────────────────────────────────
    pub(crate) total_out: u64,
    pub(crate) total_in: u64,
    pub(crate) max_output: Option<u64>,
    pub(crate) ratio_guard: Option<(f64, u64)>,
}

/// Outcome of one careful-path step.
pub(crate) enum Step {
    /// State advanced; keep driving.
    Continue,
    /// The call must return with this status.
    Stop(InflateStatus),
}

impl InflateCore {
    pub(crate) fn new() -> Self {
        Self {
            cache: BitCache::default(),
            bits_used: 0,
            bits_base: 0,
            state: InflateState::BlockHeader,
            last_block: false,
            sync_flush: false,
            fault: None,
            stored_remaining: 0,
            hlit: 0,
            hdist: 0,
            hclen: 0,
            codelen_lengths: [0; 19],
            codelen_index: 0,
            lengths: [0; MAX_CODE_LENGTHS],
            lengths_filled: 0,
            repeat_symbol: 0,
            pending_length_base: 0,
            pending_length_extra: 0,
            pending_length: 0,
            pending_dist_base: 0,
            pending_dist_extra: 0,
            match_distance: 0,
            match_remaining: 0,
            total_out: 0,
            total_in: 0,
            max_output: None,
            ratio_guard: None,
        }
    }

    /// Clear per-stream decoding state, leaving limits and counters alone.
    pub(crate) fn clear_decode_state(&mut self) {
        self.state = InflateState::BlockHeader;
        self.last_block = false;
        self.sync_flush = false;
        self.fault = None;
        self.stored_remaining = 0;
        self.codelen_index = 0;
        self.lengths_filled = 0;
        self.repeat_symbol = 0;
        self.pending_length_base = 0;
        self.pending_length_extra = 0;
        self.pending_length = 0;
        self.pending_dist_base = 0;
        self.pending_dist_extra = 0;
        self.match_distance = 0;
        self.match_remaining = 0;
    }

    // ── error helpers ────────────────────────────────────────────────────

    /// Latch `error` and return it. Every later call replays it.
    #[cold]
    fn latch(&mut self, error: OxiArcError) -> OxiArcError {
        self.state = InflateState::Bad;
        self.fault = Some(Fault::from_error(&error));
        error
    }

    /// Byte offset used in `CorruptedData` errors.
    fn byte_offset(&self) -> u64 {
        (self.bits_used + self.cache.consumed().saturating_sub(self.bits_base)) / 8
    }

    /// Bit position used in `InvalidHuffmanCode` errors.
    fn bit_offset(&self) -> u64 {
        self.bits_used + self.cache.consumed().saturating_sub(self.bits_base)
    }

    #[cold]
    fn fail_corrupted(&mut self, message: impl Into<String>) -> OxiArcError {
        let at = self.byte_offset();
        self.latch(OxiArcError::corrupted(at, message))
    }

    #[cold]
    fn fail_header(&mut self, message: &'static str) -> OxiArcError {
        self.latch(OxiArcError::invalid_header(message))
    }

    #[cold]
    fn fail_invalid_huffman(&mut self) -> OxiArcError {
        let at = self.bit_offset();
        self.latch(OxiArcError::invalid_huffman(at))
    }

    #[cold]
    fn fail_truncated(&mut self) -> OxiArcError {
        self.latch(OxiArcError::unexpected_eof(1))
    }

    // ── bit helpers ──────────────────────────────────────────────────────

    /// Top up the accumulator to at least `want` bits, absorbing bytes from
    /// `input`. Returns whether the request was met.
    #[inline]
    fn refill(&mut self, input: &[u8], in_pos: &mut usize, want: u8) -> bool {
        if self.cache.available() >= want {
            return true;
        }
        if let Some(rest) = input.get(*in_pos..) {
            *in_pos += self.cache.refill_bulk(rest);
        }
        if self.cache.available() >= want {
            return true;
        }
        if let Some(rest) = input.get(*in_pos..) {
            *in_pos += self.cache.refill_bytes(rest, want);
        }
        self.cache.available() >= want
    }

    /// Read `want` bits atomically (`want <= 32`).
    ///
    /// `Ok(None)` means "not enough input yet"; nothing is consumed in that
    /// case, so the state is safe to re-enter.
    #[inline]
    fn take_bits(
        &mut self,
        input: &[u8],
        in_pos: &mut usize,
        want: u8,
        flush: FlushMode,
    ) -> Result<Option<u32>> {
        debug_assert!(want <= 32);
        if !self.refill(input, in_pos, want) {
            if is_finish(flush) {
                return Err(self.fail_truncated());
            }
            return Ok(None);
        }
        let value = self.cache.peek_bits(want);
        self.cache.consume(want);
        Ok(Some(value))
    }

    /// Peek one Huffman code **without consuming it**.
    ///
    /// Returning the code length lets the caller decide whether it can
    /// afford the symbol (a literal needs output space) before committing —
    /// which is what makes `inflate_into` on an exactly-sized buffer report
    /// `StreamEnd` rather than `NeedOutput`.
    #[inline]
    fn peek_symbol(
        &mut self,
        tree: &HuffmanTree,
        input: &[u8],
        in_pos: &mut usize,
        flush: FlushMode,
    ) -> Result<Option<(u16, u8)>> {
        if tree.max_code_length() == 0 {
            // A tree with no codes at all can never decode anything; say so
            // now rather than letting the "held every bit and still failed"
            // rule below reach the same answer indirectly.
            return Err(self.fail_invalid_huffman());
        }
        let full = self.refill(input, in_pos, tree.max_code_length());
        let entry = tree.lookup_cached(&self.cache);
        let bits = HuffmanTree::entry_length(entry);
        if bits != 0 && bits <= self.cache.available() {
            return Ok(Some((HuffmanTree::entry_symbol(entry), bits)));
        }
        if full {
            // Every bit the longest code could need was real stream data and
            // no code matched: the stream is corrupt.
            return Err(self.fail_invalid_huffman());
        }
        if is_finish(flush) {
            return Err(self.fail_truncated());
        }
        Ok(None)
    }

    /// [`InflateCore::peek_symbol`] for a packed [`DecodeTable`]: returns
    /// the whole entry (kind flag, code length, extra-bit count and
    /// payload) **without consuming** it.
    ///
    /// The returned entry is always usable: its code length is non-zero and
    /// no longer than the bits actually held.
    #[inline]
    fn peek_entry(
        &mut self,
        table: &DecodeTable,
        input: &[u8],
        in_pos: &mut usize,
        flush: FlushMode,
    ) -> Result<Option<u32>> {
        if table.max_len() == 0 {
            return Err(self.fail_invalid_huffman());
        }
        let full = self.refill(input, in_pos, table.max_len());
        let entry = table.entry(self.cache.peek_bits(32));
        let bits = decode_table::entry_len(entry);
        if bits != 0 && bits <= self.cache.available() {
            return Ok(Some(entry));
        }
        if full {
            return Err(self.fail_invalid_huffman());
        }
        if is_finish(flush) {
            return Err(self.fail_truncated());
        }
        Ok(None)
    }

    // ── limits ───────────────────────────────────────────────────────────

    /// Bytes that may still be produced before a configured limit is hit.
    ///
    /// `u64::MAX` when unlimited; `0` when a further byte would break a
    /// limit.
    #[inline]
    fn allowance(&self, total_in_now: u64) -> u64 {
        let mut allow = u64::MAX;
        if let Some(limit) = self.max_output {
            allow = allow.min(limit.saturating_sub(self.total_out));
        }
        if let Some((ratio, min_output)) = self.ratio_guard {
            let scaled = ratio * (total_in_now.max(1) as f64);
            let ceiling = if scaled >= u64::MAX as f64 {
                u64::MAX
            } else {
                (scaled as u64).max(min_output)
            };
            allow = allow.min(ceiling.saturating_sub(self.total_out));
        }
        allow
    }

    /// Latch the error an exhausted budget stands for, and return it.
    ///
    /// The caller decides whether to raise it now or first hand back the
    /// bytes this call already produced — see [`limit_stop`].
    #[cold]
    fn latch_budget(&mut self, total_in_now: u64) -> OxiArcError {
        if let Some(limit) = self.max_output {
            if self.total_out >= limit {
                let requested = self.total_out.saturating_add(1);
                let error =
                    OxiArcError::memory_budget_exceeded(clamp_usize(limit), clamp_usize(requested));
                return self.latch(error);
            }
        }
        let ratio = match self.ratio_guard {
            Some((ratio, _)) => ratio,
            None => 0.0,
        };
        let observed = (self.total_out as f64) / (total_in_now.max(1) as f64);
        self.latch(OxiArcError::zip_bomb(observed, ratio))
    }

    /// Move to the next block, or to `Done` after the final one.
    #[inline]
    fn finish_block(&mut self) {
        self.state = if self.last_block {
            InflateState::Done
        } else {
            InflateState::BlockHeader
        };
    }
}

/// `Full`/`Partial`/anything added later behave as `None`; only `Finish`
/// turns a short stream into an error.
#[inline]
fn is_finish(flush: FlushMode) -> bool {
    match flush {
        FlushMode::Finish => true,
        FlushMode::None | FlushMode::Sync | FlushMode::Full | FlushMode::Partial => false,
        // `FlushMode` is `#[non_exhaustive]`: anything new is encoder-side
        // and reads as `None` here.
        _ => false,
    }
}

/// `Sync` asks the decoder to return at the next sync-flush boundary.
#[inline]
fn is_sync(flush: FlushMode) -> bool {
    match flush {
        FlushMode::Sync => true,
        FlushMode::None | FlushMode::Finish | FlushMode::Full | FlushMode::Partial => false,
        _ => false,
    }
}

/// Bytes still allowed before a configured limit trips, or the error it
/// trips with.
///
/// A budget that runs out is a *clean* stop, not corruption: everything
/// decoded so far is valid, so this hands those bytes back with
/// [`InflateStatus::NeedOutput`] and lets the latched fault surface on the
/// next call. Without that, a caller that asked for at most `n` bytes and
/// got exactly `n` would receive a bare `Err` and lose all of them.
///
/// When nothing has been produced yet in this call there is nothing to hand
/// back, so the error is raised immediately.
#[inline]
fn limit_stop<S: InflateSink>(
    core: &mut InflateCore,
    sink: &S,
    total_in_now: u64,
) -> Result<std::result::Result<u64, Step>> {
    let allow = core.allowance(total_in_now);
    if allow > 0 {
        return Ok(Ok(allow));
    }
    let error = core.latch_budget(total_in_now);
    if sink.written() > 0 {
        return Ok(Err(Step::Stop(InflateStatus::NeedOutput)));
    }
    Err(error)
}

/// Clamp a 64-bit count to `usize` on 32-bit targets.
#[inline]
fn clamp_usize(value: u64) -> usize {
    match usize::try_from(value) {
        Ok(v) => v,
        Err(_) => usize::MAX,
    }
}

// ---------------------------------------------------------------------------
// The careful path
// ---------------------------------------------------------------------------

/// One careful step of the state machine: it computes what it needs, checks
/// that it is there, and either advances or returns.
///
/// Every arm either consumes at least one bit, produces at least one byte,
/// or returns with `input` fully absorbed — the invariant that stops a
/// caller's drive loop from spinning.
fn step_state<S: InflateSink, const LIMITED: bool>(
    core: &mut InflateCore,
    trees: &mut Trees,
    sink: &mut S,
    input: &[u8],
    in_pos: &mut usize,
    flush: FlushMode,
) -> Result<Step> {
    match core.state {
        InflateState::BlockHeader => {
            let Some(header) = core.take_bits(input, in_pos, 3, flush)? else {
                return Ok(Step::Stop(InflateStatus::NeedInput));
            };
            core.last_block = (header & 1) != 0;
            match (header >> 1) & 0b11 {
                0 => core.state = InflateState::StoredLen,
                1 => {
                    core.sync_flush = false;
                    trees.which = ActiveTrees::Fixed;
                    core.state = InflateState::Symbol;
                }
                2 => {
                    core.sync_flush = false;
                    core.state = InflateState::TableSizes;
                }
                _ => return Err(core.fail_header("Reserved block type 3")),
            }
            Ok(Step::Continue)
        }

        InflateState::StoredLen => {
            // Idempotent: once the accumulator holds whole bytes, every
            // refill keeps it that way, so re-entry after `NeedInput`
            // discards nothing.
            core.cache.align_to_byte();
            let Some(value) = core.take_bits(input, in_pos, 32, flush)? else {
                return Ok(Step::Stop(InflateStatus::NeedInput));
            };
            let len = (value & 0xFFFF) as u16;
            let nlen = ((value >> 16) & 0xFFFF) as u16;
            if len != !nlen {
                return Err(core.fail_corrupted(format!("LEN/NLEN mismatch: {} vs {}", len, !nlen)));
            }
            core.sync_flush = len == 0;
            core.stored_remaining = len;
            core.state = InflateState::StoredCopy;
            Ok(Step::Continue)
        }

        InflateState::StoredCopy => {
            debug_assert_eq!(core.cache.available() % 8, 0);
            if core.stored_remaining == 0 {
                core.finish_block();
                if core.sync_flush && is_sync(flush) {
                    let status = if core.state == InflateState::Done {
                        InflateStatus::StreamEnd
                    } else {
                        InflateStatus::NeedInput
                    };
                    return Ok(Step::Stop(status));
                }
                return Ok(Step::Continue);
            }

            let allow = match limit_stop(core, sink, core.total_in + *in_pos as u64)? {
                Ok(allow) => allow,
                Err(stop) => return Ok(stop),
            };
            let room = sink
                .space()
                .min(clamp_usize(allow))
                .min(core.stored_remaining as usize);
            if room == 0 {
                return Ok(Step::Stop(InflateStatus::NeedOutput));
            }

            // Bytes already inside the accumulator come first: they were
            // reported as consumed when they were absorbed and can never be
            // re-read from `input`.
            if core.cache.available() >= 8 {
                let mut taken = 0usize;
                while taken < room {
                    let Some(byte) = core.cache.take_byte() else {
                        break;
                    };
                    if let Err(error) = sink.write_literal(byte) {
                        return Err(core.latch(error));
                    }
                    taken += 1;
                }
                core.stored_remaining -= taken as u16;
                core.total_out += taken as u64;
                return Ok(Step::Continue);
            }

            let available = input.len() - *in_pos;
            if available == 0 {
                if is_finish(flush) {
                    return Err(core.fail_truncated());
                }
                return Ok(Step::Stop(InflateStatus::NeedInput));
            }
            let take = room.min(available);
            let Some(chunk) = input.get(*in_pos..*in_pos + take) else {
                return Err(core.fail_truncated());
            };
            if let Err(error) = sink.write_literals(chunk) {
                return Err(core.latch(error));
            }
            *in_pos += take;
            core.bits_used += 8 * take as u64;
            core.stored_remaining -= take as u16;
            core.total_out += take as u64;
            Ok(Step::Continue)
        }

        InflateState::TableSizes => {
            let Some(value) = core.take_bits(input, in_pos, 14, flush)? else {
                return Ok(Step::Stop(InflateStatus::NeedInput));
            };
            core.hlit = (value & 0x1F) as u16 + 257;
            core.hdist = ((value >> 5) & 0x1F) as u16 + 1;
            core.hclen = ((value >> 10) & 0x0F) as u8 + 4;
            core.codelen_lengths = [0; 19];
            core.codelen_index = 0;
            core.lengths_filled = 0;
            core.state = InflateState::CodeLenLengths;
            Ok(Step::Continue)
        }

        InflateState::CodeLenLengths => {
            while core.codelen_index < core.hclen {
                let Some(bits) = core.take_bits(input, in_pos, 3, flush)? else {
                    return Ok(Step::Stop(InflateStatus::NeedInput));
                };
                let order = CODE_LENGTH_ORDER
                    .get(core.codelen_index as usize)
                    .copied()
                    .unwrap_or(0);
                if let Some(slot) = core.codelen_lengths.get_mut(order) {
                    *slot = bits as u8;
                }
                core.codelen_index += 1;
            }
            // RFC 1951 §3.2.7 requires the 19-symbol alphabet to be a
            // complete code; zlib rejects an incomplete one, and so do we.
            let built = trees
                .codelen
                .rebuild_from_code_length_code(&core.codelen_lengths);
            if let Err(error) = built {
                return Err(core.latch(error));
            }
            core.state = InflateState::CodeLens;
            Ok(Step::Continue)
        }

        InflateState::CodeLens => {
            let total = core.hlit as usize + core.hdist as usize;
            loop {
                if core.lengths_filled as usize >= total {
                    break;
                }
                let peeked = core.peek_symbol(&trees.codelen, input, in_pos, flush)?;
                let Some((code, bits)) = peeked else {
                    return Ok(Step::Stop(InflateStatus::NeedInput));
                };
                core.cache.consume(bits);
                match code {
                    0..=15 => {
                        let index = core.lengths_filled as usize;
                        if let Some(slot) = core.lengths.get_mut(index) {
                            *slot = code as u8;
                        }
                        core.lengths_filled += 1;
                    }
                    16 => {
                        if core.lengths_filled == 0 {
                            return Err(core.fail_corrupted("Code 16 at start of lengths"));
                        }
                        core.repeat_symbol = 16;
                        core.state = InflateState::CodeLensRepeat;
                        return Ok(Step::Continue);
                    }
                    17 | 18 => {
                        core.repeat_symbol = code as u8;
                        core.state = InflateState::CodeLensRepeat;
                        return Ok(Step::Continue);
                    }
                    _ => return Err(core.fail_invalid_huffman()),
                }
            }

            let split = core.hlit as usize;
            let built = match core.lengths.get(..total) {
                Some(all) => {
                    let (litlen_lengths, dist_lengths) = all.split_at(split);
                    trees
                        .litlen
                        .rebuild(litlen_lengths)
                        .and_then(|()| trees.dist.rebuild(dist_lengths))
                }
                None => Err(OxiArcError::invalid_header("Invalid dynamic header sizes")),
            };
            if let Err(error) = built {
                return Err(core.latch(error));
            }
            trees.which = ActiveTrees::Dynamic;
            core.state = InflateState::Symbol;
            Ok(Step::Continue)
        }

        InflateState::CodeLensRepeat => {
            let (extra_bits, base) = match core.repeat_symbol {
                16 => (2u8, 3usize),
                17 => (3u8, 3usize),
                _ => (7u8, 11usize),
            };
            let Some(value) = core.take_bits(input, in_pos, extra_bits, flush)? else {
                return Ok(Step::Stop(InflateStatus::NeedInput));
            };
            let repeat = value as usize + base;
            let filled = core.lengths_filled as usize;
            let total = core.hlit as usize + core.hdist as usize;
            if filled + repeat > total {
                return Err(core.fail_corrupted("Code length overflow"));
            }
            let value = if core.repeat_symbol == 16 {
                core.lengths.get(filled - 1).copied().unwrap_or(0)
            } else {
                0
            };
            if let Some(run) = core.lengths.get_mut(filled..filled + repeat) {
                run.fill(value);
            }
            core.lengths_filled += repeat as u16;
            core.state = InflateState::CodeLens;
            Ok(Step::Continue)
        }

        InflateState::Symbol => {
            let (litlen, _) = trees.active();
            let peeked = core.peek_entry(litlen, input, in_pos, flush)?;
            let Some(entry) = peeked else {
                return Ok(Step::Stop(InflateStatus::NeedInput));
            };
            let bits = decode_table::entry_len(entry);
            if entry & decode_table::LITERAL != 0 {
                // Only a literal needs room, so the space check happens
                // *after* the peek and *before* the consume: an
                // exactly-sized output buffer must still see the
                // end-of-block symbol and report `StreamEnd`.
                if sink.space() == 0 {
                    return Ok(Step::Stop(InflateStatus::NeedOutput));
                }
                if LIMITED {
                    if let Err(stop) = limit_stop(core, sink, core.total_in + *in_pos as u64)? {
                        return Ok(stop);
                    }
                }
                core.cache.consume(bits);
                let byte = decode_table::entry_payload(entry) as u8;
                if let Err(error) = sink.write_literal(byte) {
                    return Err(core.latch(error));
                }
                core.total_out += 1;
                return Ok(Step::Continue);
            }
            core.cache.consume(bits);
            if entry & decode_table::END_OF_BLOCK != 0 {
                core.finish_block();
                return Ok(Step::Continue);
            }
            if entry & decode_table::INVALID != 0 {
                let code = decode_table::entry_payload(entry);
                return Err(core.fail_corrupted(format!("Invalid literal/length code: {}", code)));
            }
            core.pending_length_base = decode_table::entry_payload(entry) as u16;
            core.pending_length_extra = decode_table::entry_extra(entry);
            core.state = InflateState::LengthExtra;
            Ok(Step::Continue)
        }

        InflateState::LengthExtra => {
            let extra_bits = core.pending_length_extra;
            let Some(extra) = core.take_bits(input, in_pos, extra_bits, flush)? else {
                return Ok(Step::Stop(InflateStatus::NeedInput));
            };
            core.pending_length = core.pending_length_base + extra as u16;
            core.state = InflateState::DistSymbol;
            Ok(Step::Continue)
        }

        InflateState::DistSymbol => {
            let (_, dist) = trees.active();
            let peeked = core.peek_entry(dist, input, in_pos, flush)?;
            let Some(entry) = peeked else {
                return Ok(Step::Stop(InflateStatus::NeedInput));
            };
            core.cache.consume(decode_table::entry_len(entry));
            if entry & decode_table::INVALID != 0 {
                let code = decode_table::entry_payload(entry);
                return Err(core.fail_corrupted(format!("Invalid distance code: {}", code)));
            }
            core.pending_dist_base = decode_table::entry_payload(entry) as u16;
            core.pending_dist_extra = decode_table::entry_extra(entry);
            core.state = InflateState::DistExtra;
            Ok(Step::Continue)
        }

        InflateState::DistExtra => {
            let extra_bits = core.pending_dist_extra;
            let Some(extra) = core.take_bits(input, in_pos, extra_bits, flush)? else {
                return Ok(Step::Stop(InflateStatus::NeedInput));
            };
            core.match_distance = core.pending_dist_base as usize + extra as usize;
            core.match_remaining = core.pending_length as usize;
            core.state = InflateState::CopyMatch;
            Ok(Step::Continue)
        }

        InflateState::CopyMatch => {
            if core.match_remaining == 0 {
                core.state = InflateState::Symbol;
                return Ok(Step::Continue);
            }
            let history = sink.history_len();
            if core.match_distance == 0 || core.match_distance > history {
                let distance = core.match_distance;
                return Err(core.latch(OxiArcError::invalid_distance(distance, history)));
            }
            if sink.space() == 0 {
                return Ok(Step::Stop(InflateStatus::NeedOutput));
            }
            let allow = match limit_stop(core, sink, core.total_in + *in_pos as u64)? {
                Ok(allow) => allow,
                Err(stop) => return Ok(stop),
            };
            let take = core
                .match_remaining
                .min(sink.space())
                .min(clamp_usize(allow));
            if take == 0 {
                return Ok(Step::Stop(InflateStatus::NeedOutput));
            }
            if let Err(error) = sink.copy_match(core.match_distance, take) {
                return Err(core.latch(error));
            }
            core.match_remaining -= take;
            core.total_out += take as u64;
            if core.match_remaining == 0 {
                core.state = InflateState::Symbol;
            }
            Ok(Step::Continue)
        }

        InflateState::Done => Ok(Step::Stop(InflateStatus::StreamEnd)),

        InflateState::Bad => {
            let error = match &core.fault {
                Some(fault) => fault.to_error(),
                None => OxiArcError::corrupted(0, "decoder is in an unrecoverable state"),
            };
            Err(error)
        }
    }
}

/// Drive the state machine until it must return.
fn run<S: InflateSink, const LIMITED: bool>(
    core: &mut InflateCore,
    trees: &mut Trees,
    sink: &mut S,
    input: &[u8],
    in_pos: &mut usize,
    flush: FlushMode,
) -> Result<InflateStatus> {
    loop {
        if core.state == InflateState::Symbol {
            let progressed = {
                let (litlen, dist) = trees.active();
                fast::symbols::<S, LIMITED>(core, litlen, dist, sink, input, in_pos)?
            };
            if progressed {
                continue;
            }
        }
        match step_state::<S, LIMITED>(core, trees, sink, input, in_pos, flush)? {
            Step::Continue => continue,
            Step::Stop(status) => return Ok(status),
        }
    }
}

/// Pick the limited or unlimited instantiation once per call.
pub(crate) fn drive<S: InflateSink>(
    core: &mut InflateCore,
    trees: &mut Trees,
    sink: &mut S,
    input: &[u8],
    in_pos: &mut usize,
    flush: FlushMode,
) -> Result<InflateStatus> {
    if core.max_output.is_some() || core.ratio_guard.is_some() {
        run::<S, true>(core, trees, sink, input, in_pos, flush)
    } else {
        run::<S, false>(core, trees, sink, input, in_pos, flush)
    }
}
