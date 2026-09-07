//! Differential oracle for the UNIX `compress(1)` / `.Z` container against
//! the system tools.
//!
//! Round-tripping `.Z` through this crate alone proves nothing about
//! interoperability — a private dialect round-trips perfectly. These tests
//! check both directions against independent references:
//!
//! 1. **decode direction**: real `compress -b N -c` output must decode
//!    byte-identically here, for every width the tool will emit;
//! 2. **encode direction**: this crate's output must be *byte-identical* to
//!    `compress -b N -c` (not merely decodable), and must be accepted by
//!    `gzip -dc` and `uncompress -c`.
//!
//! Gated behind the `z-oracle` feature; every test self-skips (prints a
//! note, does not fail) when the tool it needs is absent.
//!
//! # Platform quirk this suite works around
//!
//! `gzip -dc` and `uncompress -c` on macOS/BSD refuse `.Z` streams whose
//! `max_bits` is below 12 — they produce an empty output and (for `gzip`)
//! still exit 0 — even though `compress -b 9` happily writes such files.
//! The acceptance test therefore treats "empty output for a non-empty
//! payload at `max_bits < 12`" as a skip, and asserts byte-identity for
//! every width the tool actually decodes. Widths 9..=11 remain covered in
//! the decode direction by the committed fixtures in `tests/z_fixtures.rs`.
#![cfg(feature = "z-oracle")]

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use oxiarc_lzw::z::{ZHeader, compress, compress_with_block_mode, decompress};

/// Locate `tool` on `PATH`, returning `None` when it is not installed.
fn on_path(tool: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(tool))
        .find(|candidate| candidate.is_file())
}

/// A private scratch directory under the system temp dir.
fn scratch_dir(tag: &str) -> io::Result<PathBuf> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!(
        "oxiarc-lzw-z-oracle-{tag}-{}-{nanos}",
        std::process::id()
    ));
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// `n` bytes of `vocab` words picked by `(i*i + 3*i) % vocab.len()`.
fn words(n: usize, vocab: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::with_capacity(n + 8);
    let mut i: u32 = 0;
    while out.len() < n {
        let index = (i.wrapping_mul(i).wrapping_add(3u32.wrapping_mul(i))) as usize % vocab.len();
        out.extend_from_slice(vocab[index]);
        i = i.wrapping_add(1);
    }
    out.truncate(n);
    out
}

/// A deterministic xorshift byte stream (incompressible filler).
fn noise(n: usize, seed: u32) -> Vec<u8> {
    let mut state = seed | 1;
    (0..n)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            (state >> 7) as u8
        })
        .collect()
}

/// The payload shapes every direction is measured over.
fn payloads() -> Vec<(&'static str, Vec<u8>)> {
    const VOCAB_A: [&[u8]; 6] = [
        b"alpha ",
        b"beta ",
        b"gamma ",
        b"delta ",
        b"epsilon ",
        b"zeta ",
    ];
    const VOCAB_B: [&[u8]; 7] = [
        b"one ", b"two ", b"three ", b"four ", b"five ", b"six ", b"seven ",
    ];

    let mut mixed = words(2_000, &VOCAB_A);
    mixed.extend_from_slice(&noise(2_000, 0x1234_5678));
    mixed.extend_from_slice(&words(2_000, &VOCAB_A));

    let mut regime_change = words(10_000, &VOCAB_A);
    regime_change.extend_from_slice(&words(11_000, &VOCAB_B));

    vec![
        ("empty", Vec::new()),
        ("one_byte", vec![b'Z']),
        ("all_zero_64k", vec![0u8; 64 * 1024]),
        ("every_byte", (0..=255u8).collect()),
        ("text", b"TOBEORNOTTOBEORTOBEORNOT".repeat(400)),
        ("words", words(30_000, &VOCAB_A)),
        ("mixed", mixed),
        ("regime_change", regime_change),
        ("noise", noise(20_000, 0x9E37_79B9)),
    ]
}

