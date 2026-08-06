//! LZX decompression for Microsoft Cabinet folders (MS-CAB compression type 3).
//!
//! # Format
//!
//! LZX is an LZ77 variant with three Huffman-coded alphabets and a
//! three-entry least-recently-used queue of repeated match offsets. Its
//! bitstream is described by [MS-PATCH] section 2.2 ("LZX DELTA Compression
//! and Decompression"), whose *base* (non-delta) form is what Cabinet files
//! use, and it is referenced from [MS-CAB] as `tcompTYPE_LZX`.
//!
//! The pieces, in the order the decoder meets them:
//!
//! * **Bitstream.** The compressed byte stream is a sequence of 16-bit
//!   *little-endian* words, and bits are consumed from each word starting at
//!   its most significant bit. This is not a plain MSB-first byte reader —
//!   the first bit of the stream is bit 7 of the *second* byte — and getting
//!   it wrong makes nothing decode at all. See [`BitReader`].
//!
//! * **Stream header.** One bit says whether x86 `CALL` translation is
//!   active; if set, a 32-bit file size follows, high half first. See
//!   [`LzxDecoder::translate_e8`].
//!
//! * **Frames.** Output is produced in 32 KiB *frames*. At every frame
//!   boundary the input bitstream realigns to a 16-bit boundary and `CALL`
//!   translation is applied to that frame's bytes. Blocks may span frames and
//!   a frame may hold several blocks, but a match may never cross a frame
//!   boundary. In a Cabinet each `CFDATA` record carries exactly one frame's
//!   worth of output, so record framing and frame framing normally coincide —
//!   this decoder nevertheless keys off the output count, as the format
//!   specifies, rather than off record boundaries.
//!
//! * **Blocks.** Three types: `VERBATIM` (main and length trees),
//!   `ALIGNED_OFFSET` (main, length and aligned trees) and `UNCOMPRESSED`
//!   (byte-aligned literal payload preceded by three fresh LRU offsets).
//!
//! * **Trees.** Code lengths are *delta coded against the previous block's
//!   lengths* through a 20-symbol pretree, so decoder state has to survive
//!   from block to block — and therefore, in a Cabinet, from one `CFDATA`
//!   record to the next. A decoder that resets per record produces garbage
//!   from the second block onwards.
//!
//! # Testing
//!
//! The tests are spec-derived rather than borrowed: the position-slot tables
//! are regenerated from the recurrence the specification gives, and every
//! bitstream a test decodes is assembled bit by bit by the test itself, so a
//! misreading of the format cannot round-trip past them.

use oxiarc_core::error::{OxiArcError, Result};

/// Output frame size: the bitstream realigns and `CALL` translation applies
/// at every multiple of this many uncompressed bytes.
pub(crate) const FRAME_SIZE: usize = 32768;

/// Smallest window exponent the format allows.
pub(crate) const MIN_WINDOW_BITS: u8 = 15;

/// Largest window exponent the format allows (2 MiB).
pub(crate) const MAX_WINDOW_BITS: u8 = 21;

/// Literal alphabet size; main-tree symbols below this are literals.
const NUM_CHARS: usize = 256;

/// Match lengths 2..=8 are encoded in the main symbol itself; longer matches
/// take a second symbol from the length tree.
const NUM_PRIMARY_LENGTHS: usize = 7;

/// Number of symbols in the length tree.
const NUM_SECONDARY_LENGTHS: usize = 249;

/// Shortest match the format can encode.
const MIN_MATCH: usize = 2;

/// Number of symbols in the pretree used to delta-code code lengths.
const PRETREE_ELEMENTS: usize = 20;

/// Number of symbols in the aligned-offset tree.
const ALIGNED_ELEMENTS: usize = 8;

/// Largest code length any LZX Huffman tree may use.
const MAX_CODE_BITS: u8 = 16;

/// Largest number of position slots (window exponent 21).
const MAX_POSITION_SLOTS: usize = 51;

/// Frames past this count are never `CALL`-translated.
const MAX_TRANSLATED_FRAMES: u32 = 32768;

