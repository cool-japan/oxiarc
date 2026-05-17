//! Match finder implementations for LZMA compression.
//!
//! This module provides two match-finding strategies:
//! - [`HashChainMatchFinder`]: hash-chain based, used for levels 0–8
//! - [`Bt4MatchFinder`]: binary-tree with 4-byte hash (BT4), used for level 9
//!
//! Both implement the [`MatchFinder`] trait, allowing the encoder to select
//! the strategy via trait dispatch.

/// Minimum match length for LZMA.
pub const MIN_MATCH_LEN: u32 = 2;

/// Maximum match length for LZMA (MATCH_LEN_MAX from LZMA SDK).
pub const MAX_MATCH_LEN: u32 = 273;

/// Hash table size for the hash-chain finder (64 K entries).
const HC_HASH_SIZE: usize = 1 << 16;

/// FNV-1a 32-bit offset basis.
const FNV_OFFSET: u32 = 2_166_136_261;
/// FNV-1a 32-bit prime.
const FNV_PRIME: u32 = 16_777_619;

// BT4 hash sizes (power-of-two so we can mask)
const BT4_HASH2_SIZE: usize = 1 << 16; // 2-byte hash
const BT4_HASH3_SIZE: usize = 1 << 18; // 3-byte hash
const BT4_HASH4_SIZE: usize = 1 << 20; // 4-byte hash

/// Trait abstraction over different match-finding strategies.
///
/// All methods operate on the full input `buf` slice and an absolute `pos`
/// within it. The finder owns internal state (hash tables, tree nodes, etc.)
/// that is separate from the encoder's state.
pub trait MatchFinder {
    /// Reset internal state for a new stream with the given dictionary size.
    fn reset(&mut self, dict_size: u32);

    /// Find all useful match candidates at `pos` in `buf`.
    ///
    /// Returns `(dist, len)` pairs sorted by **strictly increasing length**.
    /// `dist` is 0-based (distance 1 = the immediately preceding byte has
    /// `dist == 0`).
    ///
    /// Also internally advances the match finder (inserts `pos` into the
    /// data structure) so the caller does **not** need to call `skip`.
    fn find_matches(&mut self, buf: &[u8], pos: usize) -> Vec<(u32, u32)>;

    /// Advance the internal position without returning match candidates.
    ///
    /// Used when the encoder already knows it will emit a long match and
    /// wants to keep the data structure consistent without paying the full
    /// `find_matches` cost.
    fn skip(&mut self, buf: &[u8], pos: usize);
}

// ---------------------------------------------------------------------------
// HashChainMatchFinder
// ---------------------------------------------------------------------------

/// Hash-chain based match finder (used for compression levels 0–8).
///
/// Maintains a 64 K hash table pointing to the most recent position for each
/// 3-byte hash, with a singly-linked chain connecting all prior positions
/// that share the same hash.
pub struct HashChainMatchFinder {
    /// Head of each hash chain (indexed by 3-byte hash).
    hash_head: Vec<u32>,
    /// Chain links: `hash_chain[pos]` is the previous position with the same hash.
    hash_chain: Vec<u32>,
    /// Maximum chain-walk depth (proportional to compression level).
    chain_depth: usize,
    /// Nice-length threshold: stop searching early once a match this long is found.
    nice_length: u32,
    /// Dictionary size limit.
    dict_size: usize,
}

impl HashChainMatchFinder {
    /// Create a new hash-chain match finder.
    ///
    /// * `chain_depth` – maximum number of chain links to follow per search.
    /// * `nice_length` – early-exit threshold when a match of this length is found.
    /// * `dict_size`   – maximum look-back distance in bytes.
    pub fn new(chain_depth: usize, nice_length: u32, dict_size: u32) -> Self {
        let dict_size = (dict_size as usize).max(4096);
        Self {
            hash_head: vec![u32::MAX; HC_HASH_SIZE],
            hash_chain: Vec::new(),
            chain_depth,
            nice_length,
            dict_size,
        }
    }

