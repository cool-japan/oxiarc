//! Huffman tree construction and block emission for the DEFLATE encoder.
//!
//! This is a faithful reimplementation of zlib's `trees.c`: the same heap with
//! the same `depth`-based tie-break, the same bit-length overflow correction,
//! the same run-length coding of the code-length alphabet, and the same
//! stored/fixed/dynamic block-type decision made on the *real* bit cost of all
//! three encodings. Reproducing the algorithm exactly (rather than
//! approximating it) is what lets the oracle suite assert byte-identity with
//! CPython's `zlib` instead of a fuzzy size band.

use super::tables::{
    BASE_DIST, BASE_LENGTH, BL_CODES, BL_ORDER, D_CODES, END_BLOCK, EXTRA_BLBITS, EXTRA_DBITS,
    EXTRA_LBITS, HEAP_SIZE, L_CODES, LENGTH_CODE, LITERALS, MAX_BITS, MAX_BL_BITS, REP_3_6,
    REPZ_3_10, REPZ_11_138, STATIC_DTREE_CODE, STATIC_DTREE_LEN, STATIC_LTREE_CODE,
    STATIC_LTREE_LEN, d_code,
};

/// Smallest heap slot (the heap is 1-based; slot 0 is unused).
const SMALLEST: usize = 1;

/// Compression strategy, mirroring zlib's `Z_*` strategy constants.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum Strategy {
    /// Normal LZ77 + Huffman (zlib `Z_DEFAULT_STRATEGY`).
    #[default]
    Default,
    /// Filtered data produced by a filter or predictor: small matches are
    /// discarded so literals dominate (zlib `Z_FILTERED`).
    Filtered,
    /// Huffman coding only — no string matching at all (zlib `Z_HUFFMAN_ONLY`).
    HuffmanOnly,
    /// Match only against the immediately preceding byte, i.e. run-length
    /// encoding (zlib `Z_RLE`).
    Rle,
    /// Never use dynamic Huffman trees (zlib `Z_FIXED`).
    Fixed,
}

/// Bit-level output accumulator.
///
/// DEFLATE packs bits least-significant-first inside each byte, so Huffman
/// codes are stored bit-reversed and simply OR-ed in at the current offset.
#[derive(Debug, Default)]
pub(crate) struct BitSink {
    /// Bytes emitted so far.
    pub out: Vec<u8>,
    /// Bits not yet forming a whole byte (LSB-first).
    pub bit_buf: u64,
    /// Number of valid bits in `bit_buf` (always `< 8` between calls).
    pub bit_valid: u32,
}

impl BitSink {
    /// Append `length` bits of `value` (LSB-first).
    #[inline(always)]
    pub(crate) fn send_bits(&mut self, value: u32, length: u32) {
        self.bit_buf |= u64::from(value) << self.bit_valid;
        self.bit_valid += length;
        while self.bit_valid >= 8 {
            self.out.push(self.bit_buf as u8);
            self.bit_buf >>= 8;
            self.bit_valid -= 8;
        }
    }

    /// Pad the current partial byte with zeros and emit it.
    pub(crate) fn bi_windup(&mut self) {
        while self.bit_valid > 0 {
            self.out.push(self.bit_buf as u8);
            self.bit_buf >>= 8;
            self.bit_valid = self.bit_valid.saturating_sub(8);
        }
        self.bit_buf = 0;
        self.bit_valid = 0;
    }

    /// Append raw bytes; only valid on a byte boundary.
    pub(crate) fn put_bytes(&mut self, bytes: &[u8]) {
        debug_assert_eq!(self.bit_valid, 0);
        self.out.extend_from_slice(bytes);
    }

    /// Append a little-endian 16-bit value; only valid on a byte boundary.
    pub(crate) fn put_short(&mut self, value: u16) {
        debug_assert_eq!(self.bit_valid, 0);
        self.out.push(value as u8);
        self.out.push((value >> 8) as u8);
    }
}

/// Per-alphabet frequency/code/length/parent storage.
#[derive(Debug)]
struct TreeData {
    freq: Vec<u16>,
    code: Vec<u16>,
    len: Vec<u16>,
    dad: Vec<u16>,
}