/// Bytes of synthetic zero padding the reader may invent past the end of the
/// input before it reports truncation.
///
/// The final frame's realignment legitimately asks for one word the encoder
/// never wrote; anything beyond that allowance means the stream really did
/// end early.
const MAX_ZERO_PADDING: usize = 4;

/// Extra verbatim bits carried by each position slot.
///
/// Produced by the recurrence in the specification: slots come in pairs and
/// the bit count rises by one with each pair after the first, saturating at
/// 17. Regenerated and checked by [`tests::position_tables_match_recurrence`].
const EXTRA_BITS: [u8; MAX_POSITION_SLOTS] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13,
    13, 14, 14, 15, 15, 16, 16, 17, 17, 17, 17, 17, 17, 17, 17, 17, 17, 17, 17, 17, 17, 17,
];

/// Base offset of each position slot: the running sum of `1 << EXTRA_BITS[i]`.
const POSITION_BASE: [u32; MAX_POSITION_SLOTS] = [
    0, 1, 2, 3, 4, 6, 8, 12, 16, 24, 32, 48, 64, 96, 128, 192, 256, 384, 512, 768, 1024, 1536,
    2048, 3072, 4096, 6144, 8192, 12288, 16384, 24576, 32768, 49152, 65536, 98304, 131072, 196608,
    262144, 393216, 524288, 655360, 786432, 917504, 1048576, 1179648, 1310720, 1441792, 1572864,
    1703936, 1835008, 1966080, 2097152,
];

/// Number of position slots for a given window exponent.
///
/// Twice the exponent up to 19, then two irregular values the specification
/// states outright.
fn position_slots(window_bits: u8) -> usize {
    match window_bits {
        20 => 42,
        21 => 50,
        other => usize::from(other) * 2,
    }
}

/// Block types (`Block_Type`, three bits).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BlockType {
    /// No block is open; the next thing in the stream is a block header.
    None,
    /// Main and length trees, offsets fully verbatim.
    Verbatim,
    /// Main, length and aligned trees; low offset bits come from the aligned
    /// tree.
    Aligned,
    /// Byte-aligned literal payload.
    Uncompressed,
}

/// Saved [`BitReader`] position, so decoding can resume across calls.
#[derive(Debug, Clone, Copy, Default)]
struct BitState {
    pos: usize,
    buffer: u32,
    bits_left: u32,
    padding: usize,
}

/// Reader over the 16-bit little-endian, MSB-first LZX bitstream.
///
/// `buffer` holds up to 32 buffered bits left justified: the next bit to be
/// consumed is always bit 31.
struct BitReader<'a> {
    input: &'a [u8],
    /// Byte offset of the next word to buffer.
    pos: usize,
    buffer: u32,
    bits_left: u32,
    /// Zero bytes invented past the end of `input`.
    padding: usize,
}

impl<'a> BitReader<'a> {
    fn new(input: &'a [u8], state: BitState) -> Self {
        Self {
            input,
            pos: state.pos,
            buffer: state.buffer,
            bits_left: state.bits_left,
            padding: state.padding,
        }
    }

    fn state(&self) -> BitState {
        BitState {
            pos: self.pos,
            buffer: self.buffer,
            bits_left: self.bits_left,
            padding: self.padding,
        }
    }

    /// Buffer at least `n` bits (`n <= 17`).
    fn ensure(&mut self, n: u32) -> Result<()> {
        debug_assert!(n <= 17, "LZX never needs more than 17 bits at once");
        while self.bits_left < n {
            let word = if self.pos + 1 < self.input.len() {
                let word = u16::from_le_bytes([self.input[self.pos], self.input[self.pos + 1]]);
                self.pos += 2;
                word
            } else if self.pos < self.input.len() {
                // A stream ending on an odd byte is truncated mid-word; the
                // absent high half reads as zero, once.
                let word = u16::from(self.input[self.pos]);
                self.pos += 1;
                self.padding += 1;
                word
            } else {
                if self.padding >= MAX_ZERO_PADDING {
                    return Err(OxiArcError::unexpected_eof(2));
                }
                self.padding += 2;
                0
            };
            // `bits_left <= 16` here: the loop only runs while it is below
            // `n <= 17`, and each pass adds a whole word.
            self.buffer |= u32::from(word) << (16 - self.bits_left);
            self.bits_left += 16;
        }
        Ok(())
    }

