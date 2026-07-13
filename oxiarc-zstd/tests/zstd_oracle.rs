//! Live differential ("oracle") tests against the reference `zstd` CLI.
//!
//! Both interop directions are exercised:
//!
//! 1. **Decode direction** — the reference CLI compresses diverse inputs
//!    across a wide level/flag matrix (`-1 .. -19`, `--ultra -22`,
//!    `--long`, `--no-check`, `--no-content-size`, raw-content
//!    dictionaries, multi-frame concatenation) and oxiarc must decode every
//!    frame byte-identically.
//! 2. **Encode direction** — oxiarc compresses the same inputs at several
//!    levels (plus no-checksum / no-content-size / dictionary variants) and
//!    the reference CLI must accept the frames and reproduce the input.
//!
//! Gated behind the `zstd-oracle` feature. Every test self-skips (prints a
//! note, does not fail) when the `zstd` binary is not found on PATH.
#![cfg(feature = "zstd-oracle")]

use std::io::Write as _;
use std::path::PathBuf;
use std::process::{Command, Stdio};

/// Locate the `zstd` binary via `which`; `None` means the tests self-skip.
fn find_zstd() -> Option<PathBuf> {
    let output = Command::new("which").arg("zstd").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if path.is_empty() {
        None
    } else {
        Some(PathBuf::from(path))
    }
}

fn skip_note(test: &str) {
    eprintln!(
        "[zstd-oracle] `zstd` not found on PATH; skipping '{test}' (self-skip, not a failure)"
    );
}

