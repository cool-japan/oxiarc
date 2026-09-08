//! Regression suite for the four legacy-vs-streaming divergences that
//! differential fuzzing found between the legacy one-shot decoders
//! ([`oxiarc_zstd::decompress`], [`oxiarc_zstd::decompress_multi_frame`], the
//! [`oxiarc_zstd::ZstdDecoder`] core they share) and the bounded push decoder
//! [`ZstdStream`].
//!
//! `fuzz/fuzz_targets/fuzz_zstd_stream.rs` compared the pair and found, in
//! roughly ten cumulative minutes, four independently-rooted inputs the legacy
//! path accepted and `ZstdStream` refused:
//!
//! 1. a frame naming a `Dictionary_ID` the caller never supplied;
//! 2. a block regenerating more than `min(Window_Size, 128 KiB)`;
//! 3. a frame declaring an ~11 MB `Window_Size`;
//! 4. leading garbage — fewer than four bytes, an unknown magic, or a
//!    truncated skippable frame — before any frame had been decoded.
//!
//! 1, 2 and 4 are now format rules both paths enforce from shared code; 3 is a
//! documented, deliberate split (see `window_declaration_split`). Every test
//! below drives **both** paths over the same bytes and asserts they agree on
//! accept/refuse, and on the bytes themselves whenever both accept.
//!
//! The inputs marked `crash-*` are the fuzzer's own minimised artifacts,
//! embedded verbatim (minus the leading byte the target consumes to pick a
//! chunk granularity) so the suite stays hermetic.

use oxiarc_core::error::OxiArcError;
use oxiarc_core::traits::FlushMode;
use oxiarc_zstd::{
    MAX_BLOCK_SIZE, ZstdDecoder, ZstdStatus, ZstdStream, compress_with_level, decompress,
    decompress_frame, decompress_multi_frame, decompress_multi_frame_with_dict,
    decompress_multi_frame_with_limit, decompress_with_dict, decompress_with_limit,
    write_skippable_frame,
};

const ZSTD_MAGIC: [u8; 4] = [0x28, 0xB5, 0x2F, 0xFD];

/// Drive `ZstdStream` over `data` in `chunk`-byte pieces.
///
/// The final piece is fed with [`FlushMode::Finish`], and `finish()` is the
/// arbiter for a stream that merely stopped wanting input.
fn stream_decode(data: &[u8], multi_frame: bool, chunk: usize) -> Result<Vec<u8>, OxiArcError> {
    let mut stream = ZstdStream::new().with_multi_frame(multi_frame);
    let mut out = Vec::new();
    let mut sink = [0u8; 61];
    let mut pos = 0usize;
    let mut guard = 0usize;
    loop {
        guard += 1;
        assert!(guard < 1_000_000, "no progress after {guard} calls");
        let end = pos.saturating_add(chunk).min(data.len());
        let flush = if end == data.len() {
            FlushMode::Finish
        } else {
            FlushMode::None
        };
        let progress = stream.decode(&data[pos..end], &mut sink, flush)?;
        pos += progress.consumed;
        out.extend_from_slice(&sink[..progress.produced]);
        if progress.status == ZstdStatus::StreamEnd {
            stream.finish()?;
            return Ok(out);
        }
        if progress.consumed == 0 && progress.produced == 0 && end == data.len() {
            stream.finish()?;
            return Ok(out);
        }
    }
}

/// The multi-frame push decoder, fed whole and one byte at a time.
///
/// Both schedules must reach the same verdict; the returned value is that
/// shared verdict.
fn via_stream(data: &[u8]) -> Result<Vec<u8>, OxiArcError> {
    let whole = stream_decode(data, true, usize::MAX);
    let byte_wise = stream_decode(data, true, 1);
    match (&whole, &byte_wise) {
        (Ok(a), Ok(b)) => assert_eq!(a, b, "ZstdStream disagreed with itself across schedules"),
        (Err(_), Err(_)) => {}
        _ => panic!("ZstdStream schedule-dependent verdict: {whole:?} vs {byte_wise:?}"),
    }
    whole
}