impl TreeData {
    fn new(size: usize) -> Self {
        Self {
            freq: vec![0; size],
            code: vec![0; size],
            len: vec![0; size],
            dad: vec![0; size],
        }
    }
}

/// Immutable description of one alphabet.
struct StaticDesc {
    static_len: Option<&'static [u8]>,
    extra_bits: &'static [u8],
    extra_base: usize,
    elems: usize,
    max_length: usize,
}

const L_DESC: StaticDesc = StaticDesc {
    static_len: Some(&STATIC_LTREE_LEN),
    extra_bits: &EXTRA_LBITS,
    extra_base: LITERALS + 1,
    elems: L_CODES,
    max_length: MAX_BITS,
};

const D_DESC: StaticDesc = StaticDesc {
    static_len: Some(&STATIC_DTREE_LEN),
    extra_bits: &EXTRA_DBITS,
    extra_base: 0,
    elems: D_CODES,
    max_length: MAX_BITS,
};

const BL_DESC: StaticDesc = StaticDesc {
    static_len: None,
    extra_bits: &EXTRA_BLBITS,
    extra_base: 0,
    elems: BL_CODES,
    max_length: MAX_BL_BITS,
};

/// Shared Huffman-construction scratch space plus the running block cost.
struct HeapState {
    heap: [i32; HEAP_SIZE],
    depth: [u8; HEAP_SIZE],
    bl_count: [u16; MAX_BITS + 1],
    heap_len: usize,
    heap_max: usize,
    /// Bit cost of the block under the dynamic trees (wrapping, like zlib's
    /// `ulg`: `build_tree` may decrement it below zero before `gen_bitlen`
    /// adds the real cost back).
    opt_len: u64,
    /// Bit cost of the block under the fixed trees.
    static_len: u64,
}

impl HeapState {
    fn new() -> Self {
        Self {
            heap: [0; HEAP_SIZE],
            depth: [0; HEAP_SIZE],
            bl_count: [0; MAX_BITS + 1],
            heap_len: 0,
            heap_max: HEAP_SIZE,
            opt_len: 0,
            static_len: 0,
        }
    }
}

#[inline]
fn smaller(freq: &[u16], n: usize, m: usize, depth: &[u8]) -> bool {
    freq[n] < freq[m] || (freq[n] == freq[m] && depth[n] <= depth[m])
}

fn pqdownheap(t: &TreeData, h: &mut HeapState, k0: usize) {
    let mut k = k0;
    let v = h.heap[k];
    let mut j = k << 1;
    while j <= h.heap_len {
        if j < h.heap_len
            && smaller(
                &t.freq,
                h.heap[j + 1] as usize,
                h.heap[j] as usize,
                &h.depth,
            )
        {
            j += 1;
        }
        if smaller(&t.freq, v as usize, h.heap[j] as usize, &h.depth) {
            break;
        }
        h.heap[k] = h.heap[j];
        k = j;
        j <<= 1;
    }
    h.heap[k] = v;
}