/// Run `zstd <args>` feeding `input` on stdin, capturing stdout.
///
/// Stdin is fed from a separate thread: writing a large input while the
/// child's stdout pipe fills up would otherwise deadlock both processes.
fn run_zstd(args: &[&str], input: &[u8]) -> Result<Vec<u8>, String> {
    let mut child = Command::new("zstd")
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn zstd: {e}"))?;
    let mut stdin = child.stdin.take().ok_or("no stdin")?;
    let input_owned = input.to_vec();
    let feeder = std::thread::spawn(move || {
        let _ = stdin.write_all(&input_owned);
        // stdin drops here, closing the pipe.
    });
    let out = child.wait_with_output().map_err(|e| format!("wait: {e}"))?;
    let _ = feeder.join();
    if !out.status.success() {
        return Err(format!(
            "zstd {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(out.stdout)
}

/// Reference-compress `input` with the given extra flags.
fn zstd_compress(input: &[u8], flags: &[&str]) -> Result<Vec<u8>, String> {
    let mut args = vec!["-q", "-c"];
    args.extend_from_slice(flags);
    run_zstd(&args, input)
}

/// Reference-decompress `frame`.
fn zstd_decompress(frame: &[u8]) -> Result<Vec<u8>, String> {
    run_zstd(&["-d", "-q", "-c"], frame)
}

/// Diverse inputs: empty, tiny, runs, repetitive text, random-ish,
/// structured binary, and sizes crossing the 128 KiB block boundary.
fn test_inputs() -> Vec<(String, Vec<u8>)> {
    let mut inputs: Vec<(String, Vec<u8>)> = vec![
        ("empty".into(), Vec::new()),
        ("one_byte".into(), vec![0x42]),
        ("zeros_300".into(), vec![0u8; 300]),
        ("zeros_200k".into(), vec![0u8; 200_000]),
        ("abc_104".into(), b"ABCDEFGH".repeat(13)),
    ];

    let mut text = Vec::new();
    while text.len() < 150_000 {
        text.extend_from_slice(b"The quick brown fox jumps over the lazy dog. ");
    }
    inputs.push(("text_150k".into(), text));

    // Deterministic pseudo-random (incompressible) data.
    let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut random = Vec::with_capacity(600_000);
    for _ in 0..600_000 {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        random.push((state >> 33) as u8);
    }
    inputs.push(("random_600k".into(), random));

    let mut structured = Vec::new();
    for i in 0u32..70_000 {
        structured.extend_from_slice(&(i % 251).to_le_bytes());
    }
    inputs.push(("structured_280k".into(), structured));

    for size in [255usize, 256, 131_071, 131_072, 131_073] {
        let mut data = Vec::with_capacity(size);
        while data.len() < size {
            data.extend_from_slice(b"abcdefghij0123456789");
        }
        data.truncate(size);
        inputs.push((format!("pattern_{size}"), data));
    }

    inputs
}

/// Decode direction: every frame the reference CLI produces — across the
/// full level/flag matrix — must decode byte-identically.
#[test]
fn oracle_decode_reference_frames() {
    if find_zstd().is_none() {
        skip_note("oracle_decode_reference_frames");
        return;
    }

    let flag_sets: &[&[&str]] = &[
        &["-1"],
        &["-3"],
        &["-6"],
        &["-9"],
        &["-12"],
        &["-19"],
        &["--ultra", "-22"],
        &["--long=24", "-6"],
        &["--no-check", "-3"],
        &["--no-content-size", "-3"],
    ];

    let mut total = 0usize;
    let mut passed = 0usize;
    for (name, data) in test_inputs() {
        for flags in flag_sets {
            total += 1;
            let frame = zstd_compress(&data, flags)
                .unwrap_or_else(|e| panic!("reference compress {name} {flags:?}: {e}"));
            match oxiarc_zstd::decompress_multi_frame(&frame) {
                Ok(out) if out == data => passed += 1,
                Ok(out) => panic!(
                    "[{name} {flags:?}] decoded {} bytes, expected {} (content mismatch)",
                    out.len(),
                    data.len()
                ),
                Err(e) => panic!("[{name} {flags:?}] oxiarc failed to decode: {e}"),
            }
        }
    }
    assert_eq!(passed, total);
    eprintln!("[zstd-oracle] decode direction: {passed}/{total} reference frames byte-identical");
}

/// Decode direction: concatenated frames (including a skippable frame in the
/// middle) decode to the concatenated content.
#[test]
fn oracle_decode_multi_frame_concatenation() {
    if find_zstd().is_none() {
        skip_note("oracle_decode_multi_frame_concatenation");
        return;
    }

    let part1 = b"ABCDEFGH".repeat(13);
    let part2 = vec![0u8; 50_000];
    let part3: Vec<u8> = (0..70_000u32).map(|i| (i % 251) as u8).collect();

    let mut combined = zstd_compress(&part1, &["-3"]).expect("compress part1");
    combined.extend_from_slice(&oxiarc_zstd::write_skippable_frame(b"metadata", 5));
    combined.extend_from_slice(&zstd_compress(&part2, &["-1"]).expect("compress part2"));
    combined.extend_from_slice(&zstd_compress(&part3, &["-19"]).expect("compress part3"));

    let mut expected = part1.clone();
    expected.extend_from_slice(&part2);
    expected.extend_from_slice(&part3);

    let decoded = oxiarc_zstd::decompress_multi_frame(&combined).expect("multi-frame decode");
    assert_eq!(decoded, expected);
}

/// Encode direction: every frame oxiarc produces must be accepted by the
/// reference CLI and decode to the original input.
#[test]
fn oracle_encode_reference_accepts() {
    if find_zstd().is_none() {
        skip_note("oracle_encode_reference_accepts");
        return;
    }

    let mut total = 0usize;
    let mut passed = 0usize;
    for (name, data) in test_inputs() {
        for level in [0i32, 1, 3, 9, 19] {
            total += 1;
            let frame = if level == 0 {
                oxiarc_zstd::compress(&data)
            } else {
                oxiarc_zstd::encode_all(&data, level)
            }
            .unwrap_or_else(|e| panic!("oxiarc compress {name} L{level}: {e}"));

            match zstd_decompress(&frame) {
                Ok(out) if out == data => passed += 1,
                Ok(out) => panic!(
                    "[{name} L{level}] reference decoded {} bytes, expected {}",
                    out.len(),
                    data.len()
                ),
                Err(e) => panic!("[{name} L{level}] reference zstd REJECTED oxiarc frame: {e}"),
            }
        }
    }
    assert_eq!(passed, total);
    eprintln!("[zstd-oracle] encode direction: {passed}/{total} oxiarc frames accepted + correct");
}

/// Encode direction: header option variants (no checksum, no content size)
/// must also be reference-decodable — including the 255/256 boundary that
/// the pre-fix encoder corrupted.
#[test]
fn oracle_encode_header_variants() {
    if find_zstd().is_none() {
        skip_note("oracle_encode_header_variants");
        return;
    }

    for size in [0usize, 1, 255, 256, 257, 1000, 100_000, 200_000] {
        let data: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
        for (checksum, content_size) in [(false, true), (true, false), (false, false)] {
            let mut encoder = oxiarc_zstd::ZstdEncoder::new();
            encoder.set_level(3);
            encoder.set_checksum(checksum);
            encoder.set_content_size(content_size);
            let frame = encoder.compress(&data).expect("compress");
            let out = zstd_decompress(&frame).unwrap_or_else(|e| {
                panic!("size {size} checksum={checksum} content_size={content_size}: {e}")
            });
            assert_eq!(
                out, data,
                "size {size} checksum={checksum} content_size={content_size}: wrong content"
            );
        }
    }
}

/// Dictionary interop, both directions, using a raw-content dictionary
/// (RFC 8878 §5 — no magic, no Dictionary_ID; `zstd -D` treats any
/// magic-less file as raw content).
#[test]
fn oracle_dictionary_both_directions() {
    if find_zstd().is_none() {
        skip_note("oracle_dictionary_both_directions");
        return;
    }

    let dict: Vec<u8> =
        b"GET /api/v1/users HTTP/1.1\r\nHost: example.com\r\nContent-Type: application/json\r\n"
            .repeat(30);
    let payload =
        br#"{"user": "alice", "action": "GET /api/v1/users HTTP/1.1", "host": "example.com"}"#
            .repeat(20);

    // Unique scratch dir (the CLI needs the dictionary as a file).
    let mut dir = std::env::temp_dir();
    dir.push(format!(
        "oxiarc_zstd_oracle_dict_{}_{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    let dict_path = dir.join("rawdict.bin");
    std::fs::write(&dict_path, &dict).expect("write dict");
    let dict_arg = dict_path.to_string_lossy().to_string();

    // Reference-compress with the dictionary -> oxiarc-decode.
    for level in ["-1", "-3", "-19"] {
        let frame = run_zstd(&["-q", "-c", level, "-D", &dict_arg], &payload)
            .expect("reference dict compress");
        let decoded = oxiarc_zstd::decompress_multi_frame_with_dict(&frame, &dict)
            .unwrap_or_else(|e| panic!("oxiarc failed on reference dict frame ({level}): {e}"));
        assert_eq!(
            decoded, payload,
            "reference dict frame {level}: wrong bytes"
        );
    }

    // oxiarc-compress with the dictionary -> reference-decode.
    let mut encoder = oxiarc_zstd::ZstdEncoder::new();
    encoder.set_level(3);
    encoder.set_dictionary(&dict);
    let frame = encoder.compress(&payload).expect("oxiarc dict compress");
    let out = run_zstd(&["-d", "-q", "-c", "-D", &dict_arg], &frame)
        .expect("reference zstd rejected oxiarc dictionary frame");
    assert_eq!(
        out, payload,
        "oxiarc dict frame: reference decoded wrong bytes"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// Streaming writer output must be reference-decodable frame-for-frame.
#[test]
fn oracle_streaming_writer_frames() {
    if find_zstd().is_none() {
        skip_note("oracle_streaming_writer_frames");
        return;
    }

    let mut payload = Vec::new();
    while payload.len() < 300_000 {
        payload.extend_from_slice(b"streaming zstd writer interop check ");
    }

    let mut buffer = Vec::new();
    {
        let mut writer = oxiarc_zstd::ZstdWriter::new(&mut buffer, 3);
        for chunk in payload.chunks(7_777) {
            writer.write_all(chunk).expect("write chunk");
        }
        writer.finish().expect("finish");
    }

    let out = zstd_decompress(&buffer).expect("reference zstd rejected streaming output");
    assert_eq!(out, payload, "streaming frames: wrong content");
}