/// Assert that every entry point refuses `data`, and that the two families
/// agree on *why* when `expect` is given.
fn both_refuse(what: &str, data: &[u8], expect: &str) {
    let legacy = decompress_multi_frame(data);
    let stream = via_stream(data);
    let bounded = decompress_multi_frame_with_limit(data, 1 << 20);
    let legacy_err = legacy
        .as_ref()
        .err()
        .unwrap_or_else(|| panic!("[{what}] decompress_multi_frame accepted: {legacy:?}"))
        .to_string();
    let stream_err = stream
        .as_ref()
        .err()
        .unwrap_or_else(|| panic!("[{what}] ZstdStream accepted: {stream:?}"))
        .to_string();
    assert!(
        bounded.is_err(),
        "[{what}] decompress_multi_frame_with_limit accepted: {bounded:?}"
    );
    assert!(
        legacy_err.contains(expect),
        "[{what}] legacy error {legacy_err:?} does not mention {expect:?}"
    );
    assert!(
        stream_err.contains(expect),
        "[{what}] stream error {stream_err:?} does not mention {expect:?}"
    );
}

/// Assert that every entry point accepts `data` and produces `expected`.
fn both_accept(what: &str, data: &[u8], expected: &[u8]) {
    assert_eq!(
        decompress_multi_frame(data).unwrap_or_else(|e| panic!("[{what}] legacy: {e}")),
        expected,
        "[{what}] decompress_multi_frame"
    );
    assert_eq!(
        via_stream(data).unwrap_or_else(|e| panic!("[{what}] stream: {e}")),
        expected,
        "[{what}] ZstdStream"
    );
    assert_eq!(
        decompress_multi_frame_with_limit(data, 1 << 20)
            .unwrap_or_else(|e| panic!("[{what}] bounded: {e}")),
        expected,
        "[{what}] decompress_multi_frame_with_limit"
    );
}

/// Assemble a frame header plus blocks.
fn frame(descriptor: u8, header_rest: &[u8], body: &[u8]) -> Vec<u8> {
    let mut out = Vec::from(ZSTD_MAGIC);
    out.push(descriptor);
    out.extend_from_slice(header_rest);
    out.extend_from_slice(body);
    out
}

/// A block header: `Last_Block`, `Block_Type` and `Block_Size`.
fn block_header(last: bool, kind: u32, size: u32) -> [u8; 3] {
    let raw = u32::from(last) | (kind << 1) | (size << 3);
    let bytes = raw.to_le_bytes();
    [bytes[0], bytes[1], bytes[2]]
}

/// A one-block `Raw` frame with an explicit window descriptor.
fn raw_frame(window_descriptor: u8, payload: &[u8]) -> Vec<u8> {
    let mut body = Vec::from(block_header(true, 0, payload.len() as u32));
    body.extend_from_slice(payload);
    frame(0x00, &[window_descriptor], &body)
}

// ---------------------------------------------------------------------------
// Finding 1 — `Dictionary_ID` (RFC 8878 §3.1.1.1.1.6)
// ---------------------------------------------------------------------------

/// A frame naming a dictionary, decodable only when one is supplied.
fn dict_id_frame(id_flag: u8, id_bytes: &[u8]) -> Vec<u8> {
    let mut header_rest = vec![0x48]; // window descriptor: 2 MiB
    header_rest.extend_from_slice(id_bytes);
    let mut body = Vec::from(block_header(true, 0, 4));
    body.extend_from_slice(b"abcd");
    frame(id_flag, &header_rest, &body)
}

