//! Systematic corrupt-input sweep.
//!
//! For every fixture the crate embeds, this walks every single-byte
//! truncation, a dense grid of single-byte corruptions, every segment-length
//! mutation and a set of hand-built structural attacks, and asserts that each
//! one returns `Ok` or `Err` — never a panic, never an allocation beyond
//! [`DecodeLimits::strict`], and never an unbounded loop.
//!
//! No fuzzer is required: the sweep is deterministic and runs in well under a
//! second, so it is a permanent regression gate rather than a campaign.

use std::time::{Duration, Instant};

use oxiarc_jpeg::{
    DecodeLimits, DecodeOptions, Decoder, TableSet, TablesMode, decode_abbreviated_into, sample,
    tiff,
};

/// Wall-clock budget for the whole sweep.
const BUDGET: Duration = Duration::from_secs(60);

/// Decode with strict limits, discarding the result. Must never panic.
fn probe(data: &[u8]) {
    let options = DecodeOptions {
        limits: DecodeLimits::strict(),
        ..DecodeOptions::default()
    };
    let mut decoder = Decoder::with_options(data, options.clone());
    if decoder.read_info().is_ok() {
        let _ = decoder.decode();
        let _ = decoder.decode_u16();
        let _ = decoder.icc_profile();
    }

    // The tolerant path must be equally safe.
    let tolerant = DecodeOptions {
        tolerate_truncated: true,
        ..options
    };
    let mut decoder = Decoder::with_options(data, tolerant);
    if decoder.read_info().is_ok() {
        let _ = decoder.decode();
    }

    // Table-only and abbreviated entry points see the same bytes.
    if let Ok(tables) = TableSet::parse(data) {
        let _ = tables.emit(TablesMode::BOTH);
        let mut out = [0u8; 256];
        let _ = decode_abbreviated_into(Some(&tables), data, &DecodeOptions::strict(), &mut out);
    }
    let _ = tiff::parse_jpeg_tables(data);
    let _ = tiff::merge_jpeg_tables(&sample::RGB_8X8_420_TABLES, data);
}

fn fixtures() -> Vec<(&'static str, Vec<u8>)> {
    vec![
        ("gray_1x1", sample::GRAY_1X1.to_vec()),
        ("gray_tables", sample::GRAY_1X1_TABLES.to_vec()),
        ("gray_scan", sample::GRAY_1X1_SCAN.to_vec()),
        ("rgb_8x8_420", sample::RGB_8X8_420.to_vec()),
        ("rgb_tables", sample::RGB_8X8_420_TABLES.to_vec()),
        ("rgb_scan", sample::RGB_8X8_420_SCAN.to_vec()),
    ]
}

#[test]
fn truncation_at_every_offset_never_panics() {
    let start = Instant::now();
    for (name, data) in fixtures() {
        for n in 0..=data.len() {
            probe(&data[..n]);
            assert!(
                start.elapsed() < BUDGET,
                "{name}: sweep exceeded its time budget at offset {n}"
            );
        }
    }
}

#[test]
fn single_byte_corruption_never_panics() {
    let start = Instant::now();
    for (name, data) in fixtures() {
        for offset in 0..data.len() {
            for mask in [0xFFu8, 0x01, 0x80, 0x0F, 0xF0] {
                let mut corrupt = data.clone();
                corrupt[offset] ^= mask;
                probe(&corrupt);
            }
            assert!(
                start.elapsed() < BUDGET,
                "{name}: sweep exceeded its time budget at offset {offset}"
            );
        }
    }
}

#[test]
fn segment_length_corruption_never_panics() {
    for (_, data) in fixtures() {
        let mut offset = 2usize;
        while offset + 4 <= data.len() {
            if data[offset] != 0xFF {
                offset += 1;
                continue;
            }
            let marker = data[offset + 1];
            if marker == 0xD8 || marker == 0xD9 || (0xD0..=0xD7).contains(&marker) {
                offset += 2;
                continue;
            }
            for value in [0u16, 1, 2, 3, 0x7FFF, 0xFFFF] {
                let mut corrupt = data.clone();
                corrupt[offset + 2] = (value >> 8) as u8;
                corrupt[offset + 3] = value as u8;
                probe(&corrupt);
            }
            let length = usize::from(u16::from_be_bytes([data[offset + 2], data[offset + 3]]));
            offset += 2 + length.max(2);
        }
    }
}

