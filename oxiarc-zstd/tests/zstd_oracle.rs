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

// ---------------------------------------------------------------------------
// FSE_Compressed_Mode sequence tables
// ---------------------------------------------------------------------------

/// Sequence-section compression modes of one compressed block, as the 2-bit
/// codes RFC 8878 §3.1.1.3.2.1.1 defines (0 predefined, 1 RLE, 2 FSE, 3 repeat).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BlockSequenceModes {
    literal_length: u8,
    offset: u8,
    match_length: u8,
}

/// Walk a zstd frame and report the sequence compression modes of every
/// compressed block it contains.
///
/// This exists so the oracle test can prove it is *actually* exercising
/// `FSE_Compressed_Mode` rather than passing vacuously on predefined-table
/// frames. It is an independent re-reading of the frame layout from RFC 8878
/// §3.1.1, deliberately not sharing code with the encoder or the decoder.
///
/// Returns `Err` with a description if the frame does not parse.
fn sequence_modes(frame: &[u8]) -> Result<Vec<BlockSequenceModes>, String> {
    let read_u24 = |data: &[u8], at: usize| -> Result<u32, String> {
        let bytes = data
            .get(at..at + 3)
            .ok_or_else(|| format!("truncated at {at}"))?;
        Ok(u32::from(bytes[0]) | (u32::from(bytes[1]) << 8) | (u32::from(bytes[2]) << 16))
    };

    if frame.len() < 6 || frame[..4] != [0x28, 0xB5, 0x2F, 0xFD] {
        return Err("not a zstd frame".into());
    }
    let descriptor = frame[4];
    let fcs_flag = descriptor >> 6;
    let single_segment = (descriptor >> 5) & 1 == 1;
    let has_checksum = (descriptor >> 2) & 1 == 1;
    let dict_id_flag = descriptor & 3;

    let mut pos = 5;
    if !single_segment {
        pos += 1; // Window_Descriptor
    }
    pos += match dict_id_flag {
        0 => 0,
        1 => 1,
        2 => 2,
        _ => 4,
    };
    pos += match fcs_flag {
        0 if single_segment => 1,
        0 => 0,
        1 => 2,
        2 => 4,
        _ => 8,
    };

    let mut modes = Vec::new();
    loop {
        let header = read_u24(frame, pos)?;
        pos += 3;
        let last_block = header & 1 == 1;
        let block_type = (header >> 1) & 3;
        let block_size = (header >> 3) as usize;

        if block_type == 2 {
            let block = frame
                .get(pos..pos + block_size)
                .ok_or_else(|| format!("compressed block at {pos} runs past the frame"))?;
            if let Some(found) = block_sequence_modes(block)? {
                modes.push(found);
            }
        }
        if block_type == 1 {
            pos += 1; // RLE block stores a single byte
        } else {
            pos += block_size;
        }
        if last_block {
            break;
        }
    }
    if has_checksum {
        pos += 4;
    }
    if pos != frame.len() {
        return Err(format!("frame has {} trailing bytes", frame.len() - pos));
    }
    Ok(modes)
}

/// Parse one Compressed_Block's literals section (to learn its length) and
/// then the sequences-section header. Returns `None` when the block has zero
/// sequences, which carries no modes byte.
fn block_sequence_modes(block: &[u8]) -> Result<Option<BlockSequenceModes>, String> {
    let first = *block.first().ok_or("empty compressed block")?;
    let literals_type = first & 3;
    let size_format = (first >> 2) & 3;

    let byte = |at: usize| -> Result<usize, String> {
        block
            .get(at)
            .map(|&b| usize::from(b))
            .ok_or_else(|| format!("literals header truncated at {at}"))
    };

    let literals_len = match literals_type {
        // Raw_Literals_Block / RLE_Literals_Block
        0 | 1 => {
            let (header_len, regenerated) = match size_format {
                0 | 2 => (1, usize::from(first) >> 3),
                1 => (2, (usize::from(first) >> 4) | (byte(1)? << 4)),
                _ => (
                    3,
                    (usize::from(first) >> 4) | (byte(1)? << 4) | (byte(2)? << 12),
                ),
            };
            header_len + if literals_type == 0 { regenerated } else { 1 }
        }
        // Compressed_Literals_Block / Treeless_Literals_Block
        _ => {
            let (header_len, compressed) = match size_format {
                0 | 1 => {
                    let value = usize::from(first) >> 4 | (byte(1)? << 4) | (byte(2)? << 12);
                    (3, (value >> 10) & 0x3FF)
                }
                2 => {
                    let value = usize::from(first) >> 4
                        | (byte(1)? << 4)
                        | (byte(2)? << 12)
                        | (byte(3)? << 20);
                    (4, (value >> 14) & 0x3FFF)
                }
                _ => {
                    let value = usize::from(first) >> 4
                        | (byte(1)? << 4)
                        | (byte(2)? << 12)
                        | (byte(3)? << 20)
                        | (byte(4)? << 28);
                    (5, (value >> 18) & 0x3FFFF)
                }
            };
            header_len + compressed
        }
    };

    let sequences = block
        .get(literals_len..)
        .ok_or("literals section runs past the block")?;
    let count_first = *sequences.first().ok_or("missing sequence count")?;
    let (count, count_len) = if count_first < 128 {
        (usize::from(count_first), 1)
    } else if count_first < 255 {
        let second = *sequences.get(1).ok_or("truncated sequence count")?;
        (
            ((usize::from(count_first) - 128) << 8) + usize::from(second),
            2,
        )
    } else {
        let low = *sequences.get(1).ok_or("truncated sequence count")?;
        let high = *sequences.get(2).ok_or("truncated sequence count")?;
        (usize::from(low) + (usize::from(high) << 8) + 0x7F00, 3)
    };
    if count == 0 {
        return Ok(None);
    }
    let modes_byte = *sequences.get(count_len).ok_or("missing modes byte")?;
    Ok(Some(BlockSequenceModes {
        literal_length: (modes_byte >> 6) & 3,
        offset: (modes_byte >> 4) & 3,
        match_length: (modes_byte >> 2) & 3,
    }))
}