#[test]
fn dictionary_id_without_a_dictionary_is_refused_by_every_entry_point() {
    for (what, frame) in [
        ("1-byte ID", dict_id_frame(0x01, &[0x2A])),
        ("2-byte ID", dict_id_frame(0x02, &[0x2A, 0x00])),
        ("4-byte ID", dict_id_frame(0x03, &[0x2A, 0x00, 0x00, 0x00])),
    ] {
        both_refuse(what, &frame, "requires dictionary ID");

        // Every other legacy entry refuses it too, with the same message.
        for (name, result) in [
            ("decompress", decompress(&frame)),
            ("decompress_frame", decompress_frame(&frame).map(|(d, _)| d)),
            (
                "ZstdDecoder::decode_frame",
                ZstdDecoder::new().decode_frame(&frame),
            ),
            (
                "decompress_with_limit",
                decompress_with_limit(&frame, 1 << 20),
            ),
        ] {
            let err = result
                .err()
                .unwrap_or_else(|| panic!("[{what}] {name} accepted a dictionary-ID frame"));
            assert!(
                err.to_string().contains("requires dictionary ID"),
                "[{what}] {name}: {err}"
            );
            assert!(
                matches!(err, OxiArcError::InvalidHeader { .. }),
                "[{what}] {name} used the wrong error class: {err:?}"
            );
        }

        // Supplying a dictionary makes both families decode it identically.
        let dict = b"raw dictionary content".to_vec();
        assert_eq!(
            decompress_with_dict(&frame, &dict).expect("with_dict"),
            b"abcd"
        );
        assert_eq!(
            decompress_multi_frame_with_dict(&frame, &dict).expect("multi_with_dict"),
            b"abcd"
        );
        let mut stream = ZstdStream::new().with_dictionary(dict);
        let mut out = [0u8; 16];
        let progress = stream
            .decode(&frame, &mut out, FlushMode::Finish)
            .expect("stream with dictionary");
        assert_eq!(&out[..progress.produced], b"abcd");
    }
}

/// `Dictionary_ID` 0 means "no dictionary", whatever the flag width says.
#[test]
fn dictionary_id_zero_is_not_a_dictionary_requirement() {
    for (what, frame) in [
        ("1-byte zero", dict_id_frame(0x01, &[0x00])),
        (
            "4-byte zero",
            dict_id_frame(0x03, &[0x00, 0x00, 0x00, 0x00]),
        ),
    ] {
        both_accept(what, &frame, b"abcd");
    }
}

/// The fuzzer's own dictionary-ID artifacts (magic byte prefixes dropped).
#[test]
fn fuzzer_dictionary_id_artifacts_are_refused_on_both_paths() {
    // crash-44bb5b87: Single_Segment + Dictionary_ID_flag = 1, ID = 4.
    const CRASH_44BB: &[u8] = &[
        0x28, 0xb5, 0x2f, 0xfd, 0x39, 0x04, 0x00, 0x14, 0x00, 0x00, 0x00, 0x00, 0x14, 0x00, 0x00,
        0x00, 0x00, 0x2d, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xcd,
    ];
    // crash-fca3ff83: Dictionary_ID_flag = 3, ID = 0x2525.
    const CRASH_FCA3: &[u8] = &[
        0x28, 0xb5, 0x2f, 0xfd, 0x03, 0x00, 0x25, 0x25, 0x00, 0x00, 0x23, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x23, 0x00, 0x00, 0x00, 0x00, 0x00,
    ];
    both_refuse("crash-44bb5b87", CRASH_44BB, "requires dictionary ID");
    both_refuse("crash-fca3ff83", CRASH_FCA3, "requires dictionary ID");
}

// ---------------------------------------------------------------------------
// Finding 2 — `Block_Maximum_Decompressed_Size`
// ---------------------------------------------------------------------------

