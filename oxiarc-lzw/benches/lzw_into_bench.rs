//! A/B benchmark for the TIFF-LZW decode rewrite.
//!
//! `legacy_vec` is a self-contained copy of the decoder oxiarc-lzw shipped
//! before 0.4.2: a `Vec<Vec<u8>>` code table whose every emitted code costs
//! one `to_vec()` heap allocation. It exists only as the benchmark baseline
//! the ">= 3x" target in the roadmap is measured against — nothing in the
//! crate uses it.
//!
//! It reproduces the old decoder's *algorithm and allocation behaviour*, not
//! its error handling: it returns `Option` instead of the crate's `Result`,
//! drops the `LzwConfig` indirection (TIFF parameters are inlined) and skips
//! the bit-width/clear-code validation branches, because none of that is on
//! the hot path being measured. Every case asserts the baseline decodes to
//! the same bytes as the current decoder before it is timed, so the
//! comparison is between two implementations that agree.
//!
//! `oxiarc_vec` is today's `decompress_tiff` (packed prefix/suffix table,
//! growable `Vec` sink) and `oxiarc_into` is `decompress_tiff_into` (same
//! table, caller-supplied slice, zero allocation).
//!
//! `gif_decode` compares `gif_decompress` with the same `Vec<Vec<u8>>`
//! baseline shape it used before 0.4.2 (one `clone()` per emitted code); it
//! now runs on the shared decode loop.
//!
//! For the comparison that actually matters — this decoder against
//! libtiff's `LZWDecode` on strips libtiff produced — see
//! `examples/lzw_vs_libtiff.rs`.

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use oxiarc_lzw::{
    compress_tiff, decompress_tiff, decompress_tiff_into, gif_compress, gif_decompress,
};
use std::collections::HashMap;
use std::hint::black_box;

// ---------------------------------------------------------------------------
// Legacy baseline: per-code `Vec<u8>` dictionary (pre-0.4.2 algorithm)
// ---------------------------------------------------------------------------

/// MSB-first bit reader: the byte-at-a-time accumulator this crate used
/// before 0.4.2, kept here as part of the legacy baseline. The current
/// decoder extracts each code from a four-byte window instead and keeps no
/// reader state at all (`bits::CodeOrder`).
struct LegacyBitReader<'a> {
    data: &'a [u8],
    byte_pos: usize,
    buffer: u32,
    bits_in_buffer: u8,
}

impl<'a> LegacyBitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            byte_pos: 0,
            buffer: 0,
            bits_in_buffer: 0,
        }
    }

    fn read_bits(&mut self, count: u8) -> Option<u16> {
        while self.bits_in_buffer < count && self.byte_pos < self.data.len() {
            self.buffer = (self.buffer << 8) | u32::from(self.data[self.byte_pos]);
            self.byte_pos += 1;
            self.bits_in_buffer += 8;
        }
        if self.bits_in_buffer < count {
            return None;
        }
        let shift = self.bits_in_buffer - count;
        let mask = (1u32 << count) - 1;
        let value = (self.buffer >> shift) & mask;
        self.bits_in_buffer -= count;
        Some(value as u16)
    }
}

/// The pre-0.4.2 dictionary: one owned `Vec<u8>` per code.
struct LegacyDictionary {
    table: Vec<Vec<u8>>,
    #[allow(dead_code)]
    reverse: HashMap<Vec<u8>, u16>,
    next_code: u16,
    current_bits: u8,
}

impl LegacyDictionary {
    fn new() -> Self {
        let mut dict = Self {
            table: Vec::with_capacity(4096),
            reverse: HashMap::new(),
            next_code: 0,
            current_bits: 9,
        };
        dict.reset();
        dict
    }

    fn reset(&mut self) {
        self.table.clear();
        self.current_bits = 9;
        for i in 0..256u16 {
            self.table.push(vec![i as u8]);
        }
        self.table.push(Vec::new()); // clear code
        self.table.push(Vec::new()); // EOI
        self.next_code = 258;
    }

    fn add_string_decode(&mut self, string: Vec<u8>) {
        if self.next_code > 4095 {
            return;
        }
        self.table.push(string);
        self.next_code += 1;
        if self.current_bits < 12 && self.next_code >= (1 << self.current_bits) - 1 {
            self.current_bits += 1;
        }
    }

    fn get_string(&self, code: u16) -> Option<&[u8]> {
        self.table.get(code as usize).map(|v| v.as_slice())
    }

    fn is_full(&self) -> bool {
        self.next_code > 4095
    }
}

/// The pre-0.4.2 `LzwDecoder::decode`, verbatim in behaviour.
fn legacy_decompress_tiff(input: &[u8], expected_size: usize) -> Option<Vec<u8>> {
    let mut dict = LegacyDictionary::new();
    let mut reader = LegacyBitReader::new(input);
    let mut output = Vec::with_capacity(expected_size.min(64 * 1024));
    let mut prev_code: Option<u16> = None;

    while output.len() < expected_size {
        let code = reader.read_bits(dict.current_bits)?;
        if code == 256 {
            dict.reset();
            prev_code = None;
            continue;
        }
        if code == 257 {
            break;
        }

        let string = if code < dict.next_code {
            dict.get_string(code)?.to_vec()
        } else if code == dict.next_code {
            let prev = prev_code?;
            let prev_string = dict.get_string(prev)?;
            let mut new_string = prev_string.to_vec();
            new_string.push(prev_string[0]);
            new_string
        } else {
            return None;
        };

        output.extend_from_slice(&string);

        if let Some(prev) = prev_code {
            if !dict.is_full() {
                let prev_string = dict.get_string(prev)?;
                let mut new_entry = prev_string.to_vec();
                new_entry.push(string[0]);
                dict.add_string_decode(new_entry);
            }
        }
        prev_code = Some(code);
    }

    output.truncate(expected_size);
    Some(output)
}

