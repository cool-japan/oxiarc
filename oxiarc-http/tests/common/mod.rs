//! Fixtures shared by the `oxiarc-http` integration tests.
//!
//! The important item here is [`single_block_bomb`]. The whole
//! `oxiarc-http` bomb design rests on one measured claim — *a single DEFLATE
//! block can expand 812 KB into 123 MiB, so a cap checked between blocks is
//! not a cap* — and that claim's only evidence in the design report was a
//! throwaway script (critique §5, item 1). This generator makes it durable
//! and self-checking: it builds exactly that stream, from first principles,
//! in Rust.

#![allow(dead_code)]

/// An LSB-first DEFLATE bit writer.
///
/// DEFLATE packs bits into bytes starting at the least significant bit, but
/// Huffman codes are packed most-significant-bit first (RFC 1951 §3.1.1).
/// The two spellings are separate methods so neither is used by accident.
#[derive(Default)]
pub struct BitWriter {
    out: Vec<u8>,
    bits: u32,
    count: u32,
}

impl BitWriter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Write `n` bits of `value`, least significant bit first. For header
    /// fields such as `BFINAL`/`BTYPE` and for Huffman *extra* bits.
    pub fn write_bits(&mut self, value: u32, n: u32) {
        for index in 0..n {
            self.push_bit((value >> index) & 1);
        }
    }

    /// Write an `n`-bit Huffman code, most significant bit first.
    pub fn write_code(&mut self, code: u32, n: u32) {
        for index in (0..n).rev() {
            self.push_bit((code >> index) & 1);
        }
    }

    fn push_bit(&mut self, bit: u32) {
        self.bits |= bit << self.count;
        self.count += 1;
        if self.count == 8 {
            self.out.push(self.bits as u8);
            self.bits = 0;
            self.count = 0;
        }
    }

    /// Pad to a byte boundary with zeros and return the stream.
    pub fn finish(mut self) -> Vec<u8> {
        if self.count > 0 {
            self.out.push(self.bits as u8);
        }
        self.out
    }
}

/// The RFC 1951 §3.2.6 fixed literal/length code for `symbol`, as
/// `(code, bit_length)`.
fn fixed_litlen(symbol: u32) -> (u32, u32) {
    match symbol {
        0..=143 => (0x30 + symbol, 8),
        144..=255 => (0x190 + (symbol - 144), 9),
        256..=279 => (symbol - 256, 7),
        _ => (0xC0 + (symbol - 280), 8),
    }
}

/// A raw DEFLATE stream of **one** fixed-Huffman block that decodes to
/// `output_len` bytes (rounded up to the next multiple of 258 plus one).
///
/// Shape: one literal `0x00`, then a run of maximum-length back-references
/// (length 258, distance 1), then end-of-block. Each reference costs 13 bits
/// — an 8-bit length code 285 with no extra bits, plus a 5-bit distance code
/// 0 — and yields 258 bytes, so the stream expands by a factor of
/// `258 * 8 / 13 = 158.8`, *inside a single block*. That is the whole point:
/// a decoder that checks its output budget at block boundaries checks it
/// exactly once, after the damage.
///
/// The result is `\0` repeated, which is what a length-258/distance-1 run of
/// a leading NUL produces.
pub fn single_block_bomb(output_len: usize) -> Vec<u8> {
    let matches = output_len.saturating_sub(1).div_ceil(258);
    let mut w = BitWriter::new();
    w.write_bits(1, 1); // BFINAL = 1
    w.write_bits(1, 2); // BTYPE  = 01, fixed Huffman

    let (lit, lit_bits) = fixed_litlen(0);
    w.write_code(lit, lit_bits);

    let (len258, len_bits) = fixed_litlen(285); // length 258, zero extra bits
    for _ in 0..matches {
        w.write_code(len258, len_bits);
        w.write_code(0, 5); // distance code 0 => distance 1
    }

    let (eob, eob_bits) = fixed_litlen(256);
    w.write_code(eob, eob_bits);
    w.finish()
}

/// How many bytes [`single_block_bomb`] actually decodes to.
pub fn single_block_bomb_output_len(output_len: usize) -> usize {
    1 + output_len.saturating_sub(1).div_ceil(258) * 258
}

/// Wrap a raw DEFLATE stream in an RFC 1950 zlib container with a correct
/// Adler-32 over `plain`.
pub fn zlib_wrap(raw: &[u8], plain: &[u8]) -> Vec<u8> {
    let mut out = vec![0x78, 0x9C];
    out.extend_from_slice(raw);
    let (mut a, mut b) = (1u32, 0u32);
    for &byte in plain {
        a = (a + u32::from(byte)) % 65521;
        b = (b + a) % 65521;
    }
    out.extend_from_slice(&((b << 16) | a).to_be_bytes());
    out
}