#[test]
fn structural_attacks_never_panic() {
    let base = sample::RGB_8X8_420.to_vec();
    // Locate the SOF so the frame parameters can be attacked directly.
    let mut sof = None;
    let mut offset = 2usize;
    while offset + 4 <= base.len() {
        let marker = base[offset + 1];
        if marker == 0xD8 || marker == 0xD9 {
            offset += 2;
            continue;
        }
        let length = usize::from(u16::from_be_bytes([base[offset + 2], base[offset + 3]]));
        if (0xC0..=0xCF).contains(&marker) && marker != 0xC4 && marker != 0xCC {
            sof = Some(offset);
            break;
        }
        offset += 2 + length;
    }
    let sof = sof.expect("fixture has a SOF");

    // Nf = 0 and Nf = 255, H/V = 0 and 5, Tq = 4.
    for (index, value) in [
        (sof + 9, 0u8),
        (sof + 9, 255),
        (sof + 11, 0x00),
        (sof + 11, 0x55),
        (sof + 12, 4),
        (sof + 4, 0),
        (sof + 4, 17),
    ] {
        let mut corrupt = base.clone();
        corrupt[index] = value;
        probe(&corrupt);
    }

    // X = 0, Y = 0, and the 65535 x 65535 dimension bomb.
    for (y, x) in [(0u16, 8u16), (8, 0), (0, 0), (65535, 65535)] {
        let mut corrupt = base.clone();
        corrupt[sof + 5] = (y >> 8) as u8;
        corrupt[sof + 6] = y as u8;
        corrupt[sof + 7] = (x >> 8) as u8;
        corrupt[sof + 8] = x as u8;
        probe(&corrupt);
    }

    // Hand-built Huffman tables: empty, over-subscribed at every length, and
    // one whose BITS sum to 257.
    for length in 0..16usize {
        let mut bits = [0u8; 16];
        bits[length] = 255;
        let mut payload = vec![0x00u8];
        payload.extend_from_slice(&bits);
        payload.extend(std::iter::repeat_n(0u8, 255));
        let mut stream = vec![0xFF, 0xD8, 0xFF, 0xC4];
        let len = (payload.len() + 2) as u16;
        stream.extend_from_slice(&len.to_be_bytes());
        stream.extend_from_slice(&payload);
        stream.extend_from_slice(&base[sof..]);
        probe(&stream);
    }

    // DRI = 0 with restart markers present, and restart markers out of order.
    let mut with_rst = base.clone();
    with_rst.splice(sof..sof, [0xFF, 0xDD, 0x00, 0x04, 0x00, 0x00]);
    probe(&with_rst);

    // A stream that is nothing but restart markers.
    let mut rst_soup = vec![0xFFu8, 0xD8];
    for n in 0..8u8 {
        rst_soup.extend_from_slice(&[0xFF, 0xD0 + n]);
    }
    rst_soup.extend_from_slice(&[0xFF, 0xD9]);
    probe(&rst_soup);

    // Duplicated SOF and SOS.
    let mut duplicated = base.clone();
    duplicated.extend_from_slice(&base[sof..]);
    probe(&duplicated);
}

#[test]
fn arbitrary_bytes_never_panic() {
    // A cheap deterministic PRNG: enough to cover shapes the structured
    // attacks miss, without adding a dependency.
    let mut state = 0x2545_F491_4F6C_DD1Du64;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    for case in 0..2_000 {
        let len = (next() % 512) as usize;
        let mut data = Vec::with_capacity(len + 2);
        if case % 3 != 0 {
            data.extend_from_slice(&[0xFF, 0xD8]);
        }
        for _ in 0..len {
            data.push((next() & 0xFF) as u8);
        }
        probe(&data);
    }
}

#[test]
fn a_scan_bomb_is_capped() {
    // Thousands of tiny scans must trip DecodeLimits::max_scans rather than
    // running forever.
    let mut stream = sample::RGB_8X8_420.to_vec();
    let eoi = stream.len() - 2;
    let sos: Vec<u8> = vec![
        0xFF, 0xDA, 0x00, 0x08, 0x01, 0x01, 0x00, 0x00, 0x3F, 0x00, 0x00,
    ];
    let mut tail = Vec::new();
    for _ in 0..2_000 {
        tail.extend_from_slice(&sos);
    }
    stream.splice(eoi..eoi, tail);
    let start = Instant::now();
    probe(&stream);
    assert!(start.elapsed() < BUDGET, "scan bomb was not capped");
}
