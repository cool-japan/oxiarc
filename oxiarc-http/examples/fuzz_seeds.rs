//! Seed-corpus generator for the seven `oxiarc-http` fuzz targets.
//!
//! The targets themselves live in the workspace `fuzz/` crate (added by the
//! Phase 8 wave-3 workspace track); this is the generator that gives each of
//! them a corpus that already reaches the interesting states, so the fuzzer
//! spends its budget past the header parser rather than rediscovering the
//! two bytes of a gzip magic.
//!
//! ```text
//! # write every corpus under fuzz/corpus/<target>/
//! cargo run -p oxiarc-http --example fuzz_seeds --all-features -- fuzz/corpus
//!
//! # or into a scratch directory (the default is the system temp dir)
//! cargo run -p oxiarc-http --example fuzz_seeds --all-features
//! ```
//!
//! `fuzz/corpus/` is git-ignored (root `TODO.md` Known Issue 9), so seeds are
//! regenerated, never committed — which is why this generator is the durable
//! artefact and the corpus is not.
//!
//! The invariants the targets assert are pinned as real tests in
//! `tests/fuzz_seeds.rs`, so a seed that stops being interesting fails the
//! suite rather than silently rotting.

use std::path::{Path, PathBuf};

/// One seed input: a file name and its bytes.
type Seed = (String, Vec<u8>);