/// The frame walker must agree with the reference on frames the reference
/// produced, so a bug in the walker cannot make the FSE assertion below pass
/// or fail for the wrong reason.
#[test]
fn oracle_frame_walker_parses_reference_frames() {
    if find_zstd().is_none() {
        skip_note("oracle_frame_walker_parses_reference_frames");
        return;
    }
    for (name, data) in test_inputs() {
        if data.is_empty() {
            continue;
        }
        for level in ["-1", "-9", "-19"] {
            let frame = zstd_compress(&data, &[level])
                .unwrap_or_else(|e| panic!("reference compress {name} {level}: {e}"));
            sequence_modes(&frame).unwrap_or_else(|e| {
                panic!("frame walker failed on reference frame {name} {level}: {e}")
            });
        }
    }
}

/// Encode direction, `FSE_Compressed_Mode`: oxiarc must emit custom sequence
/// tables on data whose symbol distribution warrants them, and the reference
/// `zstd` must accept those frames and reproduce the input byte for byte.
///
/// Without the mode assertion this test would pass on predefined-table frames
/// and prove nothing about the feature it is named for.
#[test]
fn oracle_encode_fse_compressed_sequence_tables() {
    if find_zstd().is_none() {
        skip_note("oracle_encode_fse_compressed_sequence_tables");
        return;
    }

    // Highly structured records: long runs of repeated field values separated
    // by short varying keys. This produces many sequences whose literal-length,
    // match-length and offset codes cluster tightly, which is exactly the case
    // the RFC's flat predefined distributions model badly.
    let mut data = Vec::new();
    for record in 0u32..24_000 {
        data.extend_from_slice(b"id=");
        data.extend_from_slice(format!("{:06}", record % 4096).as_bytes());
        data.extend_from_slice(b";name=alpha;state=ACTIVE;region=eu-north-1;score=");
        data.extend_from_slice(format!("{:03}", record % 97).as_bytes());
        data.extend_from_slice(b";\n");
    }

    let mut saw_fse = false;
    for level in [1i32, 3, 9, 19] {
        let frame = oxiarc_zstd::encode_all(&data, level)
            .unwrap_or_else(|e| panic!("oxiarc compress L{level}: {e}"));

        let modes = sequence_modes(&frame)
            .unwrap_or_else(|e| panic!("oxiarc frame L{level} does not parse: {e}"));
        if modes
            .iter()
            .any(|m| m.literal_length == 2 || m.offset == 2 || m.match_length == 2)
        {
            saw_fse = true;
        }

        let decoded = zstd_decompress(&frame)
            .unwrap_or_else(|e| panic!("reference zstd REJECTED oxiarc L{level} frame: {e}"));
        assert_eq!(
            decoded.len(),
            data.len(),
            "reference decoded the wrong length at L{level}"
        );
        assert!(
            decoded == data,
            "reference decode differs from the input at L{level}"
        );
    }

    assert!(
        saw_fse,
        "no block used FSE_Compressed_Mode; the oracle check would be vacuous"
    );
    eprintln!("[zstd-oracle] FSE_Compressed_Mode sequence tables accepted by reference zstd");
}

// ---------------------------------------------------------------------------
// Incremental decode leg
// ---------------------------------------------------------------------------