/// Run `bin args… < input` and return its stdout, retrying a couple of
/// times so a transient spawn failure under load cannot masquerade as "the
/// tool disagrees".
fn run_tool(bin: &Path, args: &[&str], input: &Path) -> Option<Vec<u8>> {
    for attempt in 0..3 {
        if attempt > 0 {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let Ok(stdin) = fs::File::open(input) else {
            continue;
        };
        let Ok(output) = Command::new(bin)
            .args(args)
            .stdin(Stdio::from(stdin))
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
        else {
            continue;
        };
        if output.status.success() {
            return Some(output.stdout);
        }
    }
    None
}

/// Run `compress -b max_bits -c < input`, returning its stdout.
fn reference_compress(bin: &Path, max_bits: u8, input: &Path) -> Option<Vec<u8>> {
    let width = max_bits.to_string();
    run_tool(bin, &["-b", &width, "-c"], input)
}

/// Run a reference decoder (`gzip -dc` or `uncompress -c`) over `input`.
fn reference_decompress(bin: &Path, args: &[&str], input: &Path) -> Option<Vec<u8>> {
    run_tool(bin, args, input)
}

#[test]
fn oracle_real_compress_output_decodes_byte_identically() {
    let Some(bin) = on_path("compress") else {
        println!("SKIP: `compress` is not on PATH");
        return;
    };
    let dir = scratch_dir("decode").expect("scratch dir");
    let raw = dir.join("payload.raw");

    let mut checked = 0usize;
    for (name, payload) in payloads() {
        fs::write(&raw, &payload).expect("write payload");
        for max_bits in 9u8..=16 {
            let Some(stream) = reference_compress(&bin, max_bits, &raw) else {
                println!("SKIP: `compress -b {max_bits}` failed for {name}");
                continue;
            };
            if stream.is_empty() {
                // Apple's `compress` writes nothing for empty input.
                assert!(payload.is_empty(), "{name}/b{max_bits}: empty output");
                continue;
            }
            let header = ZHeader::parse(&stream)
                .unwrap_or_else(|e| panic!("{name}/b{max_bits}: header: {e}"));
            assert_eq!(header.max_bits, max_bits, "{name}/b{max_bits}");
            assert!(header.block_mode, "compress(1) always sets block mode");

            let decoded = decompress(&stream).unwrap_or_else(|e| panic!("{name}/b{max_bits}: {e}"));
            assert_eq!(decoded, payload, "{name}/b{max_bits} decoded differently");
            checked += 1;
        }
    }
    let _ = fs::remove_dir_all(&dir);
    assert!(checked >= 64, "only {checked} reference streams decoded");
    println!("oracle: {checked} real `compress` streams decoded byte-identically");
}

#[test]
fn oracle_our_encoder_is_byte_identical_to_compress() {
    let Some(bin) = on_path("compress") else {
        println!("SKIP: `compress` is not on PATH");
        return;
    };
    let dir = scratch_dir("encode").expect("scratch dir");
    let raw = dir.join("payload.raw");

    let mut checked = 0usize;
    for (name, payload) in payloads() {
        if payload.is_empty() {
            // The reference writes a zero-byte file here; this crate writes
            // the three-byte header (what `ncompress` does). Documented
            // departure, pinned by `oxiarc_lzw::z` module docs.
            continue;
        }
        fs::write(&raw, &payload).expect("write payload");
        for max_bits in 9u8..=16 {
            let Some(expected) = reference_compress(&bin, max_bits, &raw) else {
                println!("SKIP: `compress -b {max_bits}` failed for {name}");
                continue;
            };
            let produced =
                compress(&payload, max_bits).unwrap_or_else(|e| panic!("{name}/b{max_bits}: {e}"));
            assert_eq!(
                produced.len(),
                expected.len(),
                "{name}/b{max_bits}: length differs from `compress -b {max_bits} -c`"
            );
            assert_eq!(
                produced, expected,
                "{name}/b{max_bits}: bytes differ from `compress -b {max_bits} -c`"
            );
            checked += 1;
        }
    }
    let _ = fs::remove_dir_all(&dir);
    assert!(
        checked >= 64,
        "only {checked} (payload, width) pairs compared"
    );
    println!("oracle: {checked} streams byte-identical to `compress -b N -c`");
}

#[test]
fn oracle_reference_decoders_accept_our_streams() {
    let gzip = on_path("gzip");
    let uncompress = on_path("uncompress");
    if gzip.is_none() && uncompress.is_none() {
        println!("SKIP: neither `gzip` nor `uncompress` is on PATH");
        return;
    }
    let dir = scratch_dir("accept").expect("scratch dir");
    let stream_path = dir.join("stream.Z");

    let mut accepted = 0usize;
    let mut skipped_narrow = 0usize;
    for (name, payload) in payloads() {
        if payload.is_empty() {
            continue;
        }
        for max_bits in 9u8..=16 {
            for block_mode in [true, false] {
                let stream = compress_with_block_mode(&payload, max_bits, block_mode)
                    .unwrap_or_else(|e| panic!("{name}/b{max_bits}: {e}"));
                fs::write(&stream_path, &stream).expect("write stream");

                for (tool, args) in [
                    (gzip.as_deref(), &["-dc"][..]),
                    (uncompress.as_deref(), &["-c"][..]),
                ] {
                    let Some(tool) = tool else { continue };
                    let Some(decoded) = reference_decompress(tool, args, &stream_path) else {
                        // A hard failure is only tolerated for the narrow
                        // widths this platform refuses outright.
                        assert!(
                            max_bits < 12,
                            "{name}/b{max_bits}/block={block_mode}: {} rejected the stream",
                            tool.display()
                        );
                        skipped_narrow += 1;
                        continue;
                    };
                    if decoded.is_empty() && max_bits < 12 {
                        skipped_narrow += 1;
                        continue;
                    }
                    assert_eq!(
                        decoded,
                        payload,
                        "{name}/b{max_bits}/block={block_mode}: {} decoded differently",
                        tool.display()
                    );
                    accepted += 1;
                }
            }
        }
    }
    let _ = fs::remove_dir_all(&dir);
    assert!(
        accepted >= 80,
        "only {accepted} streams accepted ({skipped_narrow} narrow-width skips)"
    );
    println!(
        "oracle: {accepted} oxiarc `.Z` streams reproduced by the reference decoders \
         ({skipped_narrow} narrow-width skips)"
    );
}

#[test]
fn oracle_block_mode_reset_really_fires_and_matches_the_reference() {
    let Some(bin) = on_path("compress") else {
        println!("SKIP: `compress` is not on PATH");
        return;
    };
    const VOCAB_A: [&[u8]; 6] = [
        b"alpha ",
        b"beta ",
        b"gamma ",
        b"delta ",
        b"epsilon ",
        b"zeta ",
    ];
    const VOCAB_B: [&[u8]; 7] = [
        b"one ", b"two ", b"three ", b"four ", b"five ", b"six ", b"seven ",
    ];
    let mut payload = words(10_000, &VOCAB_A);
    payload.extend_from_slice(&words(11_000, &VOCAB_B));

    let dir = scratch_dir("clear").expect("scratch dir");
    let raw = dir.join("payload.raw");
    fs::write(&raw, &payload).expect("write payload");

    let Some(expected) = reference_compress(&bin, 10, &raw) else {
        println!("SKIP: `compress -b 10` failed");
        let _ = fs::remove_dir_all(&dir);
        return;
    };
    let produced = compress(&payload, 10).expect("compress");
    assert_eq!(produced, expected, "narrow-width CLEAR stream differs");

    // Prove the stream really carries a ClearCode: reading it with the
    // block-mode flag cleared (so 256 is an ordinary entry) cannot
    // reproduce the payload.
    let mut as_non_block = produced.clone();
    as_non_block[2] &= 0x7F;
    if let Ok(other) = decompress(&as_non_block) {
        assert_ne!(other, payload, "no ClearCode in the stream");
    }
    let _ = fs::remove_dir_all(&dir);
    println!("oracle: block-mode reset stream is byte-identical to `compress -b 10 -c`");
}

/// LSB-first packer for hand-built code streams, the way `.Z` packs them.
fn pack(codes: &[(u16, u8)]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut acc: u32 = 0;
    let mut held: u8 = 0;
    for (code, width) in codes {
        acc |= u32::from(*code) << held;
        held += width;
        while held >= 8 {
            out.push((acc & 0xFF) as u8);
            acc >>= 8;
            held -= 8;
        }
    }
    if held > 0 {
        out.push((acc & 0xFF) as u8);
    }
    out
}

#[test]
fn oracle_hand_built_corner_streams_match_the_references() {
    // The two hand-built streams in `tests/z_roundtrip.rs` justify their
    // expectations by quoting what `gzip -dc` and `uncompress -c` do. That
    // suite is hermetic, so this test is where the quotation is checked
    // against the actual tools.
    let gzip = on_path("gzip");
    let uncompress = on_path("uncompress");
    if gzip.is_none() && uncompress.is_none() {
        println!("SKIP: neither `gzip` nor `uncompress` is on PATH");
        return;
    }
    let dir = scratch_dir("corner").expect("scratch dir");
    let path = dir.join("stream.Z");
    let header = ZHeader::new(12, true).expect("header").to_bytes();

    // 1. KwKwK: `1F 9D 8C 61 02 0A 04` is `aaaaaa` everywhere.
    let mut kwkwk = header.to_vec();
    kwkwk.extend_from_slice(&pack(&[(97, 9), (257, 9), (258, 9)]));
    assert_eq!(kwkwk, [0x1F, 0x9D, 0x8C, 0x61, 0x02, 0x0A, 0x04]);
    fs::write(&path, &kwkwk).expect("write stream");
    let mut agreed = 0usize;
    for (tool, args) in [
        (gzip.as_deref(), &["-dc"][..]),
        (uncompress.as_deref(), &["-c"][..]),
    ] {
        let Some(tool) = tool else { continue };
        let Some(decoded) = reference_decompress(tool, args, &path) else {
            panic!("{} rejected the KwKwK stream", tool.display());
        };
        assert_eq!(decoded, b"aaaaaa", "{}: KwKwK", tool.display());
        agreed += 1;
    }
    assert_eq!(decompress(&kwkwk).expect("kwkwk"), b"aaaaaa");

    // 2. A stream whose *first* code is a CLEAR. `compress(1)` never writes
    //    one, and the two references disagree: `gzip -dc` treats it as a
    //    reset and decodes the body normally, BSD `uncompress -c` reads the
    //    group padding as literals and emits garbage. This crate follows
    //    `gzip` (see `oxiarc_lzw::z`'s module docs); if a future platform's
    //    `uncompress` starts agreeing, this test says so.
    let mut clear_group = vec![(256u16, 9u8)];
    clear_group.extend(std::iter::repeat_n((0u16, 9u8), 7));
    let body = pack(&[(65, 9), (66, 9), (257, 9), (257, 9)]);
    let mut with_clear = header.to_vec();
    with_clear.extend_from_slice(&pack(&clear_group));
    with_clear.extend_from_slice(&body);
    assert_eq!(decompress(&with_clear).expect("leading clear"), b"ABABAB");

    fs::write(&path, &with_clear).expect("write stream");
    if let Some(bin) = gzip.as_deref() {
        let decoded =
            reference_decompress(bin, &["-dc"], &path).expect("gzip must accept a leading CLEAR");
        assert_eq!(
            decoded, b"ABABAB",
            "gzip -dc no longer resets on a leading CLEAR; this crate follows gzip, so the              module docs and `z_roundtrip.rs` need revisiting"
        );
        agreed += 1;
    }
    if let Some(bin) = uncompress.as_deref() {
        match reference_decompress(bin, &["-c"], &path) {
            Some(decoded) => assert_ne!(
                decoded, b"ABABAB",
                "BSD uncompress now agrees with gzip on a leading CLEAR; the documented                  divergence in `oxiarc_lzw::z` is stale"
            ),
            None => println!("note: `uncompress` rejected the leading-CLEAR stream outright"),
        }
    }

    let _ = fs::remove_dir_all(&dir);
    assert!(agreed >= 2, "only {agreed} reference comparisons ran");
    println!("oracle: {agreed} hand-built corner streams cross-checked against the references");
}