    /// FNV-1a 3-byte hash, masked to `HC_HASH_SIZE - 1`.
    pub fn hash3_fnv(data: &[u8]) -> usize {
        if data.len() < 3 {
            return 0;
        }
        let mut h = FNV_OFFSET;
        h ^= data[0] as u32;
        h = h.wrapping_mul(FNV_PRIME);
        h ^= data[1] as u32;
        h = h.wrapping_mul(FNV_PRIME);
        h ^= data[2] as u32;
        h = h.wrapping_mul(FNV_PRIME);
        (h as usize) & (HC_HASH_SIZE - 1)
    }

    /// Insert `pos` into the hash chain (update both head and link arrays).
    fn insert(&mut self, data: &[u8], pos: usize) {
        if pos + 3 > data.len() {
            return;
        }
        if pos >= self.hash_chain.len() {
            self.hash_chain.resize(pos + 1, u32::MAX);
        }
        let h = Self::hash3_fnv(&data[pos..]);
        self.hash_chain[pos] = self.hash_head[h];
        self.hash_head[h] = pos as u32;
    }

    /// Walk the hash chain at `pos` and collect strictly-longer matches.
    fn find_all_inner(&self, data: &[u8], pos: usize, max_matches: usize) -> Vec<(u32, u32)> {
        if pos + MIN_MATCH_LEN as usize > data.len() {
            return Vec::new();
        }

        let h = Self::hash3_fnv(&data[pos..]);
        let mut match_pos = self.hash_head[h] as usize;

        if match_pos == u32::MAX as usize {
            return Vec::new();
        }

        let max_len = (data.len() - pos).min(MAX_MATCH_LEN as usize);
        let mut matches: Vec<(u32, u32)> = Vec::with_capacity(max_matches.min(16));
        let mut best_len: usize = MIN_MATCH_LEN as usize - 1;
        let mut chain_count: usize = 0;

        while match_pos < pos && chain_count < self.chain_depth && matches.len() < max_matches {
            let dist = pos - match_pos;
            if dist > self.dict_size {
                break;
            }

            // Quick 2-byte prefix check before full compare
            if data[pos] == data[match_pos] && data[pos + 1] == data[match_pos + 1] {
                let mut len = 2usize;
                while len < max_len && data[pos + len] == data[match_pos + len] {
                    len += 1;
                }

                if len > best_len {
                    // dist >= 1 so dist - 1 never underflows
                    matches.push(((dist - 1) as u32, len as u32));
                    best_len = len;

                    if len >= max_len || len >= self.nice_length as usize {
                        break;
                    }
                }
            }

            // Follow chain
            if match_pos < self.hash_chain.len() {
                let next = self.hash_chain[match_pos] as usize;
                if next >= match_pos || next == u32::MAX as usize {
                    break;
                }
                match_pos = next;
            } else {
                break;
            }

            chain_count += 1;
        }

        matches
    }
}

impl MatchFinder for HashChainMatchFinder {
    fn reset(&mut self, dict_size: u32) {
        self.dict_size = (dict_size as usize).max(4096);
        self.hash_head.fill(u32::MAX);
        self.hash_chain.clear();
    }

    fn find_matches(&mut self, buf: &[u8], pos: usize) -> Vec<(u32, u32)> {
        let result = self.find_all_inner(buf, pos, 64);
        self.insert(buf, pos);
        result
    }

    fn skip(&mut self, buf: &[u8], pos: usize) {
        self.insert(buf, pos);
    }
}

// ---------------------------------------------------------------------------
// Bt4MatchFinder
// ---------------------------------------------------------------------------