/// Drive `ZstdStream` over `frame`, feeding `in_chunk` bytes per call and
/// taking at most `out_chunk` bytes back, and return the decoded output.
fn incremental_decode(frame: &[u8], in_chunk: usize, out_chunk: usize) -> Result<Vec<u8>, String> {
    use oxiarc_core::traits::FlushMode;
    use oxiarc_zstd::{ZstdStatus, ZstdStream};

    // Reference frames made with `--long` declare 16-128 MiB windows, so the
    // oracle raises the declared-window ceiling; the ring still only grows to
    // the number of bytes actually produced.
    let mut stream = ZstdStream::new().with_max_window(usize::MAX);
    let mut out = Vec::new();
    let mut scratch = vec![0u8; out_chunk.max(1)];
    let mut pos = 0usize;
    let mut calls = 0usize;

    loop {
        calls += 1;
        if calls > 40_000_000 {
            return Err("decoder did not terminate".to_string());
        }
        let end = pos.saturating_add(in_chunk).min(frame.len());
        let flush = if end == frame.len() {
            FlushMode::Finish
        } else {
            FlushMode::None
        };
        let progress = stream
            .decode(&frame[pos..end], &mut scratch, flush)
            .map_err(|e| e.to_string())?;
        pos += progress.consumed;
        out.extend_from_slice(&scratch[..progress.produced]);
        if progress.status == ZstdStatus::StreamEnd {
            stream.finish().map_err(|e| e.to_string())?;
            return Ok(out);
        }
    }
}

/// Decode direction, incremental: every reference frame across the full
/// level/flag matrix must decode byte-identically through the *push* decoder,
/// at several chunk schedules including one byte in / one byte out.
#[test]
fn oracle_incremental_decode_reference_frames() {
    if find_zstd().is_none() {
        skip_note("oracle_incremental_decode_reference_frames");
        return;
    }

    let flag_sets: &[&[&str]] = &[
        &["-1"],
        &["-3"],
        &["-9"],
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
            let frame = zstd_compress(&data, flags)
                .unwrap_or_else(|e| panic!("reference compress {name} {flags:?}: {e}"));

            // Big payloads only get the coarse schedules so the suite stays
            // fast; small ones get the pathological ones too.
            let mut schedules: Vec<(usize, usize)> =
                vec![(usize::MAX, 1 << 16), (1, 1 << 16), (4096, 37), (13, 7)];
            if data.len() <= 4096 {
                schedules.push((1, 1));
                schedules.push((3, 5));
            }

            for (ic, oc) in schedules {
                total += 1;
                match incremental_decode(&frame, ic, oc) {
                    Ok(out) if out == data => passed += 1,
                    Ok(out) => panic!(
                        "[{name} {flags:?} {ic}/{oc}] incremental decode produced {} bytes, expected {}",
                        out.len(),
                        data.len()
                    ),
                    Err(e) => panic!("[{name} {flags:?} {ic}/{oc}] incremental decode failed: {e}"),
                }
            }
        }
    }
    assert_eq!(passed, total);
    eprintln!(
        "[zstd-oracle] incremental decode: {passed}/{total} reference frame/chunk-schedule pairs byte-identical"
    );
}

/// The bounded one-shot helpers agree with the reference CLI, and their caps
/// are honoured on reference frames.
#[test]
fn oracle_bounded_helpers_on_reference_frames() {
    if find_zstd().is_none() {
        skip_note("oracle_bounded_helpers_on_reference_frames");
        return;
    }

    let mut checked = 0usize;
    for (name, data) in test_inputs() {
        for flags in [
            &["-3"][..],
            &["--long=24", "-6"][..],
            &["--no-check", "-3"][..],
        ] {
            let frame = zstd_compress(&data, flags)
                .unwrap_or_else(|e| panic!("reference compress {name} {flags:?}: {e}"));

            let out = oxiarc_zstd::decompress_with_limit(&frame, data.len())
                .unwrap_or_else(|e| panic!("[{name} {flags:?}] decompress_with_limit: {e}"));
            assert_eq!(out, data, "[{name} {flags:?}] decompress_with_limit");

            let mut dst = vec![0u8; data.len()];
            let n = oxiarc_zstd::decompress_into(&frame, &mut dst)
                .unwrap_or_else(|e| panic!("[{name} {flags:?}] decompress_into: {e}"));
            assert_eq!(n, data.len(), "[{name} {flags:?}] decompress_into length");
            assert_eq!(dst, data, "[{name} {flags:?}] decompress_into content");

            if !data.is_empty() {
                assert!(
                    oxiarc_zstd::decompress_with_limit(&frame, data.len() - 1).is_err(),
                    "[{name} {flags:?}] a tight cap must be enforced"
                );
            }
            checked += 1;
        }
    }
    eprintln!("[zstd-oracle] bounded helpers: {checked} reference frames within cap");
}

