//! LZMA compression.
//!
//! This module implements LZMA compression with both greedy and optimal parsing.
//!
//! ## Compression Levels
//!
//! - Levels 0-7: Greedy matching with varying chain depths
//! - Level 8: Optimal parsing with moderate look-ahead
//! - Level 9: Optimal parsing with extended look-ahead
//! - Level 10: Ultra optimal parsing with maximum look-ahead

use crate::LzmaLevel;
use crate::match_finder::{Bt4MatchFinder, HashChainMatchFinder, MatchFinder};
use crate::model::{
    DIST_ALIGN_BITS, END_POS_MODEL_INDEX, LEN_HIGH_BITS, LEN_LOW_BITS, LEN_MID_BITS, LengthModel,
    LzmaModel, LzmaProperties, MATCH_LEN_MIN, State,
};
use crate::optimal::{MatchType, OptimalParser, ProbabilityModels};
use crate::range_coder::RangeEncoder;
use oxiarc_core::cancel::CancellationToken;
use oxiarc_core::error::Result;
use oxiarc_core::progress::ProgressHandle;
use std::collections::VecDeque;

/// Granularity for progress reporting and cancellation checks (bytes).
const PROGRESS_GRANULARITY: u64 = 4096;

/// Maximum match length for fast mode.
const MATCH_LEN_MAX: usize = 273;

/// Maximum chain depth per compression level.
const CHAIN_DEPTH: [usize; 11] = [
    0,    // Level 0: No search (stored mode)
    4,    // Level 1: Very fast
    8,    // Level 2: Fast
    16,   // Level 3: Fast
    32,   // Level 4: Normal
    64,   // Level 5: Normal
    128,  // Level 6: Normal (default)
    256,  // Level 7: Maximum
    512,  // Level 8: High (optimal parsing)
    1024, // Level 9: Best (optimal parsing)
    2048, // Level 10: Ultra (maximum optimal parsing)
];

/// Encode a bit tree.
fn encode_bit_tree(rc: &mut RangeEncoder, probs: &mut [u16], num_bits: u32, value: u32) {
    let mut m = 1usize;

    for i in (0..num_bits).rev() {
        let bit = (value >> i) & 1;
        rc.encode_bit(&mut probs[m], bit);
        m = (m << 1) | bit as usize;
    }
}

/// Encode a length.
fn encode_length(rc: &mut RangeEncoder, len_model: &mut LengthModel, len: u32, pos_state: usize) {
    let len = len - MATCH_LEN_MIN as u32;

    if len < (1 << LEN_LOW_BITS) {
        rc.encode_bit(&mut len_model.choice, 0);
        encode_bit_tree(rc, &mut len_model.low[pos_state], LEN_LOW_BITS, len);
    } else if len < (1 << LEN_LOW_BITS) + (1 << LEN_MID_BITS) {
        rc.encode_bit(&mut len_model.choice, 1);
        rc.encode_bit(&mut len_model.choice2, 0);
        encode_bit_tree(
            rc,
            &mut len_model.mid[pos_state],
            LEN_MID_BITS,
            len - (1 << LEN_LOW_BITS),
        );
    } else {
        rc.encode_bit(&mut len_model.choice, 1);
        rc.encode_bit(&mut len_model.choice2, 1);
        encode_bit_tree(
            rc,
            &mut len_model.high,
            LEN_HIGH_BITS,
            len - (1 << LEN_LOW_BITS) - (1 << LEN_MID_BITS),
        );
    }
}

/// Get distance slot.
fn get_dist_slot(dist: u32) -> u32 {
    if dist < 4 {
        return dist;
    }

    let bits = 32 - dist.leading_zeros();
    ((bits - 1) << 1) | ((dist >> (bits - 2)) & 1)
}

/// LZMA encoder.
pub struct LzmaEncoder {
    /// Range encoder.
    rc: RangeEncoder,
    /// LZMA model.
    model: LzmaModel,
    /// Dictionary size.
    dict_size: usize,
    /// Current state.
    state: State,
    /// Rep distances.
    rep: [u32; 4],
    /// Match finder (hash-chain for levels 0–8, BT4 for level 9).
    match_finder: Box<dyn MatchFinder>,
    /// Maximum chain depth (retained for brute-force DP path).
    chain_depth: usize,
    /// Compression level.
    level: LzmaLevel,
    /// Bytes encoded.
    bytes_encoded: u64,
    /// Optimal parser (used for levels 7-9).
    optimal_parser: Option<OptimalParser>,
    /// Use optimal parsing.
    use_optimal: bool,
    /// Price update counter.
    price_update_counter: u32,
    /// Pending DP decisions for the current block (optimal parsing only).
    dp_pending: VecDeque<(MatchType, usize)>,
    /// Optional progress sink for reporting encoding progress.
    progress: Option<ProgressHandle>,
    /// Optional cancellation token for cooperative cancellation.
    cancel: Option<CancellationToken>,
    /// Bytes processed at the last progress/cancel checkpoint.
    last_checkpoint: u64,
    /// Preset dictionary bytes (virtual prefix; not written to output).
    ///
    /// When set, `compress()` prepends this slice to the input buffer so that
    /// the match finder can discover back-references into the dictionary prefix.
    /// Only the bytes starting after the dict are actually encoded.
    preset_dict: Vec<u8>,
}