/// Binary-tree match finder with 4-byte hash (BT4), used for compression level 9.
///
/// Maintains a binary search tree stored implicitly in the `son` array. The
/// tree is keyed by the byte suffixes the positions represent. During a
/// `find_matches` call the tree is traversed from the root, pruning half the
/// search space at each comparison, and simultaneously updated so that
/// `pos` is inserted into the correct position.
///
/// This is the algorithm used by the LZMA SDK (`LzFind.c`) and 7-Zip.
pub struct Bt4MatchFinder {
    /// 2-byte hash table, size 2^16.
    hash2: Vec<u32>,
    /// 3-byte hash table, size 2^18.
    hash3: Vec<u32>,
    /// 4-byte hash table (primary), size 2^20.
    hash4: Vec<u32>,
    /// Binary-tree node array.
    /// `son[2*i]`   = left child of node at cyclic index `i`.
    /// `son[2*i+1]` = right child of node at cyclic index `i`.
    son: Vec<u32>,
    /// Current cyclic buffer position (wraps at `cyclic_buffer_size`).
    cyclic_buffer_pos: usize,
    /// Cyclic buffer size = dict_size + 1.
    cyclic_buffer_size: usize,
    /// Maximum tree-walk depth per search call.
    cut_value: u32,
    /// Nice-length early-exit threshold.
    nice_length: u32,
    /// Absolute stream position of the *next* byte to process (1-based so 0
    /// is reserved as "no entry" sentinel in the hash tables).
    pos: u32,
    /// Dictionary size.
    dict_size: u32,
}

impl Bt4MatchFinder {
    /// Create a new BT4 match finder.
    ///
    /// * `cut_value`   – maximum BT traversal depth (512 for level 9).
    /// * `nice_length` – match length that causes immediate acceptance (273 for level 9).
    /// * `dict_size`   – sliding window / dictionary size.
    pub fn new(cut_value: u32, nice_length: u32, dict_size: u32) -> Self {
        let dict_size = dict_size.max(4096);
        let cyclic_buffer_size = dict_size as usize + 1;
        Self {
            hash2: vec![0u32; BT4_HASH2_SIZE],
            hash3: vec![0u32; BT4_HASH3_SIZE],
            hash4: vec![0u32; BT4_HASH4_SIZE],
            son: vec![0u32; cyclic_buffer_size * 2],
            cyclic_buffer_pos: 0,
            cyclic_buffer_size,
            cut_value,
            nice_length,
            pos: 1, // start at 1 so that 0 means "no match" in hash tables
            dict_size,
        }
    }

    // ------------------------------------------------------------------
    // Hash functions (multiplicative)
    // ------------------------------------------------------------------

    /// 2-byte multiplicative hash → 16-bit index.
    #[inline]
    fn h2(b0: u8, b1: u8) -> usize {
        let v = (b0 as u32).wrapping_mul(1_572_869).wrapping_add(b1 as u32);
        (v as usize) & (BT4_HASH2_SIZE - 1)
    }

    /// 3-byte multiplicative hash → 18-bit index.
    #[inline]
    fn h3(b0: u8, b1: u8, b2: u8) -> usize {
        let v = (b0 as u32)
            .wrapping_mul(1_572_869)
            .wrapping_add(b1 as u32)
            .wrapping_mul(1_572_869)
            .wrapping_add(b2 as u32);
        (v as usize) & (BT4_HASH3_SIZE - 1)
    }

    /// 4-byte multiplicative hash → 20-bit index.
    #[inline]
    fn h4(b0: u8, b1: u8, b2: u8, b3: u8) -> usize {
        let v = (b0 as u32)
            .wrapping_mul(1_572_869)
            .wrapping_add(b1 as u32)
            .wrapping_mul(1_572_869)
            .wrapping_add(b2 as u32)
            .wrapping_mul(1_572_869)
            .wrapping_add(b3 as u32);
        (v as usize) & (BT4_HASH4_SIZE - 1)
    }

    // ------------------------------------------------------------------
    // Core BT4 traversal + insertion
    // ------------------------------------------------------------------

