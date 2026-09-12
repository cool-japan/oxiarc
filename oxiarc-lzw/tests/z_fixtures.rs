//! Decoding real `compress(1)` output, from bytes committed to the repo.
//!
//! These fixtures were produced by the system `compress` (see
//! `tests/data/z/README.txt`) and are checked in because the exact bytes
//! *are* the test: they are the only permanently-available evidence that
//! this crate reads what the reference writes. The live differential suite
//! in `tests/z_oracle.rs` covers far more ground but self-skips when the
//! tools are absent, so it cannot be the only gate.
//!
//! Everything here runs unconditionally.

use std::io::Read;

use oxiarc_lzw::z::{ZHeader, ZReader, compress, compress_with_block_mode, decompress};

/// The six-word vocabulary the `widths_*` payload is built from.
const VOCAB_A: [&[u8]; 6] = [
    b"alpha ",
    b"beta ",
    b"gamma ",
    b"delta ",
    b"epsilon ",
    b"zeta ",
];

/// The seven-word vocabulary the second half of the CLEAR payload uses.
const VOCAB_B: [&[u8]; 7] = [
    b"one ", b"two ", b"three ", b"four ", b"five ", b"six ", b"seven ",
];

/// `n` bytes of `vocab` words picked by `(i*i + 3*i) % vocab.len()`.
fn words(n: usize, vocab: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::with_capacity(n + 8);
    let mut i: u32 = 0;
    while out.len() < n {
        let index = (i.wrapping_mul(i).wrapping_add(3u32.wrapping_mul(i))) as usize % vocab.len();
        out.extend_from_slice(vocab[index]);
        i += 1;
    }
    out.truncate(n);
    out
}

/// `n` bytes from the classic `glibc` LCG, taking bits 16..24 of the state.
fn lcg(n: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(n);
    let mut state: u32 = 1;
    for _ in 0..n {
        state = state
            .wrapping_mul(1_103_515_245)
            .wrapping_add(12_345)
            .bitand_keep_31();
        out.push(((state >> 16) & 0xFF) as u8);
    }
    out
}

/// `x & 0x7FFF_FFFF`, spelled as a trait so the LCG above reads like the C.
trait Keep31 {
    fn bitand_keep_31(self) -> u32;
}

impl Keep31 for u32 {
    fn bitand_keep_31(self) -> u32 {
        self & 0x7FFF_FFFF
    }
}

/// The 6,000-byte payload behind `widths_b09.Z` .. `widths_b16.Z`.
fn widths_payload() -> Vec<u8> {
    let mut payload = words(2000, &VOCAB_A);
    payload.extend_from_slice(&lcg(2000));
    payload.extend_from_slice(&words(2000, &VOCAB_A));
    payload
}

/// The 21,000-byte payload behind `clear_b10.Z`.
fn clear_payload() -> Vec<u8> {
    let mut payload = words(10_000, &VOCAB_A);
    payload.extend_from_slice(&words(11_000, &VOCAB_B));
    payload
}

fn fixture(name: &str) -> Vec<u8> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/data/z")
        .join(name);
    std::fs::read(&path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
}

#[test]
fn the_payload_generators_still_produce_the_bytes_the_fixtures_were_made_from() {
    // If this fails, every other test in this file is comparing against the
    // wrong expectation, so it is checked first and explicitly.
    let widths = widths_payload();
    assert_eq!(widths.len(), 6000);
    assert_eq!(&widths[..12], b"alpha epsilo");
    assert_eq!(widths.iter().map(|b| u64::from(*b)).sum::<u64>(), 640_986);

    let clear = clear_payload();
    assert_eq!(clear.len(), 21_000);
    assert_eq!(clear.iter().map(|b| u64::from(*b)).sum::<u64>(), 1_981_070);
    assert_eq!(&clear[10_000..10_012], b"one five fou");
}

#[test]
fn every_reference_width_decodes_byte_identically() {
    let expected = widths_payload();
    for max_bits in 9u8..=16 {
        let name = format!("widths_b{max_bits:02}.Z");
        let stream = fixture(&name);

        let header = ZHeader::parse(&stream).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(header.max_bits, max_bits, "{name}");
        assert!(header.block_mode, "{name}: compress(1) always sets it");

        let decoded = decompress(&stream).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(decoded, expected, "{name}");
    }
}