/// Wrap a raw DEFLATE stream in an RFC 1952 gzip container with a correct
/// CRC-32 and `ISIZE` over `plain`.
pub fn gzip_wrap(raw: &[u8], plain: &[u8]) -> Vec<u8> {
    let mut out = vec![0x1F, 0x8B, 0x08, 0x00, 0, 0, 0, 0, 0x00, 0xFF];
    out.extend_from_slice(raw);
    let crc = oxiarc_core::Crc32::compute(plain);
    out.extend_from_slice(&crc.to_le_bytes());
    out.extend_from_slice(&(plain.len() as u32).to_le_bytes());
    out
}

/// Every optional gzip header field, for the flag-coverage tests.
#[derive(Default, Clone)]
pub struct GzipHeaderFields {
    pub extra: Option<Vec<u8>>,
    pub name: Option<Vec<u8>>,
    pub comment: Option<Vec<u8>>,
    pub header_crc: bool,
    /// Raw `FLG` bits to OR in, for the reserved-bit conformance test.
    pub extra_flags: u8,
}

/// Build a gzip member with the requested optional header fields around a
/// raw DEFLATE payload.
///
/// Hand-rolled rather than generated by `python3 -m gzip`, because CPython's
/// `gzip` module can set `FNAME` and nothing else — no `FEXTRA`, no
/// `FCOMMENT`, no `FHCRC`.
pub fn gzip_member(raw: &[u8], plain: &[u8], fields: &GzipHeaderFields) -> Vec<u8> {
    let mut flg = fields.extra_flags;
    if fields.header_crc {
        flg |= 0x02; // FHCRC
    }
    if fields.extra.is_some() {
        flg |= 0x04; // FEXTRA
    }
    if fields.name.is_some() {
        flg |= 0x08; // FNAME
    }
    if fields.comment.is_some() {
        flg |= 0x10; // FCOMMENT
    }

    let mut header = vec![0x1F, 0x8B, 0x08, flg, 0, 0, 0, 0, 0x00, 0xFF];
    if let Some(extra) = &fields.extra {
        header.extend_from_slice(&(extra.len() as u16).to_le_bytes());
        header.extend_from_slice(extra);
    }
    if let Some(name) = &fields.name {
        header.extend_from_slice(name);
        header.push(0);
    }
    if let Some(comment) = &fields.comment {
        header.extend_from_slice(comment);
        header.push(0);
    }
    if fields.header_crc {
        let crc = oxiarc_core::Crc32::compute(&header);
        header.extend_from_slice(&(crc as u16).to_le_bytes());
    }

    let mut out = header;
    out.extend_from_slice(raw);
    out.extend_from_slice(&oxiarc_core::Crc32::compute(plain).to_le_bytes());
    out.extend_from_slice(&(plain.len() as u32).to_le_bytes());
    out
}

/// A raw DEFLATE stream (no container) of `plain`, for the wrappers above.
pub fn raw_deflate(plain: &[u8]) -> Vec<u8> {
    oxiarc_deflate::deflate(plain, 6).expect("deflate")
}

/// The payload set every round-trip test walks.
///
/// Deliberately includes the empty body (a legal, encodable zero-length
/// representation), a single byte, structured text, incompressible random
/// bytes, and a highly repetitive body whose legitimate expansion ratio is
/// well past 100x.
pub fn payloads() -> Vec<(&'static str, Vec<u8>)> {
    vec![
        ("empty", Vec::new()),
        ("one byte", b"x".to_vec()),
        ("1 KiB text", text(1024)),
        ("64 KiB json", json(64 * 1024)),
        ("64 KiB random", pseudo_random(64 * 1024)),
        ("64 KiB repetitive", vec![b'A'; 64 * 1024]),
        (
            "window crossing",
            // Larger than a 32 KiB DEFLATE window and than the 64 KiB
            // staging buffers, so every buffer boundary is crossed.
            pseudo_random(200_000),
        ),
    ]
}

/// Deterministic pseudo-random bytes (xorshift64*), so a failure is
/// reproducible without a fixture file.
pub fn pseudo_random(len: usize) -> Vec<u8> {
    let mut state = 0x2545_F491_4F6C_DD1Du64;
    let mut out = Vec::with_capacity(len);
    while out.len() < len {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        out.extend_from_slice(&state.to_le_bytes());
    }
    out.truncate(len);
    out
}

/// Repetitive English-ish text.
pub fn text(len: usize) -> Vec<u8> {
    let unit = b"the quick brown fox jumps over the lazy dog. ";
    let mut out = Vec::with_capacity(len + unit.len());
    while out.len() < len {
        out.extend_from_slice(unit);
    }
    out.truncate(len);
    out
}

/// Repetitive JSON-shaped text, the realistic high-ratio API payload.
pub fn json(len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len + 64);
    out.push(b'[');
    let mut index = 0u32;
    while out.len() < len {
        out.extend_from_slice(
            format!(r#"{{"id":{index},"name":"row {index}","ok":true}},"#).as_bytes(),
        );
        index += 1;
    }
    out.truncate(len);
    out
}