    /// Consume `n` already-buffered bits.
    fn remove(&mut self, n: u32) {
        debug_assert!(n <= self.bits_left);
        self.buffer <<= n;
        self.bits_left -= n;
    }

    /// Read `n` bits (`n <= 17`), most significant first.
    fn read_bits(&mut self, n: u32) -> Result<u32> {
        if n == 0 {
            return Ok(0);
        }
        self.ensure(n)?;
        let value = self.buffer >> (32 - n);
        self.remove(n);
        Ok(value)
    }

    /// Realign to a 16-bit boundary at a frame boundary.
    ///
    /// Partial bits of the current word are discarded. When nothing at all is
    /// buffered the stream is already aligned and nothing is read.
    fn align_frame(&mut self) -> Result<()> {
        if self.bits_left > 0 {
            self.ensure(16)?;
        }
        let stale = self.bits_left & 15;
        if stale != 0 {
            self.remove(stale);
        }
        Ok(())
    }

    /// Discard every buffered bit and report the offset of the next unread
    /// byte.
    ///
    /// Used by uncompressed blocks, whose payload is read as raw bytes. The
    /// specification calls for 1 to 16 bits of padding, so a stream that
    /// happens to have nothing buffered still skips a whole word.
    fn byte_align(&mut self) -> Result<usize> {
        if self.bits_left == 0 {
            self.ensure(16)?;
        }
        self.buffer = 0;
        self.bits_left = 0;
        Ok(self.pos)
    }

    /// Reposition the byte cursor after a raw-byte read.
    fn seek_bytes(&mut self, pos: usize) -> Result<()> {
        if pos > self.input.len() {
            return Err(OxiArcError::unexpected_eof(pos - self.input.len()));
        }
        self.pos = pos;
        Ok(())
    }
}

/// Canonical Huffman decoding table built from a code-length vector.
///
/// LZX assigns codes in increasing symbol order within each length, shortest
/// length first — the same canonical rule DEFLATE uses, but read most
/// significant bit first.
#[derive(Clone)]
struct HuffTable {
    /// Number of codes of each length, indexed by length.
    counts: [u32; MAX_CODE_BITS as usize + 1],
    /// Symbols ordered by (length, symbol).
    symbols: Vec<u16>,
    /// Longest code present; zero when the table has no codes at all.
    max_bits: u32,
}

impl HuffTable {
    /// Build from code lengths.
    ///
    /// An all-zero length vector yields an empty table: the format permits it
    /// (an aligned tree the block never consults), and decoding from such a
    /// table is an error rather than a silent zero symbol. Any other
    /// incomplete or over-subscribed length set is rejected.
    fn build(lengths: &[u8]) -> Result<Self> {
        let mut counts = [0u32; MAX_CODE_BITS as usize + 1];
        let mut max_bits = 0u32;
        for &len in lengths {
            if len > MAX_CODE_BITS {
                return Err(OxiArcError::corrupted(
                    0,
                    format!("LZX code length {len} exceeds the {MAX_CODE_BITS}-bit maximum"),
                ));
            }
            if len > 0 {
                counts[usize::from(len)] += 1;
                max_bits = max_bits.max(u32::from(len));
            }
        }

        if max_bits == 0 {
            return Ok(Self {
                counts,
                symbols: Vec::new(),
                max_bits: 0,
            });
        }

        // Kraft equality: an LZX code must be exactly complete.
        let mut left: i64 = 1;
        for &count in &counts[1..=MAX_CODE_BITS as usize] {
            left <<= 1;
            left -= i64::from(count);
            if left < 0 {
                return Err(OxiArcError::corrupted(
                    0,
                    "LZX Huffman table is over-subscribed",
                ));
            }
        }
        if left != 0 {
            return Err(OxiArcError::corrupted(0, "LZX Huffman table is incomplete"));
        }

        let mut offsets = [0u32; MAX_CODE_BITS as usize + 2];
        for len in 1..=MAX_CODE_BITS as usize {
            offsets[len + 1] = offsets[len] + counts[len];
        }
        let mut symbols = vec![0u16; offsets[MAX_CODE_BITS as usize + 1] as usize];
        for (symbol, &len) in lengths.iter().enumerate() {
            if len > 0 {
                let slot = &mut offsets[usize::from(len)];
                symbols[*slot as usize] = symbol as u16;
                *slot += 1;
            }
        }

        Ok(Self {
            counts,
            symbols,
            max_bits,
        })
    }