#[test]
fn the_reference_streams_decode_the_same_through_every_entry_point() {
    let expected = widths_payload();
    for max_bits in 9u8..=16 {
        let name = format!("widths_b{max_bits:02}.Z");
        let stream = fixture(&name);

        let mut into = vec![0u8; expected.len()];
        let written = oxiarc_lzw::z::decompress_into(&stream, &mut into)
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(written, expected.len(), "{name}");
        assert_eq!(into, expected, "{name}");

        let bounded = oxiarc_lzw::z::decompress_with_limit(&stream, expected.len())
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(bounded, expected, "{name}");
        assert!(
            oxiarc_lzw::z::decompress_with_limit(&stream, expected.len() - 1).is_err(),
            "{name}: one byte under the true size must be rejected"
        );

        let mut streamed = Vec::new();
        ZReader::new(&stream[..])
            .read_to_end(&mut streamed)
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(streamed, expected, "{name}");
    }
}

#[test]
fn a_reference_stream_that_really_contains_a_clear_code_decodes() {
    let stream = fixture("clear_b10.Z");
    let header = ZHeader::parse(&stream).expect("header");
    assert_eq!(header.max_bits, 10);
    assert!(header.block_mode);

    let expected = clear_payload();
    assert_eq!(decompress(&stream).expect("decompress"), expected);

    // Prove the fixture is not vacuous: decoding it with block mode turned
    // off (so code 256 is an ordinary entry rather than a reset) must go
    // wrong, which it can only do if a 256 is actually in the stream.
    let mut as_non_block = stream.clone();
    as_non_block[2] &= 0x7F;
    if let Ok(other) = decompress(&as_non_block) {
        assert_ne!(
            other, expected,
            "the fixture cannot contain a ClearCode if ignoring it changes nothing"
        );
    }

    let mut streamed = Vec::new();
    ZReader::new(&stream[..])
        .read_to_end(&mut streamed)
        .expect("stream the clear fixture");
    assert_eq!(streamed, expected);
}

#[test]
fn a_non_block_mode_stream_decodes() {
    let stream = fixture("nonblock_b12.Z");
    let header = ZHeader::parse(&stream).expect("header");
    assert_eq!(header.max_bits, 12);
    assert!(!header.block_mode, "the fixture's whole point");

    let expected = widths_payload();
    assert_eq!(decompress(&stream).expect("decompress"), expected);

    // In non-block mode code 256 is an ordinary table entry; reading the
    // same bytes as block mode must therefore not reproduce the payload.
    let mut as_block = stream.clone();
    as_block[2] |= 0x80;
    if let Ok(other) = decompress(&as_block) {
        assert_ne!(other, expected);
    }
}

#[test]
fn this_crates_encoder_reproduces_the_reference_bytes_exactly() {
    // The strongest statement available without the tools installed: our
    // encoder is not merely "decodable by compress", it is byte-identical
    // to it for every width.
    let payload = widths_payload();
    for max_bits in 9u8..=16 {
        let name = format!("widths_b{max_bits:02}.Z");
        let expected = fixture(&name);
        let produced = compress(&payload, max_bits).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(produced, expected, "{name}");
    }

    let clear = clear_payload();
    assert_eq!(
        compress(&clear, 10).expect("clear payload"),
        fixture("clear_b10.Z")
    );

    assert_eq!(
        compress_with_block_mode(&payload, 12, false).expect("non-block"),
        fixture("nonblock_b12.Z")
    );
}

#[test]
fn truncating_a_reference_stream_yields_a_prefix_and_never_panics() {
    // `.Z` has no end-of-information code, so a truncated stream decodes to
    // a prefix without error. That is the format, and both `gzip -dc` and
    // `uncompress -c` behave the same way; see the module docs.
    let expected = widths_payload();
    let stream = fixture("widths_b12.Z");
    let mut cut_count = 0usize;
    for cut in (ZHeader::LEN..stream.len()).step_by(7) {
        let decoded = decompress(&stream[..cut]).expect("a truncated .Z is not an error");
        assert!(
            decoded.len() <= expected.len(),
            "cut {cut} produced {} bytes",
            decoded.len()
        );
        assert_eq!(
            decoded,
            expected[..decoded.len()],
            "cut {cut} is not a prefix"
        );
        cut_count += 1;
    }
    assert!(cut_count > 400, "only {cut_count} truncations exercised");
}