impl LzmaEncoder {
    /// Create a new LZMA encoder.
    pub fn new(level: LzmaLevel, dict_size: u32) -> Self {
        let dict_size_usize = dict_size.max(4096) as usize;
        let props = LzmaProperties::default();
        let level_idx = (level.level() as usize).min(10);
        let chain_depth = CHAIN_DEPTH[level_idx];

        // Use optimal parsing for levels 7-9
        let use_optimal = level.level() >= 7;
        let optimal_parser = if use_optimal {
            Some(OptimalParser::with_level(level.level()))
        } else {
            None
        };

        // Choose match finder based on level:
        // level 9 → BT4 (binary tree, superior quality)
        // levels 0-8 → hash chain
        let nice_length = match level.level() {
            0..=3 => 32u32,
            4..=6 => 64u32,
            7..=8 => 128u32,
            _ => 273u32, // level 9
        };
        let match_finder: Box<dyn MatchFinder> = if level.level() >= 9 {
            Box::new(Bt4MatchFinder::new(512, nice_length, dict_size.max(4096)))
        } else {
            Box::new(HashChainMatchFinder::new(
                chain_depth,
                nice_length,
                dict_size.max(4096),
            ))
        };

        Self {
            rc: RangeEncoder::new(),
            model: LzmaModel::new(props),
            dict_size: dict_size_usize,
            state: State::new(),
            rep: [0; 4],
            match_finder,
            chain_depth,
            level,
            bytes_encoded: 0,
            optimal_parser,
            use_optimal,
            price_update_counter: 0,
            dp_pending: VecDeque::new(),
            progress: None,
            cancel: None,
            last_checkpoint: 0,
            preset_dict: Vec::new(),
        }
    }

    /// Construct encoder pre-loaded with a dictionary for improved compression
    /// on data that shares a prefix with the dictionary.
    ///
    /// During `compress()` the dictionary is transparently prepended to the input
    /// buffer. The match finder sees it as a virtual history prefix and may emit
    /// back-references into it; those references will be resolvable by any decoder
    /// initialised with the same dictionary. The dictionary bytes themselves are
    /// **not** written into the output stream.
    ///
    /// If `dict.len()` > `dict_size`, only the last `dict_size` bytes are kept
    /// to stay within the sliding window.
    ///
    /// Mirrors the DEFLATE `Deflater::with_dictionary` pattern.
    pub fn with_dictionary(level: LzmaLevel, dict_size: u32, dict: &[u8]) -> Self {
        let mut enc = Self::new(level, dict_size);
        enc.set_dictionary(dict);
        enc
    }

    /// Preload a preset dictionary.
    ///
    /// The encoder stores the dictionary as a virtual prefix. On the next call to
    /// `compress()` the dictionary is prepended to the input data so that the match
    /// finder can discover back-references into it. The dictionary bytes are **not**
    /// emitted into the compressed output.
    ///
    /// If `dict.len()` > `dict_size`, only the last `dict_size` bytes are kept.
    pub fn set_dictionary(&mut self, dict: &[u8]) {
        if dict.is_empty() {
            self.preset_dict.clear();
            return;
        }
        // Keep at most dict_size bytes at the tail (oldest bytes are beyond the
        // sliding window and can never be referenced anyway).
        let tail_start = dict.len().saturating_sub(self.dict_size);
        self.preset_dict = dict[tail_start..].to_vec();
    }

    /// Attach a progress sink; called for every ~4096 bytes compressed.
    ///
    /// The `on_progress` callback receives `(bytes_consumed, Some(total_input_size))`.
    pub fn with_progress(mut self, handle: ProgressHandle) -> Self {
        self.progress = Some(handle);
        self
    }

    /// Attach a cancellation token; checked every ~4096 bytes compressed.
    ///
    /// If the token is cancelled the encoder returns `Err(OxiArcError::Cancelled)`.
    pub fn with_cancel(mut self, token: CancellationToken) -> Self {
        self.cancel = Some(token);
        self
    }