    /// Decode one symbol.
    fn decode(&self, reader: &mut BitReader<'_>) -> Result<u16> {
        if self.max_bits == 0 {
            return Err(OxiArcError::corrupted(
                0,
                "LZX stream uses a Huffman tree that was never defined",
            ));
        }
        reader.ensure(self.max_bits)?;
        let window = reader.buffer;
        let mut first = 0u32;
        let mut index = 0u32;
        for len in 1..=self.max_bits {
            let code = window >> (32 - len);
            let count = self.counts[len as usize];
            if code < first + count {
                reader.remove(len);
                let at = (index + (code - first)) as usize;
                return self.symbols.get(at).copied().ok_or_else(|| {
                    OxiArcError::corrupted(0, "LZX Huffman symbol index out of range")
                });
            }
            index += count;
            first = (first + count) << 1;
        }
        Err(OxiArcError::invalid_huffman(0))
    }
}

/// Streaming LZX decoder for one Cabinet folder.
///
/// Everything that must survive block and `CFDATA` boundaries lives here: the
/// sliding window, the repeated-offset queue, the main and length code
/// lengths, the `CALL`-translation flags and the bit reader's position.
pub(crate) struct LzxDecoder {
    window: Vec<u8>,
    window_size: usize,
    window_posn: usize,
    /// Repeated-offset LRU queue (`R0`, `R1`, `R2`).
    repeated: [u32; 3],
    main_lengths: Vec<u8>,
    length_lengths: Vec<u8>,
    aligned_lengths: [u8; ALIGNED_ELEMENTS],
    main_table: HuffTable,
    length_table: HuffTable,
    aligned_table: HuffTable,
    main_symbols: usize,
    block_type: BlockType,
    /// Bytes of the open block still to be produced.
    block_remaining: u32,
    /// Length of the open block, needed for the uncompressed padding byte.
    block_length: u32,
    header_read: bool,
    intel_filesize: i32,
    intel_started: bool,
    frames_read: u32,
    /// Uncompressed bytes produced so far.
    output_offset: u64,
    bits: BitState,
}

impl LzxDecoder {
    /// Create a decoder for a window of `1 << window_bits` bytes.
    ///
    /// `window_bits` comes straight out of `CFFOLDER.typeCompress` and is
    /// therefore attacker controlled; exponents outside the range the format
    /// defines are rejected here rather than turned into a huge allocation.
    pub(crate) fn new(window_bits: u8) -> Result<Self> {
        if !(MIN_WINDOW_BITS..=MAX_WINDOW_BITS).contains(&window_bits) {
            return Err(OxiArcError::invalid_header(format!(
                "LZX window exponent {window_bits} is outside the supported range \
                 {MIN_WINDOW_BITS}..={MAX_WINDOW_BITS}"
            )));
        }
        let window_size = 1usize << window_bits;
        let main_symbols = NUM_CHARS + position_slots(window_bits) * 8;

        let mut window = Vec::new();
        window
            .try_reserve_exact(window_size)
            .map_err(|_| OxiArcError::memory_budget_exceeded(window_size, window_size))?;
        window.resize(window_size, 0);

        Ok(Self {
            window,
            window_size,
            window_posn: 0,
            repeated: [1, 1, 1],
            main_lengths: vec![0u8; main_symbols],
            length_lengths: vec![0u8; NUM_SECONDARY_LENGTHS],
            aligned_lengths: [0u8; ALIGNED_ELEMENTS],
            main_table: HuffTable::build(&[])?,
            length_table: HuffTable::build(&[])?,
            aligned_table: HuffTable::build(&[])?,
            main_symbols,
            block_type: BlockType::None,
            block_remaining: 0,
            block_length: 0,
            header_read: false,
            intel_filesize: 0,
            intel_started: false,
            frames_read: 0,
            output_offset: 0,
            bits: BitState::default(),
        })
    }