/// zlib `gen_bitlen`: assign code lengths from the heap, correcting any
/// lengths that exceed `max_length` by rebalancing the leaf counts.
fn gen_bitlen(t: &mut TreeData, h: &mut HeapState, desc: &StaticDesc, max_code: usize) {
    h.bl_count = [0; MAX_BITS + 1];

    let root = h.heap[h.heap_max] as usize;
    t.len[root] = 0;

    let mut overflow: i32 = 0;
    for hi in (h.heap_max + 1)..HEAP_SIZE {
        let n = h.heap[hi] as usize;
        let mut bits = t.len[t.dad[n] as usize] as usize + 1;
        if bits > desc.max_length {
            bits = desc.max_length;
            overflow += 1;
        }
        t.len[n] = bits as u16;
        if n > max_code {
            continue;
        }
        h.bl_count[bits] += 1;
        let xbits = if n >= desc.extra_base {
            desc.extra_bits[n - desc.extra_base] as u64
        } else {
            0
        };
        let f = u64::from(t.freq[n]);
        h.opt_len = h.opt_len.wrapping_add(f * (bits as u64 + xbits));
        if let Some(sl) = desc.static_len {
            h.static_len = h.static_len.wrapping_add(f * (u64::from(sl[n]) + xbits));
        }
    }
    if overflow == 0 {
        return;
    }

    // Move leaves up out of the overflowing length class.
    loop {
        let mut bits = desc.max_length - 1;
        while h.bl_count[bits] == 0 {
            bits -= 1;
        }
        h.bl_count[bits] -= 1;
        h.bl_count[bits + 1] += 2;
        h.bl_count[desc.max_length] -= 1;
        overflow -= 2;
        if overflow <= 0 {
            break;
        }
    }

    // Recompute every length, scanning in increasing frequency.
    let mut hi = HEAP_SIZE;
    for bits in (1..=desc.max_length).rev() {
        let mut n = h.bl_count[bits];
        while n != 0 {
            hi -= 1;
            let m = h.heap[hi] as usize;
            if m > max_code {
                continue;
            }
            if t.len[m] as usize != bits {
                // zlib: `s->opt_len += ((ulg)bits - tree[m].Len) * tree[m].Freq;`
                // The difference is negative when a length shrinks, and C's
                // unsigned arithmetic wraps through both the subtraction and
                // the multiplication; reproduce that exactly.
                h.opt_len = h.opt_len.wrapping_add(
                    (bits as u64)
                        .wrapping_sub(u64::from(t.len[m]))
                        .wrapping_mul(u64::from(t.freq[m])),
                );
                t.len[m] = bits as u16;
            }
            n -= 1;
        }
    }
}

/// zlib `gen_codes`: canonical code assignment, stored bit-reversed.
fn gen_codes(t: &mut TreeData, max_code: usize, bl_count: &[u16; MAX_BITS + 1]) {
    let mut next_code = [0u16; MAX_BITS + 1];
    let mut code = 0u32;
    for bits in 1..=MAX_BITS {
        code = (code + u32::from(bl_count[bits - 1])) << 1;
        next_code[bits] = code as u16;
    }
    for n in 0..=max_code {
        let len = t.len[n] as usize;
        if len == 0 {
            continue;
        }
        t.code[n] = super::tables::bi_reverse(u32::from(next_code[len]), len as u32) as u16;
        next_code[len] += 1;
    }
}

/// zlib `build_tree`: build the optimal length-limited Huffman code for one
/// alphabet and return its `max_code`.
fn build_tree(t: &mut TreeData, h: &mut HeapState, desc: &StaticDesc) -> usize {
    let elems = desc.elems;
    let mut max_code: i32 = -1;

    h.heap_len = 0;
    h.heap_max = HEAP_SIZE;

    for n in 0..elems {
        if t.freq[n] != 0 {
            h.heap_len += 1;
            h.heap[h.heap_len] = n as i32;
            max_code = n as i32;
            h.depth[n] = 0;
        } else {
            t.len[n] = 0;
        }
    }

    // DEFLATE requires at least two codes in every alphabet.
    while h.heap_len < 2 {
        h.heap_len += 1;
        let node = if max_code < 2 {
            max_code += 1;
            max_code as usize
        } else {
            0
        };
        h.heap[h.heap_len] = node as i32;
        t.freq[node] = 1;
        h.depth[node] = 0;
        h.opt_len = h.opt_len.wrapping_sub(1);
        if let Some(sl) = desc.static_len {
            h.static_len = h.static_len.wrapping_sub(u64::from(sl[node]));
        }
    }
    let max_code = max_code.max(0) as usize;

    for n in (1..=h.heap_len / 2).rev() {
        pqdownheap(t, h, n);
    }

    let mut node = elems;
    loop {
        // pqremove
        let n = h.heap[SMALLEST] as usize;
        h.heap[SMALLEST] = h.heap[h.heap_len];
        h.heap_len -= 1;
        pqdownheap(t, h, SMALLEST);

        let m = h.heap[SMALLEST] as usize;

        h.heap_max -= 1;
        h.heap[h.heap_max] = n as i32;
        h.heap_max -= 1;
        h.heap[h.heap_max] = m as i32;

        t.freq[node] = t.freq[n] + t.freq[m];
        h.depth[node] = h.depth[n].max(h.depth[m]) + 1;
        t.dad[n] = node as u16;
        t.dad[m] = node as u16;

        h.heap[SMALLEST] = node as i32;
        node += 1;
        pqdownheap(t, h, SMALLEST);

        if h.heap_len < 2 {
            break;
        }
    }

    h.heap_max -= 1;
    h.heap[h.heap_max] = h.heap[SMALLEST];

    gen_bitlen(t, h, desc, max_code);
    let bl_count = h.bl_count;
    gen_codes(t, max_code, &bl_count);
    max_code
}