    /// Check cancellation and emit progress using explicit `processed` and `total` values.
    ///
    /// `processed` is the number of real input bytes consumed so far (excluding any
    /// preset dictionary prefix), `total` is the total real input size. Used instead
    /// of tracking `bytes_encoded` directly so that a preset dictionary does not
    /// inflate the reported progress.
    fn check_progress_and_cancel_with(&mut self, processed: u64, total: u64) -> Result<()> {
        if self.bytes_encoded.saturating_sub(self.last_checkpoint) >= PROGRESS_GRANULARITY {
            self.last_checkpoint = self.bytes_encoded;
            if let Some(ref h) = self.progress {
                h.on_progress(processed, Some(total));
            }
            if let Some(ref t) = self.cancel {
                t.check()?;
            }
        }
        Ok(())
    }

    /// Get properties.
    pub fn properties(&self) -> LzmaProperties {
        self.model.props
    }

    /// Build probability models for optimal parser.
    fn build_probability_models(&self) -> ProbabilityModels<'_> {
        ProbabilityModels {
            is_match: &self.model.is_match,
            is_rep: &self.model.is_rep,
            is_rep0: &self.model.is_rep0,
            is_rep1: &self.model.is_rep1,
            is_rep2: &self.model.is_rep2,
            is_rep0_long: &self.model.is_rep0_long,
            match_len: &self.model.match_len,
            rep_len: &self.model.rep_len,
            dist_slot: &self.model.distance.slot,
            dist_special: &self.model.distance.special,
            dist_align: &self.model.distance.align,
            literal: &self.model.literal.probs,
            num_pos_states: self.model.props.num_pos_states(),
            lc: self.model.props.lc,
            lp: self.model.props.lp,
        }
    }

    /// Check for rep match.
    fn check_rep_match(&self, data: &[u8], pos: usize, rep_idx: usize) -> u32 {
        let dist = self.rep[rep_idx] as usize;

        if dist >= pos {
            return 0;
        }

        let match_pos = pos - dist - 1;
        let mut len = 0usize;
        let max_len = (data.len() - pos).min(MATCH_LEN_MAX);

        while len < max_len && data[pos + len] == data[match_pos + len] {
            len += 1;
        }

        len as u32
    }

    /// Fill `dp_pending` with the optimal decisions for a block starting at `start_pos`
    /// using the full DP forward-pass parser.
    ///
    /// Before running the DP, we pre-populate hash chains for every position in the
    /// block so that the match finder can discover intra-block back-references.
    /// After this call, `dp_pending` contains a sequence of `(MatchType, bytes_consumed)`
    /// pairs that cover up to `MAX_OPT_NUM - 1` bytes from `start_pos`.
    fn fill_dp_block(&mut self, data: &[u8], start_pos: usize) {
        // How big a block can we parse?
        // Use MAX_OPT_NUM - 1 as the hard limit; the parser keeps the last cell for the
        // terminal node, so effective entries are 0..block_len inclusive.
        const MAX_BLOCK: usize = 4095; // MAX_OPT_NUM - 1

        let available = data.len().saturating_sub(start_pos);
        if available == 0 {
            return;
        }
        let block_len = available.min(MAX_BLOCK);

        // Capture everything we need before taking the parser
        let state = self.state;
        let reps = self.rep;
        let dict_size = self.dict_size;
        let chain_depth = self.chain_depth;

        // Update prices periodically
        self.price_update_counter += 1;
        let should_update = self.price_update_counter >= 64;
        if should_update {
            self.price_update_counter = 0;
        }

        // Take parser out to avoid simultaneous borrows
        let mut parser = match self.optimal_parser.take() {
            Some(p) => p,
            None => return,
        };

        // Update prices if needed (before building models)
        if should_update {
            let models = self.build_probability_models();
            parser.update_prices(&models);
        }

        // Build the models snapshot (immutable borrow of self)
        let models = self.build_probability_models();

        // Match finder for the DP: uses a direct brute-force scan that is
        // independent of the hash chains. This ensures intra-block positions
        // are visible to the DP without corrupting the encoder's hash state.
        let look_ahead = parser.look_ahead();

        let find_matches = |pos: usize| -> Vec<(u32, u32)> {
            Self::find_matches_brute(data, pos, look_ahead, dict_size, chain_depth)
        };

        let decisions = parser.parse_block(
            data,
            start_pos,
            block_len,
            &models,
            state,
            &reps,
            find_matches,
        );

        // Put parser back
        self.optimal_parser = Some(parser);

        // Push decisions into pending queue
        for decision in decisions {
            self.dp_pending.push_back(decision);
        }
    }

    /// Brute-force match finder used during DP block filling.
    ///
    /// Unlike the hash-chain based finder, this scans backwards directly through
    /// the data without consulting or modifying the hash tables. This guarantees
    /// that intra-block positions are always searchable, even before hash chains
    /// have been updated for those positions.
    ///
    /// Complexity: O(chain_depth × MATCH_LEN_MAX) per call, which is acceptable
    /// for the DP path because we call it at most `MAX_OPT_NUM - 1` times per block.
    fn find_matches_brute(
        data: &[u8],
        pos: usize,
        max_matches: usize,
        dict_size: usize,
        chain_depth: usize,
    ) -> Vec<(u32, u32)> {
        // Need at least MATCH_LEN_MIN bytes at pos and at least 1 prior byte
        if pos < 1 || pos + MATCH_LEN_MIN > data.len() {
            return Vec::new();
        }

        let max_len = (data.len() - pos).min(MATCH_LEN_MAX);
        // search_depth: how far back we can look (capped by pos so cand >= 0)
        let search_depth = chain_depth.min(pos).min(dict_size);
        if search_depth == 0 {
            return Vec::new();
        }

        let mut matches: Vec<(u32, u32)> = Vec::with_capacity(max_matches.min(16));
        let mut best_len = 0usize;

        // Scan backwards: cand is always < pos, dist is always >= 1
        let mut cand = pos - 1; // safe: pos >= 1
        let mut steps = 0usize;

        loop {
            let dist = pos - cand; // always >= 1

            // Quick 3-byte check
            if data.get(pos) == data.get(cand)
                && data.get(pos + 1) == data.get(cand + 1)
                && data.get(pos + 2) == data.get(cand + 2)
            {
                let mut len = 3usize;
                while len < max_len {
                    match (data.get(pos + len), data.get(cand + len)) {
                        (Some(a), Some(b)) if a == b => len += 1,
                        _ => break,
                    }
                }

                if len > best_len {
                    // dist >= 1, so dist - 1 never underflows
                    matches.push(((dist - 1) as u32, len as u32));
                    best_len = len;

                    if best_len >= max_len {
                        break;
                    }
                }
            }

            steps += 1;
            if cand == 0
                || steps >= chain_depth
                || matches.len() >= max_matches
                || dist >= dict_size
            {
                break;
            }
            cand -= 1;
        }

        matches
    }

    /// Find the optimal sequence using DP-based block parsing (or heuristic fallback).
    fn find_optimal_sequence(
        &mut self,
        data: &[u8],
        start_pos: usize,
    ) -> Option<(bool, usize, u32)> {
        // If the DP pending queue is empty, fill it for a new block
        if self.dp_pending.is_empty() {
            self.fill_dp_block(data, start_pos);
        }

        // Dequeue the next decision
        if let Some((match_type, _consumed)) = self.dp_pending.pop_front() {
            match match_type {
                MatchType::Literal => None,
                MatchType::ShortRep => Some((true, 0, 1)),
                MatchType::RepMatch { rep_idx, len } => Some((true, rep_idx as usize, len)),
                MatchType::Match { dist, len } => Some((false, dist as usize, len)),
            }
        } else {
            // Fall back to heuristic selection if queue is still empty
            self.find_heuristic_match(data, start_pos)
        }
    }

    /// Heuristic match finding (fallback).
    fn find_heuristic_match(&self, data: &[u8], start_pos: usize) -> Option<(bool, usize, u32)> {
        // Get rep matches
        let mut best_rep: Option<(usize, u32)> = None;
        for rep_idx in 0..4 {
            let len = self.check_rep_match(data, start_pos, rep_idx);
            if len >= MATCH_LEN_MIN as u32
                && (best_rep.is_none() || best_rep.is_some_and(|(_, l)| len > l))
            {
                best_rep = Some((rep_idx, len));
            }
        }

        // Use brute-force scan for normal matches (heuristic path doesn't mutate match_finder)
        let matches =
            Self::find_matches_brute(data, start_pos, 32, self.dict_size, self.chain_depth);
        let normal_match = matches.last().copied();

        // Decision logic with price estimation
        match (best_rep, normal_match) {
            (Some((rep_idx, rep_len)), Some((dist, len))) => {
                // Estimate prices
                let rep_price = 4 + (rep_len / 4);
                let normal_price = 10 + (len / 4) + 8;

                if rep_price < normal_price || (rep_len >= len && rep_idx == 0) {
                    Some((true, rep_idx, rep_len))
                } else {
                    Some((false, dist as usize, len))
                }
            }
            (_, Some((dist, len))) if len >= MATCH_LEN_MIN as u32 => {
                Some((false, dist as usize, len))
            }
            (Some((rep_idx, rep_len)), _) if rep_len >= MATCH_LEN_MIN as u32 => {
                Some((true, rep_idx, rep_len))
            }
            _ => None,
        }
    }

    /// Encode a literal byte.
    fn encode_literal(&mut self, byte: u8, prev_byte: u8, match_byte: u8) {
        let lit_state = self.model.literal.get_state(
            self.bytes_encoded,
            prev_byte,
            self.model.props.lc,
            self.model.props.lp,
        );

        if self.state.is_literal() {
            self.encode_literal_normal(lit_state, byte);
        } else {
            self.encode_literal_matched(lit_state, byte, match_byte);
        }
    }

    /// Encode a normal literal.
    fn encode_literal_normal(&mut self, lit_state: usize, byte: u8) {
        let mut symbol = (byte as usize) | 0x100;
        let mut context = 1usize;

        loop {
            let bit = (symbol >> 7) & 1;
            symbol <<= 1;

            self.rc.encode_bit(
                &mut self.model.literal.probs[lit_state][context],
                bit as u32,
            );

            context = (context << 1) | bit;

            if context >= 0x100 {
                break;
            }
        }
    }

    /// Encode a literal with match context.
    fn encode_literal_matched(&mut self, lit_state: usize, byte: u8, match_byte: u8) {
        let mut symbol = (byte as usize) | 0x100;
        let mut match_symbol = (match_byte as usize) << 1;
        let mut context = 1usize;

        loop {
            let match_bit = (match_symbol >> 8) & 1;
            match_symbol <<= 1;

            let bit = (symbol >> 7) & 1;
            symbol <<= 1;

            let prob_idx = 0x100 + (match_bit << 8) + context;
            self.rc.encode_bit(
                &mut self.model.literal.probs[lit_state][prob_idx],
                bit as u32,
            );
            context = (context << 1) | bit;

            if context >= 0x100 {
                break;
            }

            if bit != match_bit {
                // Mismatch, continue without match context
                while context < 0x100 {
                    let bit = (symbol >> 7) & 1;
                    symbol <<= 1;
                    self.rc.encode_bit(
                        &mut self.model.literal.probs[lit_state][context],
                        bit as u32,
                    );
                    context = (context << 1) | bit;
                }
                break;
            }
        }
    }

    /// Encode a distance.
    fn encode_distance(&mut self, dist: u32, len: u32) {
        let len_state = ((len - MATCH_LEN_MIN as u32).min(3)) as usize;

        // Calculate slot
        let slot = get_dist_slot(dist);

        // Encode slot
        encode_bit_tree(
            &mut self.rc,
            &mut self.model.distance.slot[len_state],
            6,
            slot,
        );

        if slot >= 4 {
            let num_direct_bits = (slot >> 1) - 1;
            let base = (2 | (slot & 1)) << num_direct_bits;
            let dist_reduced = dist - base;

            if slot < END_POS_MODEL_INDEX as u32 {
                // Encode with model (reverse bit tree)
                let base_idx = (slot as usize) - (slot as usize >> 1) - 1;

                // Encode reverse bit tree manually since we need flat array indexing
                let mut m = 1usize;
                for i in 0..num_direct_bits {
                    let bit = (dist_reduced >> i) & 1;
                    self.rc
                        .encode_bit(&mut self.model.distance.special[base_idx + m - 1], bit);
                    m = (m << 1) | bit as usize;
                }
            } else {
                // Direct bits + alignment
                let num_align_bits = DIST_ALIGN_BITS;
                let num_direct = num_direct_bits - num_align_bits;

                self.rc
                    .encode_direct_bits(dist_reduced >> num_align_bits, num_direct);
                self.rc.encode_bit_tree_reverse(
                    &mut self.model.distance.align,
                    num_align_bits,
                    dist_reduced & ((1 << num_align_bits) - 1),
                );
            }
        }
    }

    /// Compress data.
    ///
    /// When a preset dictionary has been set via [`set_dictionary`] or
    /// [`with_dictionary`], the dictionary bytes are transparently prepended to
    /// `data` so that the match finder can discover back-references into them.
    /// The dictionary bytes are **not** written into the returned byte stream.
    ///
    /// [`set_dictionary`]: Self::set_dictionary
    /// [`with_dictionary`]: Self::with_dictionary
    pub fn compress(mut self, data: &[u8]) -> Result<Vec<u8>> {
        // When a preset dictionary is active, build a combined buffer and remember
        // the offset at which real data starts. The encoder loop runs over `buf`
        // starting at `data_start` so that the match finder already has the dict
        // content in its hash tables when it processes the first input byte.
        let (buf, data_start): (std::borrow::Cow<'_, [u8]>, usize) = if self.preset_dict.is_empty()
        {
            (std::borrow::Cow::Borrowed(data), 0)
        } else {
            let mut combined = Vec::with_capacity(self.preset_dict.len() + data.len());
            combined.extend_from_slice(&self.preset_dict);
            combined.extend_from_slice(data);
            let start = self.preset_dict.len();
            (std::borrow::Cow::Owned(combined), start)
        };
        let buf: &[u8] = &buf;

        // `total` reflects only the real input bytes (for progress reporting).
        let total = data.len() as u64;

        // Initialize prices for optimal parser
        if self.use_optimal {
            // Take the parser out temporarily to avoid borrow conflict
            if let Some(mut parser) = self.optimal_parser.take() {
                let models = self.build_probability_models();
                parser.update_prices(&models);
                self.optimal_parser = Some(parser);
            }
        }

        // Check cancellation before starting (pre-compress check)
        if let Some(ref t) = self.cancel {
            t.check()?;
        }

        // When a dict is present, fast-forward the match finder through the dict
        // bytes so that hash chains are populated before encoding starts. We do
        // this here (rather than in set_dictionary) because the match finder
        // needs to see the *combined* buffer to correctly set up positions.
        if data_start > 0 {
            for pos in 0..data_start {
                self.match_finder.skip(buf, pos);
            }
            // Set bytes_encoded so that `check_rep_match` uses correct base
            // (rep distances are relative to current position in `buf`).
            self.bytes_encoded = data_start as u64;
        }

        let mut i = data_start;

        while i < buf.len() {
            let pos_state = (self.bytes_encoded as usize) & (self.model.props.num_pos_states() - 1);
            let state_idx = self.state.value();

            // Determine match using optimal or greedy parsing
            let (use_match, match_info) = if self.use_optimal {
                // Use optimal parsing
                if let Some(result) = self.find_optimal_sequence(buf, i) {
                    (true, Some(result))
                } else {
                    (false, None)
                }
            } else {
                // Use greedy parsing
                // Check for rep matches first
                let mut best_rep: Option<(usize, u32)> = None;
                for rep_idx in 0..4 {
                    let len = self.check_rep_match(buf, i, rep_idx);
                    if len >= MATCH_LEN_MIN as u32 && best_rep.is_none_or(|(_, l)| len > l) {
                        best_rep = Some((rep_idx, len));
                    }
                }

                // Check for normal match via the match finder trait.
                // find_matches also inserts the position, so we must NOT
                // call skip/insert separately for the literal/match head.
                let normal_matches = self.match_finder.find_matches(buf, i);
                let normal_match = normal_matches.last().copied();

                // Decide what to encode
                match (best_rep, normal_match) {
                    (Some((rep_idx, rep_len)), Some((_dist, len)))
                        if rep_len >= len || (rep_len >= 3 && rep_idx == 0) =>
                    {
                        // Use rep match
                        (true, Some((true, rep_idx, rep_len)))
                    }
                    (_, Some((dist, len))) if len >= MATCH_LEN_MIN as u32 => {
                        // Use normal match
                        (true, Some((false, dist as usize, len)))
                    }
                    (Some((rep_idx, rep_len)), _) if rep_len >= MATCH_LEN_MIN as u32 => {
                        // Use rep match
                        (true, Some((true, rep_idx, rep_len)))
                    }
                    _ => (false, None),
                }
            };

            if !use_match {
                // Encode literal
                self.rc
                    .encode_bit(&mut self.model.is_match[state_idx][pos_state], 0);

                let prev_byte = if i > 0 { buf[i - 1] } else { 0 };
                let match_byte = if !self.state.is_literal() && (self.rep[0] as usize) < i {
                    buf[i - self.rep[0] as usize - 1]
                } else {
                    0
                };

                self.encode_literal(buf[i], prev_byte, match_byte);
                self.state.update_literal();
                self.bytes_encoded += 1;

                // Greedy path: position was already inserted by find_matches above.
                // Optimal path: DP uses brute-force internally, so we skip here.
                if self.use_optimal {
                    self.match_finder.skip(buf, i);
                }

                i += 1;

                // Progress is measured in bytes of *real* input consumed.
                let real_consumed = self.bytes_encoded.saturating_sub(data_start as u64);
                self.check_progress_and_cancel_with(real_consumed, total)?;
            } else if let Some((is_rep, idx_or_dist, len)) = match_info {
                self.rc
                    .encode_bit(&mut self.model.is_match[state_idx][pos_state], 1);

                if is_rep {
                    // Rep match
                    self.rc.encode_bit(&mut self.model.is_rep[state_idx], 1);

                    let rep_idx = idx_or_dist;
                    if rep_idx == 0 {
                        self.rc.encode_bit(&mut self.model.is_rep0[state_idx], 0);

                        if len == 1 {
                            self.rc
                                .encode_bit(&mut self.model.is_rep0_long[state_idx][pos_state], 0);
                            self.state.update_short_rep();
                        } else {
                            self.rc
                                .encode_bit(&mut self.model.is_rep0_long[state_idx][pos_state], 1);
                            encode_length(&mut self.rc, &mut self.model.rep_len, len, pos_state);
                            self.state.update_long_rep();
                        }
                    } else {
                        self.rc.encode_bit(&mut self.model.is_rep0[state_idx], 1);

                        if rep_idx == 1 {
                            self.rc.encode_bit(&mut self.model.is_rep1[state_idx], 0);
                        } else {
                            self.rc.encode_bit(&mut self.model.is_rep1[state_idx], 1);
                            if rep_idx == 2 {
                                self.rc.encode_bit(&mut self.model.is_rep2[state_idx], 0);
                            } else {
                                self.rc.encode_bit(&mut self.model.is_rep2[state_idx], 1);
                            }
                        }

                        // Shift rep distances
                        let dist = self.rep[rep_idx];
                        for j in (1..=rep_idx).rev() {
                            self.rep[j] = self.rep[j - 1];
                        }
                        self.rep[0] = dist;

                        encode_length(&mut self.rc, &mut self.model.rep_len, len, pos_state);
                        self.state.update_long_rep();
                    }
                } else {
                    // Normal match
                    self.rc.encode_bit(&mut self.model.is_rep[state_idx], 0);

                    let dist = idx_or_dist as u32;
                    encode_length(&mut self.rc, &mut self.model.match_len, len, pos_state);
                    self.encode_distance(dist, len);

                    // Shift rep distances
                    self.rep[3] = self.rep[2];
                    self.rep[2] = self.rep[1];
                    self.rep[1] = self.rep[0];
                    self.rep[0] = dist;

                    self.state.update_match();
                }

                self.bytes_encoded += len as u64;

                // Keep the match finder in sync with stream position:
                // - Greedy path: position i was already inserted by find_matches;
                //   skip positions i+1 .. i+len-1.
                // - Optimal path: no find_matches was called; skip all i .. i+len-1.
                let skip_start = if self.use_optimal { 0 } else { 1 };
                for j in skip_start..len as usize {
                    self.match_finder.skip(buf, i + j);
                }

                i += len as usize;

                let real_consumed = self.bytes_encoded.saturating_sub(data_start as u64);
                self.check_progress_and_cancel_with(real_consumed, total)?;
            }
        }

        // Final progress notification
        if let Some(ref h) = self.progress {
            let real_consumed = self.bytes_encoded.saturating_sub(data_start as u64);
            h.on_progress(real_consumed, Some(total));
        }

        // Write end marker
        let pos_state = (self.bytes_encoded as usize) & (self.model.props.num_pos_states() - 1);
        let state_idx = self.state.value();

        self.rc
            .encode_bit(&mut self.model.is_match[state_idx][pos_state], 1);
        self.rc.encode_bit(&mut self.model.is_rep[state_idx], 0);

        // Encode minimum length
        encode_length(
            &mut self.rc,
            &mut self.model.match_len,
            MATCH_LEN_MIN as u32,
            pos_state,
        );

        // Encode end marker distance
        self.encode_distance(0xFFFF_FFFF, MATCH_LEN_MIN as u32);

        Ok(self.rc.finish())
    }

    /// Get the dictionary size.
    pub fn dict_size(&self) -> u32 {
        self.dict_size as u32
    }

    /// Get the compression level.
    pub fn level(&self) -> LzmaLevel {
        self.level
    }
}

