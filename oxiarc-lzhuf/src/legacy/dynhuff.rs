//! The LHarc *dynamic* (adaptive) Huffman tree used by `-lh2-`.
//!
//! This is the "sibling-property with equal-frequency blocks" adaptive Huffman
//! of LHarc 2.x (`dhuf.c` in the canonical LHa sources), which is a different
//! and considerably more intricate structure than the Vitter-style tree
//! `-lh1-` uses in [`crate::lh1`]:
//!
//! * Nodes are held in **one flat array kept sorted by descending frequency**.
//!   A node's two children are always the adjacent pair `(2k+1, 2k+2)` at
//!   construction time and stay adjacent through every update, so a code bit is
//!   simply the **parity of the node index** — odd means `1`.
//! * Nodes of equal frequency are grouped into *blocks*. Incrementing a leaf
//!   swaps it with its block's leader before bumping the count, which keeps the
//!   array sorted in O(1) per level instead of re-sorting.
//! * Two independent trees share the arrays: the literal/length tree rooted at
//!   [`ROOT_C`] and the match-position tree rooted at [`ROOT_P`].
//! * The position tree **grows over time**: a new position group (64 distances)
//!   is grafted on every time the output passes another 64-byte boundary, so
//!   early in the stream short distances cost very few bits.
//!
//! Both the decoder and the encoder drive the same [`DynTree`], and every
//! mutation happens at exactly the point in the bit stream where the reference
//! decoder performs it — that is what makes the two sides interchangeable.

use oxiarc_core::error::{OxiArcError, Result};

/// Shortest match length; a length code is `length + 256 - THRESHOLD`.
pub(crate) const THRESHOLD: usize = 3;
/// Size of the shared literal/length symbol space (`256 + 60 - 3 + 1`).
///
/// Position symbols are stored above it, at `N_CHAR + group`.
pub(crate) const N_CHAR: usize = 314;
/// Nodes reserved for the literal/length tree.
const TREESIZE_C: usize = N_CHAR * 2;
/// Nodes reserved for the position tree (128 groups, so 128 leaves).
const TREESIZE_P: usize = 128 * 2;
/// Total node capacity of the shared arrays.
const TREESIZE: usize = TREESIZE_C + TREESIZE_P;
/// Root node of the literal/length tree.
const ROOT_C: usize = 0;
/// Root node of the position tree.
const ROOT_P: usize = TREESIZE_C;
/// Frequency at which the literal/length tree is halved and rebuilt.
const MAX_FREQ: u16 = 0x8000;
/// Sentinel frequency parked on the position root so nothing swaps past it.
const ROOT_P_SENTINEL: u16 = 0xFFFF;
/// Hard cap on a root-to-leaf walk; a conformant tree is far shallower.
const MAX_WALK: usize = 64;

/// Adaptive Huffman state for one `-lh2-` stream.
pub(crate) struct DynTree {
    /// Negative values are leaves holding `!symbol`; positive values are the
    /// index of the node's *even* (bit `0`) child, whose sibling is `index - 1`.
    child: Vec<i32>,
    parent: Vec<u32>,
    block: Vec<u32>,
    edge: Vec<u32>,
    stock: Vec<u32>,
    /// Node index of each symbol's leaf.
    s_node: Vec<u32>,
    freq: Vec<u16>,
    avail: usize,
    /// Escape symbol: `n_max - 1`. Coding it means "eight more raw bits of
    /// length follow".
    n1: usize,
    n_max: usize,
    /// Highest node currently used by the position tree.
    most_p: usize,
    total_p: u16,
    /// Dictionary size in bytes; position groups stop growing at `nn / 64`.
    nn: u64,
    /// Output byte count at which the next position group is grafted on.
    next_count: u64,
}

impl DynTree {
    /// Build the initial trees for `-lh2-`: 286 literal/length symbols over an
    /// 8 KiB dictionary.
    pub(crate) fn new_lh2() -> Self {
        let mut tree = Self {
            child: vec![0; TREESIZE],
            parent: vec![0; TREESIZE],
            block: vec![0; TREESIZE],
            edge: vec![0; TREESIZE],
            stock: vec![0; TREESIZE],
            s_node: vec![0; TREESIZE / 2],
            freq: vec![0; TREESIZE],
            avail: 0,
            n1: 0,
            n_max: 286,
            most_p: 0,
            total_p: 0,
            nn: 1 << 13,
            next_count: 64,
        };
        tree.start_c();
        tree.start_p();
        tree
    }