/// zlib `scan_tree`: accumulate bit-length-alphabet frequencies for `tree`.
fn scan_tree(t: &mut TreeData, bl: &mut TreeData, max_code: usize) {
    let mut prevlen: i32 = -1;
    let mut nextlen = t.len[0];
    let mut count: i32 = 0;
    let mut max_count: i32 = 7;
    let mut min_count: i32 = 4;
    if nextlen == 0 {
        max_count = 138;
        min_count = 3;
    }
    t.len[max_code + 1] = 0xffff;

    for n in 0..=max_code {
        let curlen = nextlen;
        nextlen = t.len[n + 1];
        count += 1;
        if count < max_count && curlen == nextlen {
            continue;
        } else if count < min_count {
            bl.freq[curlen as usize] += count as u16;
        } else if curlen != 0 {
            if i32::from(curlen) != prevlen {
                bl.freq[curlen as usize] += 1;
            }
            bl.freq[REP_3_6] += 1;
        } else if count <= 10 {
            bl.freq[REPZ_3_10] += 1;
        } else {
            bl.freq[REPZ_11_138] += 1;
        }
        count = 0;
        prevlen = i32::from(curlen);
        if nextlen == 0 {
            max_count = 138;
            min_count = 3;
        } else if curlen == nextlen {
            max_count = 6;
            min_count = 3;
        } else {
            max_count = 7;
            min_count = 4;
        }
    }
}

/// zlib `send_tree`: emit `tree`'s code lengths using the bit-length alphabet.
fn send_tree(sink: &mut BitSink, t: &TreeData, bl: &TreeData, max_code: usize) {
    let mut prevlen: i32 = -1;
    let mut nextlen = t.len[0];
    let mut count: i32 = 0;
    let mut max_count: i32 = 7;
    let mut min_count: i32 = 4;
    if nextlen == 0 {
        max_count = 138;
        min_count = 3;
    }

    for n in 0..=max_code {
        let curlen = nextlen;
        nextlen = t.len[n + 1];
        count += 1;
        if count < max_count && curlen == nextlen {
            continue;
        } else if count < min_count {
            for _ in 0..count {
                send_code(sink, curlen as usize, bl);
            }
        } else if curlen != 0 {
            if i32::from(curlen) != prevlen {
                send_code(sink, curlen as usize, bl);
                count -= 1;
            }
            send_code(sink, REP_3_6, bl);
            sink.send_bits((count - 3) as u32, 2);
        } else if count <= 10 {
            send_code(sink, REPZ_3_10, bl);
            sink.send_bits((count - 3) as u32, 3);
        } else {
            send_code(sink, REPZ_11_138, bl);
            sink.send_bits((count - 11) as u32, 7);
        }
        count = 0;
        prevlen = i32::from(curlen);
        if nextlen == 0 {
            max_count = 138;
            min_count = 3;
        } else if curlen == nextlen {
            max_count = 6;
            min_count = 3;
        } else {
            max_count = 7;
            min_count = 4;
        }
    }
}

#[inline(always)]
fn send_code(sink: &mut BitSink, c: usize, t: &TreeData) {
    sink.send_bits(u32::from(t.code[c]), u32::from(t.len[c]));
}

/// The three dynamic trees plus the shared heap: everything `_tr_flush_block`
/// needs.
pub(crate) struct BlockTrees {
    l: TreeData,
    d: TreeData,
    bl: TreeData,
    heap: HeapState,
    l_max_code: usize,
    d_max_code: usize,
}

impl std::fmt::Debug for BlockTrees {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BlockTrees").finish_non_exhaustive()
    }
}

impl BlockTrees {
    pub(crate) fn new() -> Self {
        let mut t = Self {
            l: TreeData::new(HEAP_SIZE),
            d: TreeData::new(2 * D_CODES + 1),
            bl: TreeData::new(2 * BL_CODES + 1),
            heap: HeapState::new(),
            l_max_code: 0,
            d_max_code: 0,
        };
        t.init_block();
        t
    }