    /// Traverse the binary tree rooted at `cur_match_init`, collecting matches
    /// strictly longer than `min_len`, and simultaneously insert the current
    /// position `pos` (stored as `self.pos`) into the tree.
    ///
    /// The new node occupies cyclic slot `self.cyclic_buffer_pos`; its left
    /// child pointer lives at `son[slot*2]` and right at `son[slot*2+1]`.
    fn bt_find_insert(
        &mut self,
        buf: &[u8],
        pos: usize,
        cur_match_init: u32,
        min_len: u32,
        out: &mut Vec<(u32, u32)>,
    ) {
        let cyclic_pos = self.cyclic_buffer_pos;
        // These are the index positions in `son[]` where we will write
        // the left and right children of the new node.
        let mut left_ptr = cyclic_pos * 2;
        let mut right_ptr = cyclic_pos * 2 + 1;

        let mut cur_match = cur_match_init;
        let mut best_len = min_len;
        let max_len = (buf.len() - pos).min(MAX_MATCH_LEN as usize) as u32;
        let mut count = self.cut_value;

        loop {
            if cur_match == 0 || count == 0 {
                self.son[left_ptr] = 0;
                self.son[right_ptr] = 0;
                break;
            }
            count -= 1;

            let match_dist = self.pos.wrapping_sub(cur_match);
            if match_dist > self.dict_size || match_dist == 0 {
                self.son[left_ptr] = 0;
                self.son[right_ptr] = 0;
                break;
            }

            // `cur_match` is 1-based; convert to 0-based for both buffer access
            // and cyclic-buffer slot lookup.
            let cm_buf = (cur_match - 1) as usize; // 0-based buffer index
            let cyclic_match = cm_buf % self.cyclic_buffer_size;

            // Find the common prefix length between buf[pos..] and buf[cm_buf..]
            let avail_match = (buf.len() - cm_buf).min(MAX_MATCH_LEN as usize) as u32;
            let compare_len = max_len.min(avail_match);
            let mut len = 0u32;
            while len < compare_len && buf[pos + len as usize] == buf[cm_buf + len as usize] {
                len += 1;
            }

            if len > best_len {
                best_len = len;
                out.push((match_dist - 1, len)); // dist is 0-based
            }

            if len >= max_len || best_len >= self.nice_length {
                // Full match or nice-length early-exit: connect subtree roots
                self.son[left_ptr] = self.son[cyclic_match * 2];
                self.son[right_ptr] = self.son[cyclic_match * 2 + 1];
                break;
            }

            // Decide which sub-tree to descend into by comparing the
            // discriminating byte (the one where the strings first differ).
            if len < compare_len && buf[pos + len as usize] < buf[cm_buf + len as usize] {
                // New position is "smaller": attach cur_match as right ancestor,
                // descend into cur_match's left sub-tree.
                self.son[right_ptr] = cur_match;
                right_ptr = cyclic_match * 2;
                cur_match = self.son[right_ptr];
            } else {
                // New position is "larger": attach cur_match as left ancestor,
                // descend into cur_match's right sub-tree.
                self.son[left_ptr] = cur_match;
                left_ptr = cyclic_match * 2 + 1;
                cur_match = self.son[left_ptr];
            }
        }
    }