#[test]
fn a_block_larger_than_the_frame_maximum_is_refused_on_both_paths() {
    // Window_Descriptor 0x00 = the format's smallest window, 1024 bytes, so
    // Block_Maximum_Decompressed_Size is 1024.
    let oversized_raw = raw_frame(0x00, &vec![b'X'; 1025]);
    both_refuse(
        "raw block over a 1 KiB window",
        &oversized_raw,
        "exceeds the frame maximum 1024",
    );

    // An RLE block declares a regenerated size of 64 KiB from one input byte;
    // the refusal happens before those bytes exist.
    let mut body = Vec::from(block_header(true, 1, 65536));
    body.push(b'Y');
    let rle_bomb = frame(0x00, &[0x00], &body);
    both_refuse(
        "rle block over a 1 KiB window",
        &rle_bomb,
        "exceeds the frame maximum 1024",
    );

    // A declared Frame_Content_Size lowers the ceiling further: Single_Segment
    // with FCS = 10 caps every block at 10 bytes.
    let mut body = Vec::from(block_header(true, 0, 11));
    body.extend_from_slice(b"12345678901");
    let over_content_size = frame(0x20, &[10], &body);
    both_refuse(
        "raw block over a declared content size",
        &over_content_size,
        "exceeds the frame maximum 10",
    );

    // A *Compressed* block does not declare its regenerated size, so this is
    // the case the legacy path could not see before: an RLE literals section
    // of 4095 bytes and no sequences, inside a frame whose window is 1 KiB.
    // The literals decoder's own 128 KiB cap does not fire — only the frame
    // maximum does, charged before the bytes are produced.
    let payload = [0xF5, 0xFF, b'L', 0x00]; // RLE literals, 4095 bytes; 0 sequences
    let mut body = Vec::from(block_header(true, 2, payload.len() as u32));
    body.extend_from_slice(&payload);
    let compressed_over = frame(0x00, &[0x00], &body);
    both_refuse(
        "compressed block over a 1 KiB window",
        &compressed_over,
        "block regenerated size 4095 exceeds the frame maximum 1024",
    );

    // The 128 KiB absolute ceiling is checked earlier still, from the block
    // header's own `Block_Size` field, with the same message on both paths.
    let mut body = Vec::from(block_header(true, 1, MAX_BLOCK_SIZE as u32 + 1));
    body.push(b'Z');
    let over_absolute = frame(0x00, &[0x48], &body);
    both_refuse(
        "rle block over 128 KiB",
        &over_absolute,
        &format!("block size {} exceeds maximum", MAX_BLOCK_SIZE + 1),
    );
}

#[test]
fn a_block_exactly_at_the_frame_maximum_is_accepted_on_both_paths() {
    let payload = vec![b'X'; 1024];
    both_accept(
        "raw block filling a 1 KiB window",
        &raw_frame(0x00, &payload),
        &payload,
    );

    let mut body = Vec::from(block_header(true, 1, 1024));
    body.push(b'Y');
    both_accept(
        "rle block filling a 1 KiB window",
        &frame(0x00, &[0x00], &body),
        &vec![b'Y'; 1024],
    );

    // Real frames from this crate's own encoder stay inside the ceiling.
    for size in [0usize, 1, 4096, MAX_BLOCK_SIZE, MAX_BLOCK_SIZE + 7, 400_000] {
        let data: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
        let encoded = compress_with_level(&data, 3).expect("compress");
        both_accept(&format!("encoder output, {size} bytes"), &encoded, &data);
    }
}

/// A *compressed* block whose sequences regenerate past the ceiling: the
/// fuzzer's artifact, refused by both paths without materialising the bytes.
#[test]
fn fuzzer_block_maximum_artifact_is_refused_on_both_paths() {
    // crash-83879ff8: window 1024, one Compressed block.
    const CRASH_8387: &[u8] = &[
        0x28, 0xb5, 0x2f, 0xfd, 0x00, 0x00, 0xfd, 0x00, 0x00, 0x2d, 0x00, 0x0a, 0x36, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x33, 0x33, 0x33, 0x33, 0x33, 0x33, 0x33, 0x33,
        0x33, 0x33, 0x33, 0xbe, 0x33, 0x33, 0x4d, 0x33, 0x33, 0x33, 0x30, 0x33, 0x01, 0x00, 0x00,
        0x33, 0x28,
    ];
    let legacy = decompress_multi_frame(CRASH_8387);
    let stream = via_stream(CRASH_8387);
    assert!(
        legacy.is_err(),
        "legacy accepted crash-83879ff8: {legacy:?}"
    );
    assert!(
        stream.is_err(),
        "stream accepted crash-83879ff8: {stream:?}"
    );
}

// ---------------------------------------------------------------------------
// Finding 3 — the declared `Window_Size` split
// ---------------------------------------------------------------------------

/// Window_Descriptor 0x6b: exponent 13, mantissa 3 -> 11,534,336 bytes.
const WINDOW_11MB: u8 = 0x6B;
/// Window_Descriptor 0xA8: exponent 21, mantissa 0 -> 2,147,483,648 bytes.
const WINDOW_2GB: u8 = 0xA8;