    /// Bytes of the compressed stream consumed so far.
    ///
    /// Meaningful at a frame boundary, where the format guarantees the
    /// bitstream is 16-bit aligned; buffered-but-unconsumed bits are not
    /// counted.
    ///
    /// Exists so tests can assert that frame boundaries land on the byte
    /// offsets the format requires — the property that distinguishes a
    /// correct bit reader from one that merely produces plausible output.
    #[cfg(test)]
    pub(crate) fn input_position(&self) -> usize {
        self.bits.pos - (self.bits.bits_left as usize / 8)
    }

    /// Produce the next `out_len` uncompressed bytes.
    ///
    /// `input` must be the *whole* compressed stream for the folder on every
    /// call; the decoder tracks its own position within it, so successive
    /// calls continue where the previous one stopped. `out_len` must be a
    /// multiple of [`FRAME_SIZE`] unless this is the final call, because the
    /// bitstream realigns only at frame boundaries.
    pub(crate) fn decompress(&mut self, input: &[u8], out_len: usize) -> Result<Vec<u8>> {
        let mut output = Vec::new();
        output
            .try_reserve_exact(out_len)
            .map_err(|_| OxiArcError::memory_budget_exceeded(out_len, out_len))?;

        let mut reader = BitReader::new(input, self.bits);
        let mut produced = 0usize;

        while produced < out_len {
            if !self.header_read {
                if reader.read_bits(1)? != 0 {
                    let high = reader.read_bits(16)?;
                    let low = reader.read_bits(16)?;
                    self.intel_filesize = ((high << 16) | low) as i32;
                }
                self.header_read = true;
            }

            let frame_size = FRAME_SIZE.min(out_len - produced);
            let mut todo = frame_size;

            while todo > 0 {
                if self.block_remaining == 0 {
                    self.read_block_header(&mut reader)?;
                }
                let run = todo.min(self.block_remaining as usize);
                match self.block_type {
                    BlockType::Verbatim | BlockType::Aligned => {
                        self.decode_symbols(&mut reader, run)?;
                    }
                    BlockType::Uncompressed => self.copy_uncompressed(&mut reader, run)?,
                    BlockType::None => {
                        return Err(OxiArcError::corrupted(0, "LZX block header not read"));
                    }
                }
                self.block_remaining -= run as u32;
                todo -= run;

                if self.block_remaining == 0 {
                    if self.block_type == BlockType::Uncompressed && (self.block_length & 1) == 1 {
                        // Odd-length uncompressed payloads are padded to a word.
                        let pos = (reader.pos + 1).min(reader.input.len());
                        reader.seek_bytes(pos)?;
                    }
                    self.block_type = BlockType::None;
                }
            }

            // A frame never wraps the window: the window is a power of two of
            // at least 32 KiB, so `FRAME_SIZE` divides it exactly.
            let start = if self.window_posn >= frame_size {
                self.window_posn - frame_size
            } else {
                self.window_size - frame_size
            };
            let mut frame = self.window[start..start + frame_size].to_vec();
            self.translate_e8(&mut frame);
            output.extend_from_slice(&frame);

            produced += frame_size;
            self.output_offset += frame_size as u64;
            self.frames_read = self.frames_read.saturating_add(1);
            reader.align_frame()?;
        }

        self.bits = reader.state();
        Ok(output)
    }