    /// Escape symbol; decoding it means eight raw length bits follow.
    pub(crate) fn escape_symbol(&self) -> usize {
        self.n1
    }

    /// `start_c_dyn`: one leaf per symbol at frequency 1, then the internal
    /// nodes bottom-up so the array is sorted by descending frequency.
    fn start_c(&mut self) {
        let n_max = self.n_max;
        // LHa `start_c_dyn`: the escape symbol is 512 when the alphabet is
        // large enough to reach it, otherwise the last symbol.
        self.n1 = if n_max > 256 + 256 - THRESHOLD {
            512
        } else {
            n_max - 1
        };
        for index in 0..TREESIZE_C {
            self.stock[index] = index as u32;
            self.block[index] = 0;
        }
        let mut node = n_max * 2 - 2;
        for symbol in 0..n_max {
            self.freq[node] = 1;
            self.child[node] = !(symbol as i32);
            self.s_node[symbol] = node as u32;
            self.block[node] = 1;
            node -= 1;
        }
        // `node` now sits at `n_max - 2`, the highest internal node.
        self.avail = 2;
        self.edge[1] = (n_max - 1) as u32;

        let mut lower = n_max * 2 - 2;
        let mut current = n_max as isize - 2;
        while current >= 0 {
            let slot = current as usize;
            let combined = self.freq[lower] + self.freq[lower - 1];
            self.freq[slot] = combined;
            self.child[slot] = lower as i32;
            self.parent[lower] = slot as u32;
            self.parent[lower - 1] = slot as u32;
            if combined == self.freq[slot + 1] {
                let existing = self.block[slot + 1];
                self.block[slot] = existing;
                self.edge[existing as usize] = slot as u32;
            } else {
                let fresh = self.stock[self.avail];
                self.avail += 1;
                self.block[slot] = fresh;
                self.edge[fresh as usize] = slot as u32;
            }
            lower -= 2;
            current -= 1;
        }
    }

    /// `start_p_dyn`: the position tree starts as a single leaf (group 0), so
    /// the first 64 distances cost zero tree bits.
    fn start_p(&mut self) {
        self.freq[ROOT_P] = 1;
        self.child[ROOT_P] = !(N_CHAR as i32);
        self.s_node[N_CHAR] = ROOT_P as u32;
        let fresh = self.stock[self.avail];
        self.avail += 1;
        self.block[ROOT_P] = fresh;
        self.edge[fresh as usize] = ROOT_P as u32;
        self.most_p = ROOT_P;
        self.total_p = 0;
        self.next_count = 64;
    }

    /// `reconst`: halve every leaf frequency and rebuild `[start, end)` so the
    /// array is sorted again. Called when a root's counter saturates.
    fn reconst(&mut self, start: usize, end: usize) {
        let mut write = start;
        for read in start..end {
            let node = self.child[read];
            if node < 0 {
                self.freq[write] = self.freq[read].div_ceil(2);
                self.child[write] = node;
                write += 1;
            }
            let block_id = self.block[read] as usize;
            if self.edge[block_id] as usize == read {
                self.avail -= 1;
                self.stock[self.avail] = block_id as u32;
            }
        }

        let mut source = write as isize - 1;
        let mut target = end as isize - 1;
        let mut pair = end as isize - 2;
        while target >= start as isize {
            while target >= pair {
                self.freq[target as usize] = self.freq[source as usize];
                self.child[target as usize] = self.child[source as usize];
                target -= 1;
                source -= 1;
            }
            let combined = self.freq[pair as usize] + self.freq[(pair + 1) as usize];
            let mut boundary = start;
            while combined < self.freq[boundary] {
                boundary += 1;
            }
            while source >= boundary as isize {
                self.freq[target as usize] = self.freq[source as usize];
                self.child[target as usize] = self.child[source as usize];
                target -= 1;
                source -= 1;
            }
            self.freq[target as usize] = combined;
            self.child[target as usize] = (pair + 1) as i32;
            target -= 1;
            pair -= 2;
        }

        let mut previous_freq = 0u16;
        let mut block_id = 0u32;
        for node in start..end {
            let value = self.child[node];
            if value < 0 {
                self.s_node[(!value) as usize] = node as u32;
            } else {
                let even = value as usize;
                self.parent[even] = node as u32;
                self.parent[even - 1] = node as u32;
            }
            let current_freq = self.freq[node];
            if current_freq == previous_freq {
                self.block[node] = block_id;
            } else {
                block_id = self.stock[self.avail];
                self.avail += 1;
                self.block[node] = block_id;
                self.edge[block_id as usize] = node as u32;
                previous_freq = current_freq;
            }
        }
    }