    /// Find all matches and insert `pos` into the hash tables and BT.
    fn find_and_insert(&mut self, buf: &[u8], pos: usize) -> Vec<(u32, u32)> {
        // Need at least 2 bytes to find any match
        if pos + 2 > buf.len() {
            self.advance_pos();
            return Vec::new();
        }

        let b0 = buf[pos];
        let b1 = buf[pos + 1];
        let b2 = if pos + 2 < buf.len() { buf[pos + 2] } else { 0 };
        let b3 = if pos + 3 < buf.len() { buf[pos + 3] } else { 0 };

        let idx2 = Self::h2(b0, b1);
        let idx3 = Self::h3(b0, b1, b2);
        let has_h4 = pos + 4 <= buf.len();
        let idx4 = if has_h4 { Self::h4(b0, b1, b2, b3) } else { 0 };

        let cur2 = self.hash2[idx2];
        let cur3 = if pos + 2 < buf.len() {
            self.hash3[idx3]
        } else {
            0
        };
        let cur4 = if has_h4 { self.hash4[idx4] } else { 0 };

        // Update hash tables to point to current position
        let cur_pos = self.pos;
        self.hash2[idx2] = cur_pos;
        if pos + 2 < buf.len() {
            self.hash3[idx3] = cur_pos;
        }
        if has_h4 {
            self.hash4[idx4] = cur_pos;
        }

        let mut out: Vec<(u32, u32)> = Vec::with_capacity(16);

        // Short match from hash2 (length 2)
        if cur2 != 0 {
            let dist2 = cur_pos.wrapping_sub(cur2);
            if dist2 <= self.dict_size && dist2 > 0 {
                // cur2 is 1-based; convert to 0-based buffer index
                let mp = (cur2 - 1) as usize;
                if mp + 2 <= buf.len() && buf[pos] == buf[mp] && buf[pos + 1] == buf[mp + 1] {
                    out.push((dist2 - 1, 2));
                }
            }
        }

        // Short match from hash3 (length 3)
        if cur3 != 0 {
            let dist3 = cur_pos.wrapping_sub(cur3);
            if dist3 <= self.dict_size && dist3 > 0 {
                // cur3 is 1-based; convert to 0-based buffer index
                let mp = (cur3 - 1) as usize;
                if mp + 3 <= buf.len()
                    && buf[pos] == buf[mp]
                    && buf[pos + 1] == buf[mp + 1]
                    && buf[pos + 2] == buf[mp + 2]
                {
                    let best_so_far = out.last().map(|(_, l)| *l).unwrap_or(0);
                    if 3 > best_so_far {
                        out.push((dist3 - 1, 3));
                    }
                }
            }
        }

        let min_len_for_bt = out.last().map(|(_, l)| *l).unwrap_or(0);

        // BT4 tree traversal for matches of length 4+
        self.bt_find_insert(buf, pos, cur4, min_len_for_bt, &mut out);

        // Ensure the output is strictly increasing in length
        let mut result: Vec<(u32, u32)> = Vec::with_capacity(out.len());
        let mut prev_len = 0u32;
        // out was built in order: h2 (len 2), h3 (len 3), bt4 (len 4+, increasing)
        // but we must guarantee strict increase, so filter
        for (dist, len) in out {
            if len > prev_len {
                result.push((dist, len));
                prev_len = len;
            }
        }

        self.advance_pos();
        result
    }

    /// Advance `pos` and `cyclic_buffer_pos`.
    #[inline]
    fn advance_pos(&mut self) {
        self.pos = self.pos.wrapping_add(1);
        self.cyclic_buffer_pos += 1;
        if self.cyclic_buffer_pos >= self.cyclic_buffer_size {
            self.cyclic_buffer_pos = 0;
        }
    }

    /// Insert `pos` into hash tables and the BT without collecting matches.
    fn skip_inner(&mut self, buf: &[u8], pos: usize) {
        if pos + 2 > buf.len() {
            self.advance_pos();
            return;
        }

        let b0 = buf[pos];
        let b1 = buf[pos + 1];
        let b2 = if pos + 2 < buf.len() { buf[pos + 2] } else { 0 };
        let b3 = if pos + 3 < buf.len() { buf[pos + 3] } else { 0 };
        let has_h4 = pos + 4 <= buf.len();

        let idx2 = Self::h2(b0, b1);
        let idx3 = Self::h3(b0, b1, b2);
        let idx4 = if has_h4 { Self::h4(b0, b1, b2, b3) } else { 0 };

        let cur4 = if has_h4 { self.hash4[idx4] } else { 0 };
        let cur_pos = self.pos;

        self.hash2[idx2] = cur_pos;
        if pos + 2 < buf.len() {
            self.hash3[idx3] = cur_pos;
        }
        if has_h4 {
            self.hash4[idx4] = cur_pos;
        }

        // Update tree with a very high min_len so no matches are recorded
        let mut dummy: Vec<(u32, u32)> = Vec::new();
        self.bt_find_insert(buf, pos, cur4, u32::MAX, &mut dummy);

        self.advance_pos();
    }
}

impl MatchFinder for Bt4MatchFinder {
    fn reset(&mut self, dict_size: u32) {
        let dict_size = dict_size.max(4096);
        let cyclic_buffer_size = dict_size as usize + 1;
        self.dict_size = dict_size;
        self.cyclic_buffer_size = cyclic_buffer_size;
        self.hash2.fill(0);
        self.hash3.fill(0);
        self.hash4.fill(0);
        self.son = vec![0u32; cyclic_buffer_size * 2];
        self.cyclic_buffer_pos = 0;
        self.pos = 1;
    }