#[test]
fn window_declaration_split() {
    let payload = b"declared windows are not output sizes";
    let big_window = raw_frame(WINDOW_11MB, payload);

    // The unbounded legacy path accepts it: its output `Vec` is the window, so
    // an 11 MB declaration costs nothing.
    assert_eq!(decompress(&big_window).expect("legacy single"), payload);
    assert_eq!(
        decompress_multi_frame(&big_window).expect("legacy multi"),
        payload
    );

    // `ZstdStream` keeps a real ring, so its 8 MiB default refuses it.
    let err = via_stream(&big_window).expect_err("stream must refuse 11 MB");
    assert!(
        matches!(
            err,
            OxiArcError::MemoryBudgetExceeded {
                budget: 8_388_608,
                requested: 11_534_336
            }
        ),
        "unexpected refusal: {err:?}"
    );

    // Raising the streaming ceiling makes both sides agree byte for byte.
    let mut stream = ZstdStream::new().with_max_window(16 << 20);
    let mut out = vec![0u8; payload.len()];
    let progress = stream
        .decode(&big_window, &mut out, FlushMode::Finish)
        .expect("raised ceiling");
    assert_eq!(&out[..progress.produced], payload);

    // The bounded one-shot helpers accept it too: an 11 MB declaration is below
    // the reference decoder's own 128 MiB ceiling, and a piped `zstd -3` frame
    // declares 2 MiB whatever its payload, so tying the ceiling to the output
    // limit would reject ordinary reference frames.
    assert_eq!(
        decompress_with_limit(&big_window, 1 << 20).expect("bounded single"),
        payload
    );
    assert_eq!(
        decompress_multi_frame_with_limit(&big_window, 1 << 20).expect("bounded multi"),
        payload
    );

    // What the bounded helpers do refuse is a declaration past 128 MiB — the
    // same frame `zstd -d` rejects with "Window size larger than maximum".
    let absurd = raw_frame(WINDOW_2GB, payload);
    assert_eq!(
        decompress(&absurd).expect("legacy accepts any declaration"),
        payload
    );
    for (name, result) in [
        (
            "decompress_with_limit",
            decompress_with_limit(&absurd, 1 << 20),
        ),
        (
            "decompress_multi_frame_with_limit",
            decompress_multi_frame_with_limit(&absurd, 1 << 20),
        ),
    ] {
        let err = result.expect_err("a 2 GiB window must be refused");
        assert!(
            matches!(
                err,
                OxiArcError::MemoryBudgetExceeded {
                    budget: 134_217_728,
                    requested: 2_147_483_648
                }
            ),
            "{name}: unexpected refusal {err:?}"
        );
    }
    // Raising the caller's own limit raises the ceiling with it.
    assert_eq!(
        decompress_with_limit(&absurd, 3 << 30).expect("limit above the declaration"),
        payload
    );
}

/// The other half of the bounded contract: a declared `Frame_Content_Size`
/// past the limit is refused before a byte is decoded.
#[test]
fn a_declared_content_size_past_the_limit_is_refused_pre_decode() {
    let data = vec![b'A'; 1 << 20];
    let encoded = compress_with_level(&data, 3).expect("compress");

    let err = decompress_with_limit(&encoded, 4096).expect_err("declared size exceeds the limit");
    assert!(
        matches!(
            err,
            OxiArcError::MemoryBudgetExceeded {
                budget: 4096,
                requested: 1_048_576
            }
        ),
        "unexpected refusal: {err:?}"
    );
    assert!(decompress_multi_frame_with_limit(&encoded, 4096).is_err());

    // The unbounded legacy path has no limit to exceed.
    assert_eq!(decompress(&encoded).expect("legacy"), data);
    assert_eq!(
        decompress_with_limit(&encoded, 1 << 20).expect("exact limit"),
        data
    );
}

// ---------------------------------------------------------------------------
// Finding 4 — frame-boundary and EOF classification
// ---------------------------------------------------------------------------