    /// Read a block header and, for compressed blocks, the code-length trees.
    fn read_block_header(&mut self, reader: &mut BitReader<'_>) -> Result<()> {
        let raw = reader.read_bits(3)?;
        let high = reader.read_bits(16)?;
        let low = reader.read_bits(8)?;
        let length = (high << 8) | low;
        if length == 0 {
            return Err(OxiArcError::corrupted(0, "LZX block declares zero length"));
        }
        self.block_length = length;
        self.block_remaining = length;

        match raw {
            1 => {
                self.block_type = BlockType::Verbatim;
                self.read_compressed_trees(reader, false)
            }
            2 => {
                self.block_type = BlockType::Aligned;
                self.read_compressed_trees(reader, true)
            }
            3 => {
                self.block_type = BlockType::Uncompressed;
                // An uncompressed block may hold any byte, so `CALL`
                // translation has to be assumed live from here on.
                self.intel_started = true;
                self.read_uncompressed_header(reader)
            }
            other => Err(OxiArcError::corrupted(
                0,
                format!("LZX block type {other} is not defined"),
            )),
        }
    }

    /// Read the three stored LRU offsets that open an uncompressed block.
    fn read_uncompressed_header(&mut self, reader: &mut BitReader<'_>) -> Result<()> {
        let input = reader.input;
        let mut pos = reader.byte_align()?;
        if pos + 12 > input.len() {
            return Err(OxiArcError::unexpected_eof(12));
        }
        for slot in &mut self.repeated {
            // A stored zero is left as-is; `copy_match` rejects it with a
            // typed error rather than silently substituting a distance.
            *slot =
                u32::from_le_bytes([input[pos], input[pos + 1], input[pos + 2], input[pos + 3]]);
            pos += 4;
        }
        reader.seek_bytes(pos)
    }

    /// Read the aligned (optional), main and length trees for a block.
    fn read_compressed_trees(&mut self, reader: &mut BitReader<'_>, aligned: bool) -> Result<()> {
        if aligned {
            for slot in &mut self.aligned_lengths {
                *slot = reader.read_bits(3)? as u8;
            }
            self.aligned_table = HuffTable::build(&self.aligned_lengths)?;
        }

        let mut main = std::mem::take(&mut self.main_lengths);
        let mut result = read_lengths(reader, &mut main, 0, NUM_CHARS);
        if result.is_ok() {
            result = read_lengths(reader, &mut main, NUM_CHARS, self.main_symbols);
        }
        self.main_lengths = main;
        result?;
        self.main_table = HuffTable::build(&self.main_lengths)?;
        // Once a literal 0xE8 becomes encodable, `CALL` translation is live
        // for the rest of the stream.
        if self.main_lengths[0xE8] != 0 {
            self.intel_started = true;
        }

        let mut lengths = std::mem::take(&mut self.length_lengths);
        let result = read_lengths(reader, &mut lengths, 0, NUM_SECONDARY_LENGTHS);
        self.length_lengths = lengths;
        result?;
        self.length_table = HuffTable::build(&self.length_lengths)?;
        Ok(())
    }

    /// Decode `count` bytes of a verbatim or aligned-offset block.
    fn decode_symbols(&mut self, reader: &mut BitReader<'_>, count: usize) -> Result<()> {
        let aligned = self.block_type == BlockType::Aligned;
        let mut remaining = count;

        while remaining > 0 {
            let symbol = usize::from(self.main_table.decode(reader)?);
            if symbol < NUM_CHARS {
                self.push_byte(symbol as u8);
                remaining -= 1;
                continue;
            }

            let symbol = symbol - NUM_CHARS;
            let mut match_length = symbol & 7;
            if match_length == NUM_PRIMARY_LENGTHS {
                let footer = usize::from(self.length_table.decode(reader)?);
                if footer >= NUM_SECONDARY_LENGTHS {
                    return Err(OxiArcError::corrupted(
                        0,
                        "LZX length-tree symbol is out of range",
                    ));
                }
                match_length += footer;
            }
            match_length += MIN_MATCH;

            let offset = self.decode_offset(reader, symbol >> 3, aligned)?;

            // A match may cross neither a block nor a frame boundary, and
            // `remaining` is the smaller of the two limits.
            if match_length > remaining {
                return Err(OxiArcError::corrupted(
                    0,
                    format!(
                        "LZX match of {match_length} bytes overruns its block or frame by {}",
                        match_length - remaining
                    ),
                ));
            }
            self.copy_match(offset, match_length)?;
            remaining -= match_length;
        }
        Ok(())
    }