    /// `swap_inc`: move `node` to the head of its equal-frequency block, bump
    /// its count, and return its parent.
    fn swap_inc(&mut self, node: usize) -> usize {
        let block_id = self.block[node] as usize;
        let leader = self.edge[block_id] as usize;
        let mut position = node;
        let mut adjust = false;

        if leader != position {
            let node_child = self.child[position];
            let leader_child = self.child[leader];
            self.child[position] = leader_child;
            self.child[leader] = node_child;
            if node_child >= 0 {
                let even = node_child as usize;
                self.parent[even] = leader as u32;
                self.parent[even - 1] = leader as u32;
            } else {
                self.s_node[(!node_child) as usize] = leader as u32;
            }
            if leader_child >= 0 {
                let even = leader_child as usize;
                self.parent[even] = position as u32;
                self.parent[even - 1] = position as u32;
            } else {
                self.s_node[(!leader_child) as usize] = position as u32;
            }
            position = leader;
            adjust = true;
        } else if block_id as u32 == self.block[position + 1] {
            adjust = true;
        }

        if adjust {
            self.edge[block_id] += 1;
            self.freq[position] += 1;
            if self.freq[position] == self.freq[position - 1] {
                self.block[position] = self.block[position - 1];
            } else {
                let fresh = self.stock[self.avail];
                self.avail += 1;
                self.block[position] = fresh;
                self.edge[fresh as usize] = position as u32;
            }
        } else {
            self.freq[position] += 1;
            if self.freq[position] == self.freq[position - 1] {
                self.avail -= 1;
                self.stock[self.avail] = block_id as u32;
                self.block[position] = self.block[position - 1];
            }
        }
        self.parent[position] as usize
    }

    /// `update_c`: record one use of literal/length symbol `symbol`.
    fn update_c(&mut self, symbol: usize) {
        if self.freq[ROOT_C] == MAX_FREQ {
            self.reconst(0, self.n_max * 2 - 1);
        }
        self.freq[ROOT_C] += 1;
        let mut node = self.s_node[symbol] as usize;
        loop {
            node = self.swap_inc(node);
            if node == ROOT_C {
                break;
            }
        }
    }

    /// `update_p`: record one use of position group `group`.
    fn update_p(&mut self, group: usize) {
        if self.total_p == MAX_FREQ {
            self.reconst(ROOT_P, self.most_p + 1);
            self.total_p = self.freq[ROOT_P];
            self.freq[ROOT_P] = ROOT_P_SENTINEL;
        }
        let mut node = self.s_node[group + N_CHAR] as usize;
        while node != ROOT_P {
            node = self.swap_inc(node);
        }
        self.total_p += 1;
    }

    /// `make_new_node`: graft position group `group` onto the tree, pushing the
    /// previous deepest leaf down one level.
    fn make_new_node(&mut self, group: usize) {
        let moved = self.most_p + 1;
        let fresh = moved + 1;
        self.child[moved] = self.child[self.most_p];
        self.s_node[(!self.child[moved]) as usize] = moved as u32;
        self.child[fresh] = !((group + N_CHAR) as i32);
        self.child[self.most_p] = fresh as i32;
        self.freq[moved] = self.freq[self.most_p];
        self.freq[fresh] = 0;
        self.block[moved] = self.block[self.most_p];
        if self.most_p == ROOT_P {
            self.freq[ROOT_P] = ROOT_P_SENTINEL;
            let root_block = self.block[ROOT_P] as usize;
            self.edge[root_block] += 1;
        }
        self.parent[moved] = self.most_p as u32;
        self.parent[fresh] = self.most_p as u32;
        let block_id = self.stock[self.avail];
        self.avail += 1;
        self.block[fresh] = block_id;
        self.edge[block_id as usize] = fresh as u32;
        self.s_node[group + N_CHAR] = fresh as u32;
        self.most_p = fresh;
        self.update_p(group);
    }