#[test]
fn leading_garbage_is_an_error_on_both_paths() {
    let cases: [(&str, Vec<u8>, &str); 6] = [
        // crash-32276b1f: a single stray byte, fewer than a magic.
        (
            "crash-32276b1f",
            vec![0x8b],
            "truncated Zstandard frame magic",
        ),
        (
            "three stray bytes",
            vec![0x01, 0x02, 0x03],
            "truncated Zstandard frame magic",
        ),
        // crash-b3afddf7: four bytes that are no known magic.
        (
            "crash-b3afddf7",
            vec![0x03, 0x03, 0x00, 0x40],
            "Invalid magic",
        ),
        (
            "ascii garbage",
            b"this is definitely not zstd".to_vec(),
            "Invalid magic",
        ),
        // crash-636297ac: a skippable magic with no size field.
        (
            "crash-636297ac",
            vec![0x50, 0x2a, 0x4d, 0x18],
            "truncated skippable frame size",
        ),
        (
            "garbage before a valid frame",
            {
                let mut v = vec![0xDE, 0xAD, 0xBE, 0xEF];
                v.extend_from_slice(&compress_with_level(b"payload", 3).expect("compress"));
                v
            },
            "Invalid magic",
        ),
    ];
    for (what, bytes, expect) in cases {
        both_refuse(what, &bytes, expect);
    }
}

#[test]
fn a_truncated_skippable_frame_is_an_error_wherever_it_sits() {
    let mut leading = write_skippable_frame(b"metadata", 0);
    leading.truncate(leading.len() - 3);
    both_refuse(
        "truncated skippable payload, no frame yet",
        &leading,
        "truncated skippable frame payload",
    );

    let mut trailing = compress_with_level(b"payload", 3).expect("compress");
    let mut skippable = write_skippable_frame(b"metadata", 0);
    skippable.truncate(skippable.len() - 3);
    trailing.extend_from_slice(&skippable);
    both_refuse(
        "truncated skippable payload after a frame",
        &trailing,
        "truncated skippable frame payload",
    );

    // A skippable magic with no size field, after a complete frame, is the same
    // narrowing: a recognised frame start is never trailing garbage.
    let mut magic_only = compress_with_level(b"payload", 3).expect("compress");
    magic_only.extend_from_slice(&[0x50, 0x2A, 0x4D, 0x18]);
    both_refuse(
        "skippable magic only, after a frame",
        &magic_only,
        "truncated skippable frame size",
    );
}

#[test]
fn a_short_or_unknown_tail_after_a_frame_is_a_clean_end_on_both_paths() {
    let payload = b"payload";
    let base = compress_with_level(payload, 3).expect("compress");

    for tail in [
        &b""[..],
        &b"\x01"[..],
        &b"\x01\x02"[..],
        &b"\x01\x02\x03"[..],
        &b"NOT A ZSTD FRAME"[..],
    ] {
        let mut bytes = base.clone();
        bytes.extend_from_slice(tail);
        both_accept(&format!("tail {tail:?}"), &bytes, payload);
    }

    // A *truncated frame* after a complete one is still an error, though.
    let mut truncated = base.clone();
    truncated.extend_from_slice(&base[..base.len() - 2]);
    assert!(decompress_multi_frame(&truncated).is_err());
    assert!(via_stream(&truncated).is_err());
    assert!(decompress_multi_frame_with_limit(&truncated, 1 << 20).is_err());
}

#[test]
fn empty_input_is_empty_for_multi_frame_and_an_error_for_a_single_frame() {
    assert!(
        decompress_multi_frame(&[])
            .expect("legacy multi")
            .is_empty()
    );
    assert!(via_stream(&[]).expect("stream multi").is_empty());
    assert!(
        decompress_multi_frame_with_limit(&[], 1 << 20)
            .expect("bounded multi")
            .is_empty()
    );

    assert!(decompress(&[]).is_err());
    assert!(decompress_with_limit(&[], 1 << 20).is_err());
    assert!(stream_decode(&[], false, usize::MAX).is_err());
}