    /// zlib `init_block`: clear the frequencies for a fresh block.
    pub(crate) fn init_block(&mut self) {
        self.l.freq[..L_CODES].fill(0);
        self.d.freq[..D_CODES].fill(0);
        self.bl.freq[..BL_CODES].fill(0);
        self.l.freq[END_BLOCK] = 1;
        self.heap.opt_len = 0;
        self.heap.static_len = 0;
    }

    /// Tally one literal byte.
    #[inline(always)]
    pub(crate) fn tally_lit(&mut self, lit: u8) {
        self.l.freq[lit as usize] += 1;
    }

    /// Tally one match of `length` at `dist` (both as emitted, not biased).
    #[inline(always)]
    pub(crate) fn tally_dist(&mut self, dist: usize, lc: usize) {
        self.l.freq[LENGTH_CODE[lc] as usize + LITERALS + 1] += 1;
        self.d.freq[d_code(dist - 1)] += 1;
    }

    /// The real bit cost of encoding `syms` as one block — the cheaper of the
    /// dynamic encoding (**including** its tree description) and the fixed
    /// encoding — computed with the same machinery that emits the block.
    ///
    /// `committed` is what the block has already tallied; `syms` is the
    /// candidate continuation. Both are needed because the tree is shared by
    /// the whole block: a continuation that introduces one extra length code
    /// costs tree bits that a per-span measurement cannot see.
    ///
    /// Used by the optimal parser to choose between two candidate token
    /// sequences — a model that prices only the symbols cannot see that a
    /// token sequence with more distinct codes buys a bigger tree, which on
    /// highly-compressible input is most of the output. The trees are left
    /// re-initialised, so this must be called on a scratch instance, never on
    /// the one accumulating the block being written.
    pub(crate) fn estimate_block_bits(&mut self, committed: &[Sym], syms: &[Sym]) -> u64 {
        self.init_block();
        for sym in committed.iter().chain(syms.iter()) {
            if sym.dist == 0 {
                self.tally_lit(sym.lc);
            } else {
                self.tally_dist(usize::from(sym.dist), usize::from(sym.lc));
            }
        }
        self.l_max_code = build_tree(&mut self.l, &mut self.heap, &L_DESC);
        self.d_max_code = build_tree(&mut self.d, &mut self.heap, &D_DESC);
        let _ = self.build_bl_tree();
        let dynamic = self.heap.opt_len.wrapping_add(3 + 7) >> 3;
        let fixed = self.heap.static_len.wrapping_add(3 + 7) >> 3;
        self.init_block();
        dynamic.min(fixed)
    }

    /// zlib `build_bl_tree`: build the code-length tree and return the index of
    /// the last bit-length code that must be transmitted.
    fn build_bl_tree(&mut self) -> usize {
        scan_tree(&mut self.l, &mut self.bl, self.l_max_code);
        scan_tree(&mut self.d, &mut self.bl, self.d_max_code);
        build_tree(&mut self.bl, &mut self.heap, &BL_DESC);

        let mut max_blindex = BL_CODES - 1;
        while max_blindex >= 3 {
            if self.bl.len[BL_ORDER[max_blindex]] != 0 {
                break;
            }
            max_blindex -= 1;
        }
        self.heap.opt_len = self
            .heap
            .opt_len
            .wrapping_add(3 * (max_blindex as u64 + 1) + 5 + 5 + 4);
        max_blindex
    }

    fn send_all_trees(&self, sink: &mut BitSink, lcodes: usize, dcodes: usize, blcodes: usize) {
        sink.send_bits((lcodes - 257) as u32, 5);
        sink.send_bits((dcodes - 1) as u32, 5);
        sink.send_bits((blcodes - 4) as u32, 4);
        for &slot in BL_ORDER.iter().take(blcodes) {
            sink.send_bits(u32::from(self.bl.len[slot]), 3);
        }
        send_tree(sink, &self.l, &self.bl, lcodes - 1);
        send_tree(sink, &self.d, &self.bl, dcodes - 1);
    }
}