    /// Resolve a match offset from its position slot, updating the LRU queue.
    fn decode_offset(
        &mut self,
        reader: &mut BitReader<'_>,
        slot: usize,
        aligned: bool,
    ) -> Result<u32> {
        if slot < 3 {
            // Slots 0..2 name an entry of the repeated-offset queue: slot 0
            // leaves it untouched, slots 1 and 2 promote their entry.
            let offset = self.repeated[slot];
            if slot != 0 {
                self.repeated.swap(0, slot);
            }
            return Ok(offset);
        }

        let offset = if slot == 3 {
            1
        } else {
            let extra = u32::from(*EXTRA_BITS.get(slot).ok_or_else(|| {
                OxiArcError::corrupted(0, format!("LZX position slot {slot} does not exist"))
            })?);
            let base = POSITION_BASE[slot];
            let formatted = if aligned && extra > 3 {
                let verbatim = reader.read_bits(extra - 3)? << 3;
                let low = u32::from(self.aligned_table.decode(reader)?);
                base + verbatim + low
            } else if aligned && extra == 3 {
                base + u32::from(self.aligned_table.decode(reader)?)
            } else {
                base + reader.read_bits(extra)?
            };
            // Encoded distances start at 3 because slots 0..2 are the LRU
            // codes, so the stored value is the distance plus two.
            formatted.saturating_sub(2)
        };

        self.repeated[2] = self.repeated[1];
        self.repeated[1] = self.repeated[0];
        self.repeated[0] = offset;
        Ok(offset)
    }

    /// Append one byte to the sliding window.
    fn push_byte(&mut self, byte: u8) {
        self.window[self.window_posn] = byte;
        self.window_posn += 1;
        if self.window_posn == self.window_size {
            self.window_posn = 0;
        }
    }

    /// Copy a match from the sliding window.
    fn copy_match(&mut self, offset: u32, length: usize) -> Result<()> {
        let offset = offset as usize;
        if offset == 0 || offset > self.window_size {
            return Err(OxiArcError::invalid_distance(offset, self.window_size));
        }
        // Nothing before the start of the stream may be referenced: the
        // window is zero filled, and handing those zeros back would be
        // silent corruption rather than an error.
        let produced = self.output_offset + self.window_posn as u64;
        if offset as u64 > produced {
            return Err(OxiArcError::invalid_distance(offset, produced as usize));
        }
        let mut src = (self.window_posn + self.window_size - offset) % self.window_size;
        for _ in 0..length {
            let byte = self.window[src];
            self.push_byte(byte);
            src += 1;
            if src == self.window_size {
                src = 0;
            }
        }
        Ok(())
    }

    /// Copy `count` bytes of an uncompressed block's payload.
    fn copy_uncompressed(&mut self, reader: &mut BitReader<'_>, count: usize) -> Result<()> {
        let input = reader.input;
        let start = reader.pos;
        let end = start
            .checked_add(count)
            .ok_or_else(|| OxiArcError::corrupted(0, "LZX uncompressed block length overflows"))?;
        if end > input.len() {
            return Err(OxiArcError::unexpected_eof(end - input.len()));
        }
        for &byte in &input[start..end] {
            self.push_byte(byte);
        }
        reader.seek_bytes(end)
    }

