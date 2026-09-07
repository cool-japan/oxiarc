//! A minimal, independent DEFLATE block walker for encoder tests.
//!
//! It is deliberately *not* built on `oxiarc-deflate`'s own inflater: the point
//! is to observe the encoder's structural decisions (block type, `BFINAL`,
//! literal/match mix) with a second implementation, so a shared bug cannot
//! make a test pass. Only what the encoder can emit is supported; anything
//! else returns `None`.
#![allow(dead_code)]

/// DEFLATE block type (`BTYPE`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockType {
    /// `00` — stored (uncompressed).
    Stored,
    /// `01` — fixed Huffman codes.
    Fixed,
    /// `10` — dynamic Huffman codes.
    Dynamic,
    /// `11` — reserved (an encoder must never emit this).
    Reserved,
}

/// One block of a raw DEFLATE stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Block {
    /// The `BFINAL` bit.
    pub last: bool,
    /// The `BTYPE` field.
    pub btype: BlockType,
    /// Number of literal symbols in the block (bytes for a stored block).
    pub literals: usize,
    /// Number of length/distance pairs in the block.
    pub matches: usize,
    /// Number of uncompressed bytes the block expands to.
    pub uncompressed: usize,
}

struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
    bit: u32,
}

impl<'a> Bits<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            pos: 0,
            bit: 0,
        }
    }

    fn read(&mut self, n: u32) -> Option<u32> {
        let mut v = 0u32;
        for i in 0..n {
            let byte = *self.data.get(self.pos)?;
            v |= u32::from((byte >> self.bit) & 1) << i;
            self.bit += 1;
            if self.bit == 8 {
                self.bit = 0;
                self.pos += 1;
            }
        }
        Some(v)
    }

    fn align(&mut self) {
        if self.bit != 0 {
            self.bit = 0;
            self.pos += 1;
        }
    }

    fn done(&self) -> bool {
        self.pos >= self.data.len()
    }
}

/// A canonical Huffman decoder built from code lengths.
struct Huff {
    counts: [u16; 16],
    symbols: Vec<u16>,
}

impl Huff {
    fn new(lengths: &[u8]) -> Option<Self> {
        let mut counts = [0u16; 16];
        for &l in lengths {
            if usize::from(l) >= 16 {
                return None;
            }
            counts[usize::from(l)] += 1;
        }
        counts[0] = 0;
        let mut offsets = [0u16; 16];
        for i in 1..16 {
            offsets[i] = offsets[i - 1] + counts[i - 1];
        }
        let mut symbols = vec![0u16; lengths.len()];
        for (sym, &l) in lengths.iter().enumerate() {
            if l != 0 {
                symbols[usize::from(offsets[usize::from(l)])] = sym as u16;
                offsets[usize::from(l)] += 1;
            }
        }
        Some(Self { counts, symbols })
    }

    fn decode(&self, bits: &mut Bits) -> Option<u16> {
        let mut code = 0i32;
        let mut first = 0i32;
        let mut index = 0i32;
        for len in 1..16 {
            code |= bits.read(1)? as i32;
            let count = i32::from(self.counts[len]);
            if code - first < count {
                return self.symbols.get((index + (code - first)) as usize).copied();
            }
            index += count;
            first = (first + count) << 1;
            code <<= 1;
        }
        None
    }
}

const LENGTH_BASE: [u16; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
    163, 195, 227, 258,
];
const LENGTH_EXTRA: [u32; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];
const DIST_EXTRA: [u32; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13,
    13,
];
const CODE_LENGTH_ORDER: [usize; 19] = [
    16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
];

fn fixed_trees() -> Option<(Huff, Huff)> {
    let mut lit = [0u8; 288];
    for (i, l) in lit.iter_mut().enumerate() {
        *l = match i {
            0..=143 => 8,
            144..=255 => 9,
            256..=279 => 7,
            _ => 8,
        };
    }
    Some((Huff::new(&lit)?, Huff::new(&[5u8; 30])?))
}