/// A truncated reference frame must be an error through the push decoder — a
/// short `Ok` would be silent data loss.
#[test]
fn oracle_incremental_truncation_is_an_error() {
    if find_zstd().is_none() {
        skip_note("oracle_incremental_truncation_is_an_error");
        return;
    }

    let data = b"truncate me at every quarter ".repeat(400);
    let frame = zstd_compress(&data, &["-6"]).expect("reference compress");
    for cut in [1usize, frame.len() / 4, frame.len() / 2, frame.len() - 1] {
        let result = incremental_decode(&frame[..cut], usize::MAX, 1 << 16);
        assert!(
            result.is_err(),
            "truncation at {cut}/{} decoded successfully",
            frame.len()
        );
    }
}

/// The single most important differential for a windowed decoder: reference
/// frames whose payload is **far larger than their declared `Window_Size`**.
///
/// Everything else in this file compresses at most 600 KB, and `zstd`'s default
/// window at level 3+ is 2 MiB, so no other test ever makes the ring wrap. Here
/// the payload is 4 MiB against declared windows of 128 KiB and 1 KiB, so the
/// ring wraps 32x and 4096x respectively and every back-reference lands at or
/// near the window boundary — exactly the arithmetic that the old
/// "whole output is the window" decoder never had to get right.
#[test]
fn oracle_incremental_small_window_large_payload() {
    use oxiarc_core::traits::FlushMode;
    use oxiarc_zstd::{ZstdStatus, ZstdStream};

    if find_zstd().is_none() {
        skip_note("oracle_incremental_small_window_large_payload");
        return;
    }

    // Compressible enough that the encoder emits long matches, and long enough
    // to wrap even a 1 KiB window thousands of times.
    let mut raw = Vec::with_capacity(4 << 20);
    let mut i = 0u32;
    while raw.len() < (4 << 20) {
        raw.extend_from_slice(
            format!("record {i:08} name=widget-{} qty={}\n", i % 97, i % 13).as_bytes(),
        );
        i += 1;
    }
    raw.truncate(4 << 20);

    let flag_sets: &[&[&str]] = &[
        &["--zstd=wlog=17", "-6"],  // 128 KiB window, 32x wrap
        &["--zstd=wlog=10", "-3"],  // 1 KiB window, 4096x wrap
        &["--long=17", "-9"],       // long mode with a small window
        &["--zstd=wlog=11", "-19"], // deep search, 2 KiB window
    ];

    for flags in flag_sets {
        let frame = zstd_compress(&raw, flags)
            .unwrap_or_else(|e| panic!("reference compress {flags:?}: {e}"));

        // The declared window really is small, or the test proves nothing.
        assert_eq!(frame[4] & 0x20, 0, "{flags:?}: expected a windowed frame");
        let wd = frame[5];
        let base = 1u64 << (10 + u32::from(wd >> 3));
        let declared = base + (base >> 3) * u64::from(wd & 7);
        assert!(
            declared <= 256 * 1024,
            "{flags:?}: declared window {declared} is not smaller than the payload"
        );

        for chunk in [usize::MAX, 4096, 1] {
            let mut stream = ZstdStream::new().with_max_window(usize::MAX);
            let mut out = Vec::with_capacity(raw.len());
            let mut scratch = vec![0u8; 64 * 1024];
            let mut pos = 0usize;
            loop {
                let end = pos.saturating_add(chunk).min(frame.len());
                let flush = if end == frame.len() {
                    FlushMode::Finish
                } else {
                    FlushMode::None
                };
                let progress = stream
                    .decode(&frame[pos..end], &mut scratch, flush)
                    .unwrap_or_else(|e| panic!("[{flags:?} chunk {chunk}] decode failed: {e}"));
                pos += progress.consumed;
                out.extend_from_slice(&scratch[..progress.produced]);
                if progress.status == ZstdStatus::StreamEnd {
                    break;
                }
            }
            assert!(
                out == raw,
                "[{flags:?} chunk {chunk}] wrapped-ring decode differs from the input"
            );
            // The whole 4 MiB was produced through a ring no larger than the
            // frame's declared window.
            assert!(
                stream.window_size() as u64 <= declared.max(1),
                "[{flags:?}] window grew to {} for a declared {declared}",
                stream.window_size()
            );
        }

        // And through the bounded one-shot helper.
        let got = oxiarc_zstd::decompress_with_limit(&frame, raw.len())
            .unwrap_or_else(|e| panic!("[{flags:?}] decompress_with_limit: {e}"));
        assert!(got == raw, "[{flags:?}] decompress_with_limit differs");
    }

    eprintln!(
        "[zstd-oracle] wrapped-ring decode: 4 MiB through 1-128 KiB declared windows, byte-identical"
    );
}