/// One tallied symbol: a literal (`dist == 0`) or a match.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Sym {
    /// Match distance, or 0 for a literal.
    pub dist: u16,
    /// `length - MIN_MATCH` for a match, or the literal byte.
    pub lc: u8,
}

/// Emit the symbol buffer with the given trees, followed by end-of-block.
fn compress_block(sink: &mut BitSink, syms: &[Sym], ltree: &TreeData, dtree: &TreeData) {
    for sym in syms {
        if sym.dist == 0 {
            send_code(sink, sym.lc as usize, ltree);
        } else {
            let lc = sym.lc as usize;
            let code = LENGTH_CODE[lc] as usize;
            send_code(sink, code + LITERALS + 1, ltree);
            let extra = EXTRA_LBITS[code];
            if extra != 0 {
                sink.send_bits((lc - BASE_LENGTH[code] as usize) as u32, u32::from(extra));
            }
            let dist = sym.dist as usize - 1;
            let dcode = d_code(dist);
            send_code(sink, dcode, dtree);
            let extra = EXTRA_DBITS[dcode];
            if extra != 0 {
                sink.send_bits((dist - BASE_DIST[dcode] as usize) as u32, u32::from(extra));
            }
        }
    }
    send_code(sink, END_BLOCK, ltree);
}

/// Emit the symbol buffer with the fixed (static) trees.
fn compress_block_static(sink: &mut BitSink, syms: &[Sym]) {
    for sym in syms {
        if sym.dist == 0 {
            let c = sym.lc as usize;
            sink.send_bits(
                u32::from(STATIC_LTREE_CODE[c]),
                u32::from(STATIC_LTREE_LEN[c]),
            );
        } else {
            let lc = sym.lc as usize;
            let code = LENGTH_CODE[lc] as usize;
            let c = code + LITERALS + 1;
            sink.send_bits(
                u32::from(STATIC_LTREE_CODE[c]),
                u32::from(STATIC_LTREE_LEN[c]),
            );
            let extra = EXTRA_LBITS[code];
            if extra != 0 {
                sink.send_bits((lc - BASE_LENGTH[code] as usize) as u32, u32::from(extra));
            }
            let dist = sym.dist as usize - 1;
            let dcode = d_code(dist);
            sink.send_bits(
                u32::from(STATIC_DTREE_CODE[dcode]),
                u32::from(STATIC_DTREE_LEN[dcode]),
            );
            let extra = EXTRA_DBITS[dcode];
            if extra != 0 {
                sink.send_bits((dist - BASE_DIST[dcode] as usize) as u32, u32::from(extra));
            }
        }
    }
    sink.send_bits(
        u32::from(STATIC_LTREE_CODE[END_BLOCK]),
        u32::from(STATIC_LTREE_LEN[END_BLOCK]),
    );
}

/// Emit a stored (BTYPE=00) block. `buf` is the literal payload.
pub(crate) fn stored_block(sink: &mut BitSink, buf: &[u8], last: bool) {
    sink.send_bits(u32::from(last), 3); // STORED_BLOCK << 1 | last
    sink.bi_windup();
    let len = buf.len() as u16;
    sink.put_short(len);
    sink.put_short(!len);
    sink.put_bytes(buf);
}

/// Emit an empty fixed-Huffman block (zlib `_tr_align`, used by
/// `Z_PARTIAL_FLUSH`).
pub(crate) fn align_block(sink: &mut BitSink) {
    sink.send_bits(1 << 1, 3); // STATIC_TREES << 1, last = 0
    sink.send_bits(
        u32::from(STATIC_LTREE_CODE[END_BLOCK]),
        u32::from(STATIC_LTREE_LEN[END_BLOCK]),
    );
}

/// zlib `_tr_flush_block`: pick the cheapest of stored / fixed / dynamic and
/// write the block.
///
/// `stored` carries the block's raw bytes when they are still addressable
/// (zlib's `buf != NULL`) together with their length, which is known even when
/// the bytes are not.
pub(crate) struct StoredView<'a> {
    /// `Some(window[block_start..block_start + len])`, or `None` when the raw
    /// bytes are no longer addressable.
    pub buf: Option<&'a [u8]>,
    /// Number of input bytes the block covers.
    pub len: usize,
}