/// One fuzz target's corpus: the target's name and its seeds.
type Corpus = (&'static str, Vec<Seed>);

/// The seven targets, each with its seed inputs.
fn corpora() -> Vec<Corpus> {
    vec![
        ("fuzz_http_parse_accept_encoding", accept_encoding_seeds()),
        ("fuzz_http_parse_content_encoding", content_encoding_seeds()),
        ("fuzz_http_qvalue", qvalue_seeds()),
        ("fuzz_http_decoder_gzip", gzip_seeds()),
        ("fuzz_http_decoder_deflate", deflate_seeds()),
        ("fuzz_http_decoder_chunked", chunked_seeds()),
        ("fuzz_http_negotiate", negotiate_seeds()),
    ]
}

/// RFC 9110 §12.5.3's own examples plus the shapes that historically break
/// list parsers.
pub fn accept_encoding_seeds() -> Vec<Seed> {
    named([
        "compress, gzip",
        "",
        "*",
        "compress;q=0.5, gzip;q=1.0",
        "gzip;q=1.0, identity; q=0.5, *;q=0",
        "gzip,,,br",
        "foo , ,bar,charlie",
        "GZIP, Br, X-GZIP",
        "gzip;Q=0.5",
        "gzip;q=0.0001",
        "gzip;q=1.5",
        "gzip;q=abc",
        "*;q=0, identity;q=0",
        "*;q=0.5, gzip;q=0.1",
        "gzip;q=0.5, gzip;q=0.9",
        "br;q=1, zstd;q=1, gzip;q=1, deflate;q=1, identity;q=0",
        // The DoS bound: 64 raw elements is the cap, so 65 must be refused.
        &",".repeat(65),
        "\u{feff}gzip",
        "gzip;q=0.5;extra=1",
    ])
}

/// `Content-Encoding` values, including the chained and hostile ones.
pub fn content_encoding_seeds() -> Vec<Seed> {
    named([
        "gzip",
        "x-gzip",
        "deflate",
        "br",
        "zstd",
        "dcb",
        "dcz",
        "identity",
        "",
        "gzip, br",
        "br, gzip",
        "gzip, gzip",
        "identity, gzip",
        "gzip, unknown",
        "gzip, gzip, gzip, gzip, gzip",
        "compress",
        "GZIP , Br",
        "gzip;charset=utf-8",
    ])
}

/// RFC 9110 §12.4.2 quality values, valid and not.
pub fn qvalue_seeds() -> Vec<Seed> {
    named([
        "0",
        "0.0",
        "0.000",
        "1",
        "1.0",
        "1.000",
        "0.5",
        "0.001",
        "0.999",
        "1.001",
        "1.5",
        "0.0001",
        "-0.5",
        "abc",
        "",
        ".",
        "0.",
        "q=0.5",
        "Q=0.5",
        " q = 0.5 ",
    ])
}

/// gzip bodies: valid, multi-member, every header flag, and the failure
/// shapes a decoder has to get right.
pub fn gzip_seeds() -> Vec<Seed> {
    let plain = b"the quick brown fox jumps over the lazy dog. ".repeat(64);
    let ok = oxiarc_deflate::gzip_compress(&plain, 6).expect("gzip");
    let small = oxiarc_deflate::gzip_compress(b"hi", 6).expect("gzip");

    let mut multi = ok.clone();
    multi.extend_from_slice(&small);

    let mut trailing = ok.clone();
    trailing.extend_from_slice(b"XX");

    let mut zero_padded = ok.clone();
    zero_padded.extend_from_slice(&[0u8; 4]);

    let mut bad_crc = ok.clone();
    let at = bad_crc.len() - 8;
    bad_crc[at] ^= 0x01;

    let mut reserved_flags = ok.clone();
    reserved_flags[3] |= 0x20;

    let bomb = gzip_wrap(&single_block_bomb(4 * 1024 * 1024), &[]);

    vec![
        ("ok".into(), ok.clone()),
        ("small".into(), small),
        ("multi_member".into(), multi),
        ("trailing_garbage".into(), trailing),
        ("zero_padded".into(), zero_padded),
        ("bad_crc".into(), bad_crc),
        ("reserved_flags".into(), reserved_flags),
        ("truncated_payload".into(), ok[..ok.len() / 2].to_vec()),
        ("truncated_trailer".into(), ok[..ok.len() - 3].to_vec()),
        ("header_only".into(), ok[..10].to_vec()),
        ("magic_only".into(), vec![0x1F, 0x8B]),
        ("empty".into(), Vec::new()),
        ("single_block_bomb".into(), bomb),
    ]
}

/// `deflate` bodies in all three spellings the sniff has to separate, plus
/// the crafted collision.
pub fn deflate_seeds() -> Vec<Seed> {
    let plain = b"deflate is spelled three different ways in the wild. ".repeat(48);
    let zlib = oxiarc_deflate::zlib_compress(&plain, 6).expect("zlib");
    let raw = oxiarc_deflate::deflate(&plain, 6).expect("deflate");
    let gz = oxiarc_deflate::gzip_compress(&plain, 6).expect("gzip");
    let with_dict =
        oxiarc_deflate::zlib_compress_with_dict(&plain, 6, b"the quick brown fox").expect("dict");

    // A valid *raw* stream whose first two bytes also satisfy the zlib
    // header test: CM = 8, CINFO = 0, (0x08 * 256 + 0x1D) % 31 == 0.
    let mut collision = vec![0x08, 0x1D, 0x00, 0xE2, 0xFF];
    collision.extend_from_slice(&[b'z'; 29]);
    collision.extend_from_slice(&[0x01, 0x00, 0x00, 0xFF, 0xFF]);

    vec![
        ("zlib".into(), zlib.clone()),
        ("raw".into(), raw.clone()),
        ("gzip_mislabelled".into(), gz),
        ("fdict".into(), with_dict),
        ("sniff_collision".into(), collision),
        ("zlib_truncated".into(), zlib[..zlib.len() / 3].to_vec()),
        ("raw_truncated".into(), raw[..raw.len() / 3].to_vec()),
        ("two_bytes".into(), vec![0x78, 0x9C]),
        ("one_byte".into(), vec![0x78]),
        ("empty".into(), Vec::new()),
    ]
}

/// Bodies for the chunk-invariance target. The target reads a chunking plan
/// out of the input's own bytes, so the seeds are ordinary wire bodies with
/// a short plan prefix.
pub fn chunked_seeds() -> Vec<Seed> {
    let plain = b"chunk invariance is the strongest streaming property. ".repeat(96);
    let mut out = Vec::new();
    for (label, body) in [
        (
            "gzip",
            oxiarc_deflate::gzip_compress(&plain, 6).expect("gzip"),
        ),
        (
            "zlib",
            oxiarc_deflate::zlib_compress(&plain, 6).expect("zlib"),
        ),
        ("raw", oxiarc_deflate::deflate(&plain, 6).expect("deflate")),
    ] {
        for (plan_label, plan) in [
            ("one", vec![1u8, 1]),
            ("three", vec![3u8, 3, 3]),
            ("mixed", vec![1u8, 7, 64, 2, 255]),
        ] {
            let mut seed = plan;
            seed.extend_from_slice(&body);
            out.push((format!("{label}_{plan_label}"), seed));
        }
    }
    out
}

/// Negotiation inputs: an `Accept-Encoding` value and a coding set, which
/// the target splits on the first NUL.
pub fn negotiate_seeds() -> Vec<Seed> {
    let headers = [
        "",
        "*",
        "gzip, deflate",
        "deflate;q=0.9, gzip;q=0.8",
        "gzip;q=0, deflate",
        "*;q=0",
        "*;q=0, identity;q=0",
        "*;q=0, identity;q=1",
        "identity;q=0",
        "*;q=0.5, gzip;q=0.1",
    ];
    let available = ["gzip,deflate", "", "br,zstd,gzip", "identity"];
    let mut out = Vec::new();
    for (h, header) in headers.iter().enumerate() {
        for (a, avail) in available.iter().enumerate() {
            let mut seed = header.as_bytes().to_vec();
            seed.push(0);
            seed.extend_from_slice(avail.as_bytes());
            out.push((format!("h{h}_a{a}"), seed));
        }
    }
    out
}

fn named<'a>(values: impl IntoIterator<Item = &'a str>) -> Vec<Seed> {
    values
        .into_iter()
        .enumerate()
        .map(|(index, value)| (format!("{index:03}"), value.as_bytes().to_vec()))
        .collect()
}