    fn find_matches(&mut self, buf: &[u8], pos: usize) -> Vec<(u32, u32)> {
        self.find_and_insert(buf, pos)
    }

    fn skip(&mut self, buf: &[u8], pos: usize) {
        self.skip_inner(buf, pos);
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ------------------------------------------------------------------
    // Helper: brute-force reference matcher for test verification
    // ------------------------------------------------------------------

    /// Brute-force match finder: scans backwards up to `depth` positions,
    /// returns all (dist_0based, len) pairs in order of strictly increasing
    /// length. Used only in tests for ground-truth comparison.
    fn brute_force_matches(
        buf: &[u8],
        pos: usize,
        depth: usize,
        dict_size: usize,
    ) -> Vec<(u32, u32)> {
        if pos < 1 || pos + 2 > buf.len() {
            return Vec::new();
        }
        let max_len = (buf.len() - pos).min(MAX_MATCH_LEN as usize);
        let look = depth.min(pos).min(dict_size);
        let mut best_len = 0usize;
        let mut out: Vec<(u32, u32)> = Vec::new();

        for step in 1..=look {
            let cand = pos - step; // dist = step, dist_0based = step - 1
            let mut len = 0usize;
            while len < max_len && buf[pos + len] == buf[cand + len] {
                len += 1;
            }
            if len >= 2 && len > best_len {
                out.push(((step - 1) as u32, len as u32));
                best_len = len;
            }
        }
        out
    }

    // -----------------------------------------------------------------------
    // 1. BT4 vs brute force on small deterministic input
    // -----------------------------------------------------------------------
    #[test]
    fn test_bt4_matches_brute_force_on_small_input() {
        // 500 bytes cycling 0..50 – rich repeat structure
        let buf: Vec<u8> = (0u8..50).cycle().take(500).collect();
        let dict_size = 256usize;
        let depth = 128usize;

        let mut bt4 = Bt4MatchFinder::new(128, 32, dict_size as u32);

        for pos in 0..buf.len() {
            let bt4_matches = bt4.find_matches(&buf, pos);
            let brute = brute_force_matches(&buf, pos, depth, dict_size);

            // BT4 must find at least as long a match as brute force
            let bt4_best = bt4_matches.last().map(|(_, l)| *l).unwrap_or(0);
            let brute_best = brute.last().map(|(_, l)| *l).unwrap_or(0);
            assert!(
                bt4_best >= brute_best,
                "pos={pos}: bt4 best len {bt4_best} < brute best len {brute_best}"
            );

            // All reported BT4 lengths must be strictly increasing
            for w in bt4_matches.windows(2) {
                assert!(
                    w[1].1 > w[0].1,
                    "pos={pos}: non-strict length increase in BT4 output"
                );
            }
        }
    }

    // -----------------------------------------------------------------------
    // 2. BT4 vs brute force on pseudo-random seeded data
    // -----------------------------------------------------------------------
    #[test]
    fn test_bt4_matches_brute_force_random_seeded() {
        // Deterministic pattern with some local structure (prime step modulo)
        let mut buf = Vec::with_capacity(600);
        let mut x: u8 = 7;
        for i in 0..600usize {
            x = x.wrapping_add(((i * 97) & 0xFF) as u8);
            buf.push(x);
        }

        let dict_size = 256usize;
        let depth = 128usize;
        let mut bt4 = Bt4MatchFinder::new(128, 32, dict_size as u32);

        for pos in 0..buf.len() {
            let bt4_matches = bt4.find_matches(&buf, pos);
            let brute = brute_force_matches(&buf, pos, depth, dict_size);

            let bt4_best = bt4_matches.last().map(|(_, l)| *l).unwrap_or(0);
            let brute_best = brute.last().map(|(_, l)| *l).unwrap_or(0);
            assert!(
                bt4_best >= brute_best,
                "pos={pos}: bt4 best {bt4_best} < brute {brute_best}"
            );
        }
    }

    // -----------------------------------------------------------------------
    // 3. BT4 round-trip at level 9
    // -----------------------------------------------------------------------
    #[test]
    fn test_bt4_roundtrip_level_9() {
        use crate::{LzmaLevel, compress, decompress_bytes};

        let mut data = Vec::with_capacity(5000);
        for i in 0..5000usize {
            data.push(((i * 37 + i / 100) & 0xFF) as u8);
        }

        let compressed = compress(&data, LzmaLevel::new(9)).expect("compress failed");
        let decompressed = decompress_bytes(&compressed).expect("decompress failed");
        assert_eq!(decompressed, data, "level 9 roundtrip failed");
    }

    // -----------------------------------------------------------------------
    // 4. BT4 compresses repeating patterns at least as well as hash-chain
    // -----------------------------------------------------------------------
    #[test]
    fn test_bt4_compresses_better_than_chain_at_level_9() {
        use crate::{LzmaLevel, compress};

        // 10 000 bytes of repeating pattern
        let pattern = b"The quick brown fox jumps over the lazy dog. ";
        let data: Vec<u8> = pattern.iter().cycle().take(10_000).copied().collect();

        let out6 = compress(&data, LzmaLevel::new(6)).expect("compress l6 failed");
        let out9 = compress(&data, LzmaLevel::new(9)).expect("compress l9 failed");

        // Level 9 (BT4) must produce output no larger than level 6 (hash-chain)
        assert!(
            out9.len() <= out6.len(),
            "BT4 level 9 ({} bytes) should be <= chain level 6 ({} bytes)",
            out9.len(),
            out6.len()
        );
    }

    // -----------------------------------------------------------------------
    // 5. Skip keeps tree consistent
    // -----------------------------------------------------------------------
    #[test]
    fn test_bt4_skip_keeps_tree_consistent() {
        use crate::{LzmaLevel, compress, decompress_bytes};

        // Pattern where a long match forces skip(), then regular data follows
        let mut data = vec![0u8; 600];
        for (i, b) in data.iter_mut().enumerate().take(50) {
            *b = (i as u8).wrapping_mul(3);
        }
        for i in 50..250 {
            data[i] = data[i % 50];
        }
        for (offset, b) in data.iter_mut().enumerate().skip(250).take(350) {
            *b = ((offset * 7) & 0xFF) as u8;
        }

        let compressed = compress(&data, LzmaLevel::new(9)).expect("compress failed");
        let decompressed = decompress_bytes(&compressed).expect("decompress failed");
        assert_eq!(decompressed, data, "skip consistency roundtrip failed");
    }

    // -----------------------------------------------------------------------
    // 6. BT4 handles a long run (273 byte match)
    // -----------------------------------------------------------------------
    #[test]
    fn test_bt4_handles_long_match() {
        // 600 bytes all equal: BT4 should find a match of length 273
        let buf = vec![0xABu8; 600];
        let mut bt4 = Bt4MatchFinder::new(512, 273, 4096);

        let mut found_long = false;
        for pos in 0..buf.len() {
            let matches = bt4.find_matches(&buf, pos);
            if let Some(&(_, len)) = matches.last() {
                if len >= 273 {
                    found_long = true;
                }
            }
        }
        assert!(
            found_long,
            "BT4 should find a match of length >= 273 in a repeated-byte buffer"
        );
    }

    // -----------------------------------------------------------------------
    // 7. BT4 handles no match at positions 0, 1, 2 with unique bytes
    // -----------------------------------------------------------------------
    #[test]
    fn test_bt4_handles_no_match() {
        let buf = [0xAAu8, 0xBB, 0xCC, 0xDD, 0xEE];
        let mut bt4 = Bt4MatchFinder::new(512, 273, 4096);

        // pos=0: no history at all
        let m0 = bt4.find_matches(&buf, 0);
        assert!(m0.is_empty(), "pos=0: expected no matches, got {:?}", m0);

        // pos=1: only 1 byte of history
        let m1 = bt4.find_matches(&buf, 1);
        assert!(m1.is_empty(), "pos=1: expected no matches, got {:?}", m1);

        // pos=2: only 2 bytes of history, unique bytes → no 2-gram repeat
        let m2 = bt4.find_matches(&buf, 2);
        assert!(
            m2.is_empty(),
            "pos=2: expected no matches with unique bytes, got {:?}",
            m2
        );
    }

    // -----------------------------------------------------------------------
    // 8. HashChainMatchFinder produces valid, strictly-increasing results
    // -----------------------------------------------------------------------
    #[test]
    fn test_hash_chain_finder_trait_impl_unchanged() {
        let depth = 64usize;
        let nice = 64u32;
        let dict = 1 << 20u32;
        let buf: Vec<u8> = b"abcdefghijklmnopqrstuvwxyzabcdefghijklmnop".to_vec();

        let mut hc = HashChainMatchFinder::new(depth, nice, dict);

        for pos in 0..buf.len() {
            let matches = hc.find_matches(&buf, pos);

            // Lengths must be strictly increasing
            for w in matches.windows(2) {
                assert!(w[1].1 > w[0].1, "pos={pos}: non-strict lengths");
            }

            // Distances must point to valid, matching bytes
            for &(dist, len) in &matches {
                let match_pos = pos as i64 - dist as i64 - 1;
                assert!(match_pos >= 0, "pos={pos}: negative match pos");
                let match_pos = match_pos as usize;
                for off in 0..len as usize {
                    assert_eq!(
                        buf[pos + off],
                        buf[match_pos + off],
                        "pos={pos} dist={dist} len={len}: mismatch at offset {off}"
                    );
                }
            }
        }

        // Must find a match at the second occurrence of the alphabet prefix
        let mut hc2 = HashChainMatchFinder::new(depth, nice, dict);
        // The buffer is "abcdefghijklmnopqrstuvwxyz" + "abcdefghijklmnop"
        // The second 'a' starts at index 26
        for p in 0..26 {
            hc2.find_matches(&buf, p);
        }
        let m = hc2.find_matches(&buf, 26);
        assert!(
            !m.is_empty(),
            "expected match at index 26 (repeated prefix), got empty"
        );
    }

    // -----------------------------------------------------------------------
    // 9. Level 8 (hash-chain) roundtrip
    // -----------------------------------------------------------------------
    #[test]
    fn test_match_finder_dispatch_level_8_chain() {
        use crate::{LzmaLevel, compress, decompress_bytes};

        let data: Vec<u8> = (0u8..=255).cycle().take(3000).collect();
        let compressed = compress(&data, LzmaLevel::new(8)).expect("compress l8 failed");
        let decompressed = decompress_bytes(&compressed).expect("decompress l8 failed");
        assert_eq!(decompressed, data, "level 8 roundtrip failed");
    }

    // -----------------------------------------------------------------------
    // 10. Level 9 (BT4) roundtrip and quality check
    // -----------------------------------------------------------------------
    #[test]
    fn test_match_finder_dispatch_level_9_bt4() {
        use crate::{LzmaLevel, compress, decompress_bytes};

        let data: Vec<u8> = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ"
            .iter()
            .cycle()
            .take(8000)
            .copied()
            .collect();

        let out9 = compress(&data, LzmaLevel::new(9)).expect("compress l9 failed");
        let dec9 = decompress_bytes(&out9).expect("decompress l9 failed");
        assert_eq!(dec9, data, "level 9 bt4 roundtrip failed");

        let out6 = compress(&data, LzmaLevel::new(6)).expect("compress l6 failed");

        // BT4 at level 9 should produce output no larger than greedy at level 6
        assert!(
            out9.len() <= out6.len(),
            "level 9 BT4 ({} bytes) should be no larger than level 6 chain ({} bytes)",
            out9.len(),
            out6.len()
        );

        // Bt4MatchFinder struct should be larger than HashChainMatchFinder
        assert!(
            std::mem::size_of::<Bt4MatchFinder>() > std::mem::size_of::<HashChainMatchFinder>(),
            "Bt4MatchFinder struct should be larger than HashChainMatchFinder struct"
        );
    }
}