#[test]
fn skippable_frames_do_not_count_as_decoded_frames() {
    let skippable = write_skippable_frame(b"some metadata", 3);

    // A skippable frame alone is a clean, empty stream on both paths.
    both_accept("skippable only", &skippable, b"");

    // ... but it does not license a trailing short tail: nothing was decoded.
    let mut with_tail = skippable.clone();
    with_tail.extend_from_slice(&[0x01, 0x02]);
    both_refuse(
        "skippable then two stray bytes",
        &with_tail,
        "truncated Zstandard frame magic",
    );

    let mut with_garbage = skippable.clone();
    with_garbage.extend_from_slice(b"NOT A ZSTD FRAME");
    both_refuse("skippable then garbage", &with_garbage, "Invalid magic");

    // Interleavings that are entirely well formed still decode identically.
    let a = compress_with_level(b"first ", 3).expect("compress");
    let b = compress_with_level(b"second", 3).expect("compress");
    let mut joined = skippable.clone();
    joined.extend_from_slice(&a);
    joined.extend_from_slice(&skippable);
    joined.extend_from_slice(&b);
    joined.extend_from_slice(&skippable);
    both_accept("skippable interleaving", &joined, b"first second");
}

/// The artifact that first exposed the target's own reference-function bug:
/// two identical minimal frames, each regenerating one zero byte.
#[test]
fn fuzzer_two_frame_artifact_decodes_identically_on_both_paths() {
    const MINIMIZED: &[u8] = &[
        0x28, 0xb5, 0x2f, 0xfd, 0x00, 0x00, 0x0b, 0x00, 0x00, 0x00, 0x28, 0xb5, 0x2f, 0xfd, 0x00,
        0x00, 0x0b, 0x00, 0x00, 0x00,
    ];
    both_accept("minimized-from-decf534c", MINIMIZED, &[0x00, 0x00]);

    // The single-frame entry points see only the first frame, by contract.
    assert_eq!(decompress(MINIMIZED).expect("single"), &[0x00]);
    assert_eq!(
        stream_decode(MINIMIZED, false, usize::MAX).expect("stream single"),
        &[0x00]
    );
}

/// crash-8452fb44 — the artifact that exposed finding 3, which turns out to
/// carry *two* violations: an 11 MB `Window_Size` on its first frame, and a
/// second frame whose RLE block regenerates 49,163 bytes into a 1 KiB window.
///
/// Each path meets a different one first — `ZstdStream` the window ceiling,
/// the legacy path the block maximum it now enforces — so the artifact no
/// longer diverges: both refuse. The pure window split is pinned by
/// [`window_declaration_split`] instead.
#[test]
fn fuzzer_window_artifact_is_refused_on_both_paths() {
    const CRASH_8452: &[u8] = &[
        0x28, 0xb5, 0x2f, 0xfd, 0x00, 0x6b, 0xe3, 0xfd, 0x00, 0x00, 0x28, 0xb5, 0x2f, 0xfd, 0x00,
        0x00, 0x5b, 0x00, 0x06, 0x00, 0x8e, 0x31,
    ];
    let legacy = decompress_multi_frame(CRASH_8452).expect_err("legacy: block maximum");
    assert!(
        legacy
            .to_string()
            .contains("block regenerated size 49163 exceeds the frame maximum 1024"),
        "legacy refused for the wrong reason: {legacy}"
    );
    let stream = via_stream(CRASH_8452).expect_err("stream: window ceiling");
    assert!(
        matches!(
            stream,
            OxiArcError::MemoryBudgetExceeded {
                budget: 8_388_608,
                requested: 11_534_336
            }
        ),
        "stream refused for the wrong reason: {stream:?}"
    );
    // Lift the ceiling and `ZstdStream` lands on the very same block-maximum
    // violation the legacy path reports.
    let mut lifted = ZstdStream::new().with_max_window(16 << 20);
    let mut sink = vec![0u8; 1 << 16];
    let mut pos = 0usize;
    let err = loop {
        match lifted.decode(&CRASH_8452[pos..], &mut sink, FlushMode::Finish) {
            Ok(progress) => {
                pos += progress.consumed;
                assert_ne!(
                    progress.status,
                    ZstdStatus::StreamEnd,
                    "the second frame must not decode"
                );
            }
            Err(e) => break e,
        }
    };
    assert!(
        err.to_string()
            .contains("block regenerated size 49163 exceeds the frame maximum 1024"),
        "lifted-ceiling stream refused for the wrong reason: {err}"
    );
}