// ---------------------------------------------------------------------------
// Payloads
// ---------------------------------------------------------------------------

fn uniform(size: usize) -> Vec<u8> {
    vec![0xAA; size]
}

fn random(size: usize) -> Vec<u8> {
    let mut data = Vec::with_capacity(size);
    let mut seed: u64 = 0x1234_5678_9ABC_DEF0;
    for _ in 0..size {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        data.push((seed >> 32) as u8);
    }
    data
}

fn text_like(size: usize) -> Vec<u8> {
    let text = b"The quick brown fox jumps over the lazy dog. \
                 Pack my box with five dozen liquor jugs. \
                 How vexingly quick daft zebras jump! ";
    let mut data = Vec::with_capacity(size);
    while data.len() < size {
        let remaining = size - data.len();
        data.extend_from_slice(&text[..remaining.min(text.len())]);
    }
    data
}

fn image_like(size: usize) -> Vec<u8> {
    let side = (size as f64).sqrt() as usize;
    let mut data = Vec::with_capacity(size);
    for y in 0..side {
        for x in 0..side {
            data.push((((x * 255 / side) + (y * 255 / side)) / 2).min(255) as u8);
        }
    }
    while data.len() < size {
        data.push(128);
    }
    data
}

type PatternGenerator = fn(usize) -> Vec<u8>;

/// Strip-shaped decode: legacy per-code-`Vec` vs the prefix/suffix table.
fn bench_strip_decode(c: &mut Criterion) {
    let mut group = c.benchmark_group("tiff_strip_decode");

    let patterns: [(&str, PatternGenerator); 4] = [
        ("image", image_like as PatternGenerator),
        ("text", text_like as PatternGenerator),
        ("uniform", uniform as PatternGenerator),
        ("random", random as PatternGenerator),
    ];

    for (size_name, size) in [("256KB", 512 * 512), ("1MB", 1024 * 1024)] {
        for (pattern_name, generator) in patterns {
            let raw = generator(size);
            let strip = compress_tiff(&raw).expect("compress benchmark payload");
            let id = format!("{size_name}/{pattern_name}");

            // Sanity: all three paths must agree before we time them.
            let legacy = legacy_decompress_tiff(&strip, raw.len()).expect("legacy decode");
            assert_eq!(legacy, raw, "legacy baseline disagrees on {id}");
            let mut into = vec![0u8; raw.len()];
            let written = decompress_tiff_into(&strip, &mut into).expect("into decode");
            assert_eq!(written, raw.len());
            assert_eq!(into, raw, "decompress_tiff_into disagrees on {id}");

            group.throughput(Throughput::Bytes(size as u64));
            group.bench_with_input(
                BenchmarkId::new("legacy_vec", &id),
                &(strip.clone(), raw.len()),
                |b, (strip, len)| {
                    b.iter(|| black_box(legacy_decompress_tiff(black_box(strip), *len)));
                },
            );
            group.bench_with_input(
                BenchmarkId::new("oxiarc_vec", &id),
                &(strip.clone(), raw.len()),
                |b, (strip, len)| {
                    b.iter(|| black_box(decompress_tiff(black_box(strip), *len)));
                },
            );
            let mut dst = vec![0u8; raw.len()];
            group.bench_with_input(BenchmarkId::new("oxiarc_into", &id), &strip, |b, strip| {
                b.iter(|| black_box(decompress_tiff_into(black_box(strip), &mut dst)));
            });
        }
    }

    group.finish();
}

/// Encode side: the old encoder cloned the current match for every input
/// byte; the new one tracks it as a code.
fn bench_strip_encode(c: &mut Criterion) {
    let mut group = c.benchmark_group("tiff_strip_encode");
    let patterns: [(&str, PatternGenerator); 3] = [
        ("image", image_like as PatternGenerator),
        ("text", text_like as PatternGenerator),
        ("random", random as PatternGenerator),
    ];
    for (pattern_name, generator) in patterns {
        let size = 512 * 512;
        let raw = generator(size);
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(
            BenchmarkId::new("compress_tiff", pattern_name),
            &raw,
            |b, raw| {
                b.iter(|| black_box(compress_tiff(black_box(raw))));
            },
        );
    }
    group.finish();
}

/// GIF image data: the codec that used to allocate a `Vec<u8>` per emitted
/// code and now shares the strip decoder's loop.
fn bench_gif_decode(c: &mut Criterion) {
    let mut group = c.benchmark_group("gif_decode");
    let patterns: [(&str, PatternGenerator); 4] = [
        ("image", image_like as PatternGenerator),
        ("text", text_like as PatternGenerator),
        ("uniform", uniform as PatternGenerator),
        ("random", random as PatternGenerator),
    ];
    for (pattern_name, generator) in patterns {
        let size = 1024 * 1024;
        let raw = generator(size);
        let stream = gif_compress(&raw, 8).expect("gif compress benchmark payload");
        assert_eq!(
            gif_decompress(&stream, 8).expect("gif decompress"),
            raw,
            "gif round trip disagrees on {pattern_name}"
        );
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(
            BenchmarkId::new("gif_decompress", pattern_name),
            &stream,
            |b, stream| {
                b.iter(|| black_box(gif_decompress(black_box(stream), 8)));
            },
        );
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_strip_decode,
    bench_strip_encode,
    bench_gif_decode
);
criterion_main!(benches);