// ── the single-block bomb, duplicated from tests/common so this example
//    stays self-contained (an example cannot import a test module) ─────────

struct BitWriter {
    out: Vec<u8>,
    bits: u32,
    count: u32,
}

impl BitWriter {
    fn new() -> Self {
        Self {
            out: Vec::new(),
            bits: 0,
            count: 0,
        }
    }
    fn write_bits(&mut self, value: u32, n: u32) {
        for index in 0..n {
            self.push_bit((value >> index) & 1);
        }
    }
    fn write_code(&mut self, code: u32, n: u32) {
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
    fn finish(mut self) -> Vec<u8> {
        if self.count > 0 {
            self.out.push(self.bits as u8);
        }
        self.out
    }
}

fn fixed_litlen(symbol: u32) -> (u32, u32) {
    match symbol {
        0..=143 => (0x30 + symbol, 8),
        144..=255 => (0x190 + (symbol - 144), 9),
        256..=279 => (symbol - 256, 7),
        _ => (0xC0 + (symbol - 280), 8),
    }
}

/// One fixed-Huffman DEFLATE block that expands ~158.8x — the shape a
/// between-blocks output cap cannot stop.
pub fn single_block_bomb(output_len: usize) -> Vec<u8> {
    let matches = output_len.saturating_sub(1).div_ceil(258);
    let mut w = BitWriter::new();
    w.write_bits(1, 1);
    w.write_bits(1, 2);
    let (lit, lit_bits) = fixed_litlen(0);
    w.write_code(lit, lit_bits);
    let (len258, len_bits) = fixed_litlen(285);
    for _ in 0..matches {
        w.write_code(len258, len_bits);
        w.write_code(0, 5);
    }
    let (eob, eob_bits) = fixed_litlen(256);
    w.write_code(eob, eob_bits);
    w.finish()
}

/// Wrap raw DEFLATE in a gzip container (the trailer is deliberately wrong
/// for a bomb, which never reaches it).
pub fn gzip_wrap(raw: &[u8], plain: &[u8]) -> Vec<u8> {
    let mut out = vec![0x1F, 0x8B, 0x08, 0x00, 0, 0, 0, 0, 0x00, 0xFF];
    out.extend_from_slice(raw);
    out.extend_from_slice(&oxiarc_core::Crc32::compute(plain).to_le_bytes());
    out.extend_from_slice(&(plain.len() as u32).to_le_bytes());
    out
}

fn main() -> std::io::Result<()> {
    let root: PathBuf = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("oxiarc_http_fuzz_seeds"));

    let mut total = 0usize;
    for (target, seeds) in corpora() {
        let dir: &Path = &root.join(target);
        std::fs::create_dir_all(dir)?;
        for (name, bytes) in &seeds {
            std::fs::write(dir.join(name), bytes)?;
        }
        println!("{:>34}: {:>3} seeds", target, seeds.len());
        total += seeds.len();
    }
    println!("\n{total} seeds written under {}", root.display());
    Ok(())
}