pub(crate) fn flush_block(
    trees: &mut BlockTrees,
    sink: &mut BitSink,
    syms: &[Sym],
    stored: StoredView<'_>,
    last: bool,
    level: u8,
    strategy: Strategy,
) {
    let StoredView {
        buf: stored_buf,
        len: stored_len,
    } = stored;
    let mut max_blindex = 0usize;
    let opt_lenb;
    let static_lenb;

    if level > 0 {
        trees.l_max_code = build_tree(&mut trees.l, &mut trees.heap, &L_DESC);
        trees.d_max_code = build_tree(&mut trees.d, &mut trees.heap, &D_DESC);
        max_blindex = trees.build_bl_tree();

        let mut o = trees.heap.opt_len.wrapping_add(3 + 7) >> 3;
        let s = trees.heap.static_len.wrapping_add(3 + 7) >> 3;
        static_lenb = s;
        // zlib narrows `opt_lenb` to the *cheaper* of dynamic and static only.
        // `Z_FIXED` deliberately does NOT enter this comparison: it is applied
        // below, after the stored-block test, so a `Z_FIXED` stream still
        // stores a block only when a stored block beats the best *coded*
        // encoding — not merely the static one. Folding `Z_FIXED` in here
        // would emit stored blocks zlib emits as fixed whenever
        // `dynamic < stored + 4 <= static` (measured: 40 000 bytes drawn from
        // the 9-bit half of the fixed literal alphabet — dynamic 34 342,
        // stored 40 005, static 44 905 — where zlib emits fixed).
        if s <= o {
            o = s;
        }
        opt_lenb = o;
    } else {
        opt_lenb = stored_len as u64 + 5;
        static_lenb = opt_lenb;
    }

    match stored_buf {
        Some(buf) if stored_len as u64 + 4 <= opt_lenb => stored_block(sink, buf, last),
        _ if strategy == Strategy::Fixed || static_lenb == opt_lenb => {
            sink.send_bits((1 << 1) + u32::from(last), 3);
            compress_block_static(sink, syms);
        }
        _ => {
            sink.send_bits((2 << 1) + u32::from(last), 3);
            trees.send_all_trees(
                sink,
                trees.l_max_code + 1,
                trees.d_max_code + 1,
                max_blindex + 1,
            );
            compress_block(sink, syms, &trees.l, &trees.d);
        }
    }

    trees.init_block();
    if last {
        sink.bi_windup();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bit_sink_packs_lsb_first() {
        let mut s = BitSink::default();
        s.send_bits(0b1, 1);
        s.send_bits(0b10, 2);
        s.send_bits(0b11111, 5);
        assert_eq!(s.bit_valid, 0);
        assert_eq!(s.out, vec![0b1111_1101]);
    }

    #[test]
    fn bi_windup_pads_with_zeros() {
        let mut s = BitSink::default();
        s.send_bits(0b101, 3);
        s.bi_windup();
        assert_eq!(s.out, vec![0b0000_0101]);
        assert_eq!(s.bit_valid, 0);
    }

    #[test]
    fn stored_block_writes_len_and_complement() {
        let mut s = BitSink::default();
        stored_block(&mut s, b"abc", true);
        assert_eq!(s.out[0], 0x01);
        assert_eq!(&s.out[1..5], &[3, 0, 0xFC, 0xFF]);
        assert_eq!(&s.out[5..], b"abc");
    }

    #[test]
    fn all_literal_block_prefers_static_or_dynamic_not_stored() {
        // 300 copies of one byte: dynamic must be far below stored.
        let mut trees = BlockTrees::new();
        let mut sink = BitSink::default();
        let syms: Vec<Sym> = (0..300).map(|_| Sym { dist: 0, lc: b'a' }).collect();
        for s in &syms {
            trees.tally_lit(s.lc);
        }
        let raw = vec![b'a'; 300];
        flush_block(
            &mut trees,
            &mut sink,
            &syms,
            StoredView {
                buf: Some(&raw),
                len: 300,
            },
            true,
            6,
            Strategy::Default,
        );
        assert!(sink.out.len() < 100, "got {} bytes", sink.out.len());
        assert_ne!(sink.out[0] & 0x06, 0, "must not be a stored block");
    }
}