    /// Graft in every position group the output length has earned so far.
    ///
    /// Called at exactly the same point on both sides: immediately before a
    /// position code is read or written, with `produced` equal to the number of
    /// bytes decoded/encoded *before* the current match.
    pub(crate) fn grow_positions(&mut self, produced: u64) {
        while produced > self.next_count {
            let group = (self.next_count / 64) as usize;
            self.make_new_node(group);
            self.next_count += 64;
            if self.next_count >= self.nn {
                self.next_count = u64::MAX;
            }
        }
    }

    /// Walk from `root` down to a leaf, taking one bit per level, and return
    /// the leaf's symbol.
    fn walk<F>(&self, root: usize, mut next_bit: F) -> Result<usize>
    where
        F: FnMut() -> Result<bool>,
    {
        let mut node = self.child[root];
        let mut steps = 0usize;
        while node > 0 {
            steps += 1;
            if steps > MAX_WALK {
                return Err(OxiArcError::corrupted(
                    0,
                    "-lh2-: adaptive Huffman walk exceeded the maximum code length",
                ));
            }
            let index = node as usize - usize::from(next_bit()?);
            node = self.child[index];
        }
        Ok((!node) as usize)
    }

    /// Root-to-leaf bit path for `symbol`'s leaf under `root`.
    ///
    /// Empty when the leaf *is* the root, which is how a single-group position
    /// tree costs zero bits.
    fn path(&self, root: usize, symbol: usize) -> Vec<bool> {
        let mut bits = Vec::with_capacity(16);
        let mut node = self.s_node[symbol] as usize;
        while node != root {
            bits.push(node & 1 == 1);
            node = self.parent[node] as usize;
        }
        bits.reverse();
        bits
    }

    /// Decode one literal/length symbol (before the eight escape bits).
    pub(crate) fn decode_c<F>(&mut self, next_bit: F) -> Result<usize>
    where
        F: FnMut() -> Result<bool>,
    {
        let symbol = self.walk(ROOT_C, next_bit)?;
        if symbol >= self.n_max {
            return Err(OxiArcError::corrupted(
                0,
                "-lh2-: literal/length symbol out of range",
            ));
        }
        self.update_c(symbol);
        Ok(symbol)
    }

    /// Decode one position group (before the six low distance bits).
    pub(crate) fn decode_p<F>(&mut self, next_bit: F) -> Result<usize>
    where
        F: FnMut() -> Result<bool>,
    {
        let symbol = self.walk(ROOT_P, next_bit)?;
        if !(N_CHAR..N_CHAR + 128).contains(&symbol) {
            return Err(OxiArcError::corrupted(
                0,
                "-lh2-: position symbol out of range",
            ));
        }
        let group = symbol - N_CHAR;
        self.update_p(group);
        Ok(group)
    }

    /// Bit path for a literal/length symbol, then record its use.
    pub(crate) fn encode_c(&mut self, symbol: usize) -> Vec<bool> {
        let bits = self.path(ROOT_C, symbol);
        self.update_c(symbol);
        bits
    }

    /// Bit path for a position group, then record its use.
    pub(crate) fn encode_p(&mut self, group: usize) -> Vec<bool> {
        let bits = self.path(ROOT_P, group + N_CHAR);
        self.update_p(group);
        bits
    }