// ---------------------------------------------------------------------------
// The agreement contract itself, swept over mutated frames
// ---------------------------------------------------------------------------

/// Deterministic pseudo-random bytes (no `rand` dependency).
fn pseudo_random(len: usize, seed: u32) -> Vec<u8> {
    let mut x = seed | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            (x >> 24) as u8
        })
        .collect()
}

/// Frames spanning both header shapes, every block type, checksums, skippable
/// frames and concatenation.
fn agreement_corpus() -> Vec<(&'static str, Vec<u8>)> {
    let skippable = write_skippable_frame(b"metadata", 2);
    let text = b"the quick brown fox jumps over the lazy dog ".repeat(64);
    let mut multi = compress_with_level(b"first frame", 3).expect("compress");
    multi.extend_from_slice(&skippable);
    multi.extend_from_slice(&compress_with_level(b"second frame", 3).expect("compress"));

    vec![
        ("empty", compress_with_level(b"", 3).expect("compress")),
        ("one_byte", compress_with_level(b"a", 3).expect("compress")),
        ("text", compress_with_level(&text, 3).expect("compress")),
        (
            "incompressible",
            compress_with_level(&pseudo_random(3000, 0x1234), 9).expect("compress"),
        ),
        (
            "rle_like",
            compress_with_level(&vec![7u8; 4000], 1).expect("compress"),
        ),
        ("multi_frame", multi),
        ("windowed", raw_frame(0x48, &pseudo_random(2000, 0xBEEF))),
    ]
}

/// The property the differential fuzz target could not assert before this
/// track: with the format rules shared, the legacy one-shot decoder and
/// `ZstdStream` agree on **accept or refuse**, not merely on the bytes when
/// both accept.
///
/// The single permitted asymmetry is the documented declared-window split: a
/// mutation that inflates `Window_Size` past the streaming decoder's 8 MiB
/// default is refused there and accepted by the unbounded legacy path, always
/// as [`OxiArcError::MemoryBudgetExceeded`]. Anything else — a corrupted-data
/// or invalid-header refusal on one side only — is a divergence and fails.
#[test]
fn both_paths_agree_on_accept_or_refuse_under_mutation() {
    let mut cases = 0usize;
    let mut window_splits = 0usize;
    for (name, frame) in agreement_corpus() {
        // Every byte of the header region (where a single byte redefines the
        // window, the content size, the dictionary ID and the checksum flag),
        // then the body at ~41 sampled offsets.
        let step = (frame.len() / 41).max(1);
        let offsets: Vec<usize> = (0..frame.len().min(20))
            .chain((0..frame.len()).step_by(step))
            .collect();
        for offset in offsets {
            for value in [0x00u8, 0x01, 0x0F, 0x40, 0x7F, 0x80, 0xC0, 0xE0, 0xFF] {
                let mut mutated = frame.clone();
                mutated[offset] = value;
                cases += 1;

                let legacy = decompress_multi_frame(&mutated);
                let stream = via_stream(&mutated);
                match (&legacy, &stream) {
                    (Ok(expected), Ok(actual)) => assert_eq!(
                        expected, actual,
                        "[{name}] byte {offset}={value:#04x}: both accepted, bytes differ"
                    ),
                    (Ok(_), Err(e)) => {
                        assert!(
                            matches!(e, OxiArcError::MemoryBudgetExceeded { .. }),
                            "[{name}] byte {offset}={value:#04x}: legacy accepted, ZstdStream \
                             refused for a non-window reason: {e}"
                        );
                        window_splits += 1;
                    }
                    (Err(e), Ok(_)) => panic!(
                        "[{name}] byte {offset}={value:#04x}: ZstdStream accepted what the legacy \
                         path refused: {e}"
                    ),
                    (Err(_), Err(_)) => {}
                }
            }
        }
    }
    assert!(cases > 1_000, "only {cases} mutations were exercised");
    // The window split is real, not a vacuous allowance: mutations do reach it.
    assert!(
        window_splits > 0,
        "no mutation exercised the declared-window split"
    );
    eprintln!("[legacy_hardening] {cases} mutations, {window_splits} window-split cases");
}