fn dynamic_trees(bits: &mut Bits) -> Option<(Huff, Huff)> {
    let hlit = bits.read(5)? as usize + 257;
    let hdist = bits.read(5)? as usize + 1;
    let hclen = bits.read(4)? as usize + 4;
    let mut cl = [0u8; 19];
    for &slot in CODE_LENGTH_ORDER.iter().take(hclen) {
        cl[slot] = bits.read(3)? as u8;
    }
    let cl_tree = Huff::new(&cl)?;
    let mut lengths = vec![0u8; hlit + hdist];
    let mut i = 0usize;
    while i < lengths.len() {
        let sym = cl_tree.decode(bits)?;
        match sym {
            0..=15 => {
                lengths[i] = sym as u8;
                i += 1;
            }
            16 => {
                let prev = *lengths.get(i.checked_sub(1)?)?;
                let n = 3 + bits.read(2)? as usize;
                for _ in 0..n {
                    *lengths.get_mut(i)? = prev;
                    i += 1;
                }
            }
            17 => {
                let n = 3 + bits.read(3)? as usize;
                i = i.checked_add(n)?;
            }
            18 => {
                let n = 11 + bits.read(7)? as usize;
                i = i.checked_add(n)?;
            }
            _ => return None,
        }
        if i > lengths.len() {
            return None;
        }
    }
    let lit = Huff::new(&lengths[..hlit])?;
    let dist = Huff::new(&lengths[hlit..])?;
    Some((lit, dist))
}

/// Walk `data` (a raw DEFLATE stream) and describe every block.
///
/// Returns `None` if the stream is malformed for this walker.
pub fn walk_blocks(data: &[u8]) -> Option<Vec<Block>> {
    let mut bits = Bits::new(data);
    let mut out = Vec::new();
    loop {
        let last = bits.read(1)? == 1;
        let btype = match bits.read(2)? {
            0 => BlockType::Stored,
            1 => BlockType::Fixed,
            2 => BlockType::Dynamic,
            _ => {
                out.push(Block {
                    last,
                    btype: BlockType::Reserved,
                    literals: 0,
                    matches: 0,
                    uncompressed: 0,
                });
                return Some(out);
            }
        };
        let mut block = Block {
            last,
            btype,
            literals: 0,
            matches: 0,
            uncompressed: 0,
        };
        match btype {
            BlockType::Stored => {
                bits.align();
                let len = bits.read(16)? as usize;
                let nlen = bits.read(16)? as usize;
                if nlen != (!len) & 0xffff {
                    return None;
                }
                if bits.bit != 0 {
                    return None;
                }
                bits.pos = bits.pos.checked_add(len)?;
                if bits.pos > data.len() {
                    return None;
                }
                block.literals = len;
                block.uncompressed = len;
            }
            BlockType::Fixed | BlockType::Dynamic => {
                let (lit, dist) = if btype == BlockType::Fixed {
                    fixed_trees()?
                } else {
                    dynamic_trees(&mut bits)?
                };
                loop {
                    let sym = lit.decode(&mut bits)?;
                    if sym == 256 {
                        break;
                    }
                    if sym < 256 {
                        block.literals += 1;
                        block.uncompressed += 1;
                        continue;
                    }
                    let idx = usize::from(sym) - 257;
                    let base = *LENGTH_BASE.get(idx)?;
                    let extra = bits.read(*LENGTH_EXTRA.get(idx)?)?;
                    let length = usize::from(base) + extra as usize;
                    let dsym = usize::from(dist.decode(&mut bits)?);
                    let dextra = *DIST_EXTRA.get(dsym)?;
                    let _ = bits.read(dextra)?;
                    block.matches += 1;
                    block.uncompressed += length;
                }
            }
            BlockType::Reserved => unreachable!(),
        }
        out.push(block);
        if last {
            break;
        }
        if bits.done() {
            return None;
        }
    }
    Some(out)
}