/// Compress data using LZMA.
pub fn compress(data: &[u8], level: LzmaLevel) -> Result<Vec<u8>> {
    let dict_size = match level.level() {
        0 => 1 << 16,     // 64 KB
        1..=3 => 1 << 20, // 1 MB
        4..=6 => 1 << 22, // 4 MB
        7..=8 => 1 << 24, // 16 MB
        _ => 1 << 25,     // 32 MB for levels 9-10
    };

    let encoder = LzmaEncoder::new(level, dict_size);
    let props = encoder.properties();

    // Build output with header
    let mut output = Vec::new();

    // Properties byte
    output.push(props.to_byte());

    // Dictionary size (4 bytes, little-endian)
    output.extend_from_slice(&dict_size.to_le_bytes());

    // Uncompressed size (8 bytes, little-endian)
    output.extend_from_slice(&(data.len() as u64).to_le_bytes());

    // Compressed data
    let compressed = encoder.compress(data)?;
    output.extend_from_slice(&compressed);

    Ok(output)
}

/// Compress data without header.
pub fn compress_raw(data: &[u8], level: LzmaLevel, dict_size: u32) -> Result<Vec<u8>> {
    let encoder = LzmaEncoder::new(level, dict_size);
    encoder.compress(data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::match_finder::HashChainMatchFinder;

    #[test]
    fn test_encoder_creation() {
        let encoder = LzmaEncoder::new(LzmaLevel::DEFAULT, 1 << 20);
        assert_eq!(encoder.dict_size(), 1 << 20);
    }

    #[test]
    fn test_dist_slot() {
        assert_eq!(get_dist_slot(0), 0);
        assert_eq!(get_dist_slot(1), 1);
        assert_eq!(get_dist_slot(2), 2);
        assert_eq!(get_dist_slot(3), 3);
        assert_eq!(get_dist_slot(4), 4);
    }

    #[test]
    fn test_hash3_in_match_finder() {
        // hash3 now lives in HashChainMatchFinder
        let data1 = [0u8, 0, 0];
        let data2 = [1u8, 2, 3];

        let h1 = HashChainMatchFinder::hash3_fnv(&data1);
        let h2 = HashChainMatchFinder::hash3_fnv(&data2);

        assert_ne!(h1, h2);
        assert!(h1 < (1 << 16));
        assert!(h2 < (1 << 16));
    }

    #[test]
    fn test_chain_depth_by_level() {
        // Level 1 should have depth 4
        let enc1 = LzmaEncoder::new(LzmaLevel::new(1), 1 << 16);
        assert_eq!(enc1.chain_depth, 4);

        // Level 6 should have depth 128
        let enc6 = LzmaEncoder::new(LzmaLevel::new(6), 1 << 16);
        assert_eq!(enc6.chain_depth, 128);

        // Level 9 should have depth 1024 (stored for brute-force DP path)
        let enc9 = LzmaEncoder::new(LzmaLevel::new(9), 1 << 16);
        assert_eq!(enc9.chain_depth, 1024);

        // Level 10 is clamped to 9 by LzmaLevel::new(), so chain_depth is 1024
        let enc10 = LzmaEncoder::new(LzmaLevel::new(10), 1 << 16);
        assert_eq!(enc10.chain_depth, 1024);
    }

    #[test]
    fn test_optimal_parser_level_8() {
        let encoder = LzmaEncoder::new(LzmaLevel::new(8), 1 << 20);
        assert!(encoder.use_optimal);
        assert!(encoder.optimal_parser.is_some());
        assert_eq!(encoder.optimal_parser.as_ref().map(|p| p.level()), Some(8));
    }

    #[test]
    fn test_optimal_parser_level_9() {
        let encoder = LzmaEncoder::new(LzmaLevel::new(9), 1 << 20);
        assert!(encoder.use_optimal);
        assert!(encoder.optimal_parser.is_some());
        assert_eq!(encoder.optimal_parser.as_ref().map(|p| p.level()), Some(9));
    }

    #[test]
    fn test_optimal_parser_level_10() {
        // Level 10 is clamped to 9 by LzmaLevel::new()
        let encoder = LzmaEncoder::new(LzmaLevel::new(10), 1 << 20);
        assert!(encoder.use_optimal);
        assert!(encoder.optimal_parser.is_some());
        // Parser level is 9 (clamped from 10)
        assert_eq!(encoder.optimal_parser.as_ref().map(|p| p.level()), Some(9));
    }

    #[test]
    fn test_no_optimal_parser_for_low_levels() {
        let encoder = LzmaEncoder::new(LzmaLevel::new(6), 1 << 20);
        assert!(!encoder.use_optimal);
        assert!(encoder.optimal_parser.is_none());
    }

    #[test]
    fn test_match_candidates_via_brute_force() {
        // At position 0 with no history, brute-force should return no matches
        let data = b"ABCDEFGHIJ";
        let matches = LzmaEncoder::find_matches_brute(data, 0, 32, 1 << 20, 512);
        assert!(matches.is_empty());
    }
}