    /// Apply x86 `CALL` translation to one output frame.
    ///
    /// The encoder rewrites the 32-bit displacement of every `E8` byte it
    /// finds into an absolute address so repeated calls to the same target
    /// compress alike; this undoes it. Translation is applied to the *output
    /// copy* only — the sliding window keeps the untranslated bytes, because
    /// later matches refer to those.
    fn translate_e8(&mut self, frame: &mut [u8]) {
        if self.intel_filesize == 0
            || !self.intel_started
            || frame.len() <= 10
            || self.frames_read >= MAX_TRANSLATED_FRAMES
        {
            return;
        }
        let file_size = self.intel_filesize;
        // Bounded by `MAX_TRANSLATED_FRAMES * FRAME_SIZE` (1 GiB), so this
        // cast cannot lose information.
        let frame_start = self.output_offset as i32;

        let mut index = 0usize;
        // The last ten bytes are never scanned: a displacement must fit
        // entirely inside the frame.
        let limit = frame.len() - 10;
        while index < limit {
            if frame[index] != 0xE8 {
                index += 1;
                continue;
            }
            let position = frame_start.wrapping_add(index as i32);
            let absolute = i32::from_le_bytes([
                frame[index + 1],
                frame[index + 2],
                frame[index + 3],
                frame[index + 4],
            ]);
            if absolute >= -position && absolute < file_size {
                let relative = if absolute >= 0 {
                    absolute.wrapping_sub(position)
                } else {
                    absolute.wrapping_add(file_size)
                };
                frame[index + 1..index + 5].copy_from_slice(&relative.to_le_bytes());
            }
            index += 5;
        }
    }
}

/// Decode code lengths for `lengths[first..last]`.
///
/// Lengths are delta coded against whatever the previous block left in
/// `lengths`, through a 20-symbol pretree whose own lengths are four bits
/// each. Symbols 0..=16 are deltas, 17 and 18 are zero runs of different
/// magnitudes, and 19 is a short run of one repeated delta.
fn read_lengths(
    reader: &mut BitReader<'_>,
    lengths: &mut [u8],
    first: usize,
    last: usize,
) -> Result<()> {
    if last > lengths.len() || first > last {
        return Err(OxiArcError::corrupted(
            0,
            "LZX code-length range is out of bounds",
        ));
    }

    let mut pretree_lengths = [0u8; PRETREE_ELEMENTS];
    for slot in &mut pretree_lengths {
        *slot = reader.read_bits(4)? as u8;
    }
    let pretree = HuffTable::build(&pretree_lengths)?;

    let mut index = first;
    while index < last {
        let symbol = pretree.decode(reader)?;
        match symbol {
            17 => {
                let run = reader.read_bits(4)? as usize + 4;
                let end = (index + run).min(last);
                lengths[index..end].fill(0);
                index = end;
            }
            18 => {
                let run = reader.read_bits(5)? as usize + 20;
                let end = (index + run).min(last);
                lengths[index..end].fill(0);
                index = end;
            }
            19 => {
                let run = reader.read_bits(1)? as usize + 4;
                let delta = pretree.decode(reader)?;
                if delta > 16 {
                    return Err(OxiArcError::corrupted(
                        0,
                        "LZX pretree run repeats a non-delta symbol",
                    ));
                }
                let value = apply_delta(lengths[index], delta as u8);
                let end = (index + run).min(last);
                lengths[index..end].fill(value);
                index = end;
            }
            delta if delta <= 16 => {
                lengths[index] = apply_delta(lengths[index], delta as u8);
                index += 1;
            }
            other => {
                return Err(OxiArcError::corrupted(
                    0,
                    format!("LZX pretree symbol {other} is not defined"),
                ));
            }
        }
    }
    Ok(())
}

/// Apply one pretree delta to a previous code length.
///
/// The new length is `previous - delta` reduced modulo 17, which keeps every
/// result inside the 0..=16 range a code length may take.
fn apply_delta(previous: u8, delta: u8) -> u8 {
    let value = i16::from(previous) - i16::from(delta);
    if value < 0 {
        (value + 17) as u8
    } else {
        value as u8
    }
}

#[cfg(test)]
#[path = "lzx_tests.rs"]
mod tests;

#[cfg(test)]
pub(super) use tests::cab_two_record_fixture;