    /// Number of position groups currently reachable.
    #[cfg(test)]
    pub(crate) fn position_groups(&self) -> usize {
        (self.most_p - ROOT_P) / 2 + 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feed a decoder from a fixed bit list.
    struct BitFeed {
        bits: Vec<bool>,
        index: usize,
    }

    impl BitFeed {
        fn next(&mut self) -> Result<bool> {
            let bit = self.bits.get(self.index).copied().unwrap_or(false);
            self.index += 1;
            Ok(bit)
        }
    }

    #[test]
    fn initial_tree_is_balanced_over_286_symbols() {
        let tree = DynTree::new_lh2();
        assert_eq!(tree.escape_symbol(), 285);
        assert_eq!(tree.position_groups(), 1);
        // Every literal starts at frequency 1, so every code is 9 bits (256
        // symbols would be exactly 9; 286 makes some 9 and some 10).
        let lengths: Vec<usize> = (0..286).map(|s| tree.path(ROOT_C, s).len()).collect();
        assert!(lengths.iter().all(|&l| (8..=10).contains(&l)));
    }

    #[test]
    fn encode_and_decode_agree_symbol_for_symbol() {
        let symbols: Vec<usize> = (0..600).map(|i| (i * 37) % 286).collect();

        let mut encoder = DynTree::new_lh2();
        let mut bits = Vec::new();
        for &symbol in &symbols {
            bits.extend(encoder.encode_c(symbol));
        }

        let mut decoder = DynTree::new_lh2();
        let mut feed = BitFeed { bits, index: 0 };
        for &expected in &symbols {
            let got = decoder
                .decode_c(|| feed.next())
                .expect("decode literal symbol");
            assert_eq!(got, expected);
        }
    }

    #[test]
    fn frequent_symbols_get_shorter_codes() {
        let mut tree = DynTree::new_lh2();
        let before = tree.path(ROOT_C, 65).len();
        for _ in 0..2000 {
            let _ = tree.encode_c(65);
        }
        let after = tree.path(ROOT_C, 65).len();
        assert!(
            after < before,
            "an adaptive tree must shorten a hot symbol: {before} -> {after}"
        );
        assert_eq!(after, 1, "a dominant symbol should reach a 1-bit code");
    }

    #[test]
    fn reconstruction_survives_saturation() {
        // Drive the root counter past MAX_FREQ so `reconst` runs, then check the
        // tree still round-trips.
        let mut encoder = DynTree::new_lh2();
        let mut decoder = DynTree::new_lh2();
        let mut bits = Vec::new();
        let symbols: Vec<usize> = (0..40_000).map(|i| (i * 7) % 286).collect();
        for &symbol in &symbols {
            bits.extend(encoder.encode_c(symbol));
        }
        let mut feed = BitFeed { bits, index: 0 };
        for &expected in &symbols {
            assert_eq!(
                decoder.decode_c(|| feed.next()).expect("decode"),
                expected,
                "mismatch after tree reconstruction"
            );
        }
    }

    #[test]
    fn position_tree_grows_with_output_length() {
        let mut tree = DynTree::new_lh2();
        assert_eq!(tree.position_groups(), 1);
        tree.grow_positions(65);
        assert_eq!(tree.position_groups(), 2);
        tree.grow_positions(1000);
        assert_eq!(tree.position_groups(), 16);
        // Groups stop at 128 (8192 / 64).
        tree.grow_positions(100_000);
        assert_eq!(tree.position_groups(), 128);
    }

    #[test]
    fn single_group_position_code_is_free() {
        let mut tree = DynTree::new_lh2();
        assert!(
            tree.encode_p(0).is_empty(),
            "one group means the leaf is the root, so no bits are spent"
        );
    }

    #[test]
    fn position_encode_decode_agree_after_growth() {
        let mut encoder = DynTree::new_lh2();
        let mut decoder = DynTree::new_lh2();
        let mut bits = Vec::new();
        let mut produced = 0u64;
        let mut groups = Vec::new();
        for step in 0..500 {
            produced += 37;
            encoder.grow_positions(produced);
            let group = step % ((produced / 64).max(1) as usize).min(128);
            groups.push((produced, group));
            bits.extend(encoder.encode_p(group));
        }
        let mut feed = BitFeed { bits, index: 0 };
        for (produced, expected) in groups {
            decoder.grow_positions(produced);
            assert_eq!(
                decoder.decode_p(|| feed.next()).expect("decode position"),
                expected
            );
        }
    }
}
