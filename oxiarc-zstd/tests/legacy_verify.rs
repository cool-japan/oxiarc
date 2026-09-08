//! Adversarial verification of the legacy/streaming hardening.
//!
//! `tests/legacy_hardening.rs` pins the four divergences differential fuzzing
//! found between the legacy one-shot decoders and [`ZstdStream`]. This suite is
//! the independent pass over that work: it hunts for divergences the hand-
//! written cases could miss, and for places where the shared rules disagree
//! with the reference decoder.
//!
//! What it pins:
//!
//! 1. **Skippable frames in front of a frame are metadata, on every entry
//!    point.** `zstd -d` decodes `[skippable][frame]` exactly like `[frame]`,
//!    and so do [`ZstdStream`] and the bounded helpers. The legacy one-shot
//!    decoders used to refuse it with `InvalidMagic` — the one accept/refuse
//!    divergence left after the hardening track, found by the sweep below.
//! 2. **A crafted-frame matrix**: every one of the 256 `Window_Descriptor`
//!    bytes, `Raw` and `Rle` blocks straddling
//!    `Block_Maximum_Decompressed_Size`, declared content sizes that agree and
//!    disagree, multi-block frames and single-segment frames — the legacy path
//!    and a window-unrestricted [`ZstdStream`] must reach the *same* verdict,
//!    with no carve-out at all.
//! 3. **A truncation / mutation / splice sweep** over reference-encoded and
//!    hand-built seeds, asserting accept/refuse agreement (the declared-window
//!    split, [`OxiArcError::MemoryBudgetExceeded`], being the one documented
//!    exception), byte equality when both accept, and no panic, hang or
//!    schedule-dependent verdict.
//! 4. **Where this crate is deliberately stricter than `zstd -d`** (oracle
//!    feature): RFC 8878 §3.1.1.2.3 caps a block's regenerated size at
//!    `min(Window_Size, 128 KiB)`; the reference decoder lets a frame that also
//!    declares a larger `Frame_Content_Size` past that cap. No encoder emits
//!    such a frame — `--zstd=wlog=10` frames with a far larger content size
//!    still decode here, test (5) — so this crate keeps the RFC rule.

use oxiarc_core::error::OxiArcError;
use oxiarc_core::traits::FlushMode;
use oxiarc_zstd::{
    ZstdDecoder, ZstdStatus, ZstdStream, compress_with_level, decompress, decompress_frame,
    decompress_into, decompress_multi_frame, decompress_multi_frame_with_dict,
    decompress_multi_frame_with_limit, decompress_with_dict, decompress_with_limit,
    write_skippable_frame,
};

const ZSTD_MAGIC: [u8; 4] = [0x28, 0xB5, 0x2F, 0xFD];

/// Drive `ZstdStream` over `data` in `chunk`-byte pieces.
fn stream_decode(
    data: &[u8],
    multi_frame: bool,
    chunk: usize,
    max_window: usize,
) -> Result<Vec<u8>, OxiArcError> {
    let mut stream = ZstdStream::new()
        .with_multi_frame(multi_frame)
        .with_max_window(max_window);
    let mut out = Vec::new();
    let mut sink = [0u8; 61];
    let mut pos = 0usize;
    let mut guard = 0usize;
    loop {
        guard += 1;
        assert!(guard < 4_000_000, "no progress after {guard} decode calls");
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

/// Run `f`, turning a panic into `Err(message)` so a sweep can report every
/// input rather than dying on the first one.
fn catch<T>(
    f: impl FnOnce() -> Result<T, OxiArcError> + std::panic::UnwindSafe,
) -> Result<Result<T, OxiArcError>, String> {
    match std::panic::catch_unwind(f) {
        Ok(v) => Ok(v),
        Err(p) => Err(p
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| p.downcast_ref::<&str>().map(|s| (*s).to_string()))
            .unwrap_or_else(|| "<non-string panic>".to_string())),
    }
}

/// Record a finding, keeping at most `CAP` of them so a sweep that goes wrong
/// wholesale still prints something readable.
fn record(findings: &mut Vec<String>, message: String) {
    const CAP: usize = 12;
    if findings.len() < CAP {
        findings.push(message);
    }
}

/// A block header: `Last_Block`, `Block_Type` and `Block_Size`.
fn block_header(last: bool, kind: u32, size: u32) -> [u8; 3] {
    let raw = u32::from(last) | (kind << 1) | (size << 3);
    let b = raw.to_le_bytes();
    [b[0], b[1], b[2]]
}

/// `Window_Size` for a `Window_Descriptor` byte (RFC 8878 §3.1.1.1.2).
fn window_of(wd: u8) -> u64 {
    let base = 1u64 << (10 + u32::from(wd >> 3));
    base + (base >> 3) * u64::from(wd & 0x07)
}

/// A one-block windowed frame, optionally carrying a 4-byte content size.
fn crafted_frame(wd: u8, kind: u32, block_size: usize, fcs: Option<u32>) -> Vec<u8> {
    let mut f = Vec::from(ZSTD_MAGIC);
    match fcs {
        Some(v) => {
            f.push(0x80);
            f.push(wd);
            f.extend_from_slice(&v.to_le_bytes());
        }
        None => {
            f.push(0x00);
            f.push(wd);
        }
    }
    f.extend_from_slice(&block_header(true, kind, block_size as u32));
    if kind == 0 {
        f.extend(std::iter::repeat_n(0x61u8, block_size));
    } else {
        f.push(0x62);
    }
    f
}

// ---------------------------------------------------------------------------
// 1. Skippable frames in front of a Zstandard frame
// ---------------------------------------------------------------------------

/// A skippable frame (RFC 8878 §3.1.2) in front of a Zstandard frame is
/// metadata: `zstd -d` walks past it, and so must every entry point here.
///
/// Before this was fixed the legacy one-shot decoders returned
/// `InvalidMagic` while `ZstdStream` — and therefore `decompress_with_limit`
/// and `decompress_into` — decoded the frame, the last accept/refuse
/// divergence between the two families.
#[test]
fn a_leading_skippable_frame_decodes_on_every_entry_point() {
    let frame = compress_with_level(b"payload behind metadata", 3).expect("compress");
    let meta = write_skippable_frame(b"metadata", 0);
    let mut data = meta.clone();
    data.extend_from_slice(&frame);
    let expected: &[u8] = b"payload behind metadata";

    assert_eq!(decompress(&data).expect("decompress"), expected);
    assert_eq!(
        ZstdDecoder::new()
            .decode_frame(&data)
            .expect("decode_frame"),
        expected
    );
    assert_eq!(
        decompress_multi_frame(&data).expect("multi frame"),
        expected
    );
    assert_eq!(
        decompress_with_limit(&data, 1 << 20).expect("bounded"),
        expected
    );
    assert_eq!(
        decompress_multi_frame_with_limit(&data, 1 << 20).expect("bounded multi"),
        expected
    );
    assert_eq!(
        stream_decode(&data, false, usize::MAX, 8 << 20).expect("stream single"),
        expected
    );
    assert_eq!(
        stream_decode(&data, true, 1, 8 << 20).expect("stream byte-wise"),
        expected
    );
    let mut dst = [0u8; 64];
    let n = decompress_into(&data, &mut dst).expect("decompress_into");
    assert_eq!(&dst[..n], expected);

    // `decompress_frame` reports the metadata it walked past, so a caller
    // walking a concatenated stream lands on the next frame.
    let (out, consumed) = decompress_frame(&data).expect("decompress_frame");
    assert_eq!(out, expected);
    assert_eq!(consumed, data.len());

    // Several of them in a row, and the dictionary entry points too.
    let mut stacked = meta.clone();
    stacked.extend_from_slice(&write_skippable_frame(b"more", 7));
    stacked.extend_from_slice(&frame);
    assert_eq!(decompress(&stacked).expect("stacked"), expected);
    let dict = b"dictionary content".to_vec();
    let dict_frame = {
        let mut d = meta.clone();
        d.extend_from_slice(&frame);
        d
    };
    assert_eq!(
        decompress_with_dict(&dict_frame, &dict).expect("dict"),
        expected
    );
    assert_eq!(
        decompress_multi_frame_with_dict(&dict_frame, &dict).expect("dict multi"),
        expected
    );
}

/// A *truncated* skippable frame is still an error wherever it sits, on both
/// families: recognising a frame start and then running out of bytes is never
/// "trailing garbage".
#[test]
fn a_truncated_skippable_prefix_is_an_error_on_every_entry_point() {
    let frame = compress_with_level(b"payload", 3).expect("compress");

    // Size field cut short.
    let mut short_size = Vec::from(0x184D_2A50u32.to_le_bytes());
    short_size.extend_from_slice(&[0x00, 0x00]);
    short_size.extend_from_slice(&frame);
    for (what, verdict) in [
        ("decompress", decompress(&short_size).is_err()),
        ("decompress_frame", decompress_frame(&short_size).is_err()),
        ("multi_frame", decompress_multi_frame(&short_size).is_err()),
        (
            "with_limit",
            decompress_with_limit(&short_size, 1 << 20).is_err(),
        ),
        (
            "stream",
            stream_decode(&short_size, false, usize::MAX, 8 << 20).is_err(),
        ),
    ] {
        assert!(
            verdict,
            "[{what}] accepted a truncated skippable size field"
        );
    }

    // Payload cut short: the declared size runs past the end of the input.
    let mut short_payload = Vec::from(0x184D_2A50u32.to_le_bytes());
    short_payload.extend_from_slice(&1000u32.to_le_bytes());
    short_payload.extend_from_slice(b"only a few bytes");
    short_payload.extend_from_slice(&frame);
    for (what, verdict) in [
        ("decompress", decompress(&short_payload).is_err()),
        (
            "multi_frame",
            decompress_multi_frame(&short_payload).is_err(),
        ),
        (
            "with_limit",
            decompress_with_limit(&short_payload, 1 << 20).is_err(),
        ),
        (
            "stream",
            stream_decode(&short_payload, false, usize::MAX, 8 << 20).is_err(),
        ),
    ] {
        assert!(verdict, "[{what}] accepted a truncated skippable payload");
    }

    // Leading bytes that are *not* a recognised frame start stay an error.
    let mut garbage = b"NOT A ZSTD FRAME".to_vec();
    garbage.extend_from_slice(&frame);
    assert!(decompress(&garbage).is_err(), "leading garbage accepted");
    assert!(
        decompress_multi_frame(&garbage).is_err(),
        "leading garbage accepted by multi-frame"
    );
    assert!(
        stream_decode(&garbage, true, usize::MAX, 8 << 20).is_err(),
        "leading garbage accepted by the stream"
    );
}

/// A reused [`ZstdDecoder`] must not carry a failed frame's partial output into
/// the next call.
///
/// `decode_frame` used to keep whatever the previous, *failed* frame had
/// already produced: the next frame's output came back prefixed with it. When
/// that next frame declared a checksum the corruption surfaced as a bogus
/// `CrcMismatch`; when it declared neither a checksum nor a
/// `Frame_Content_Size` — a frame `zstd -3 --no-check` writes routinely — it
/// came back as `Ok`, 131 076 bytes where 4 were expected. Every call now
/// starts from a clean decoder, exactly as `ZstdStream::begin_frame` does.
#[test]
fn a_reused_decoder_does_not_carry_a_failed_frame_into_the_next_one() {
    // A multi-block frame, so a cut in the middle leaves real output behind.
    let payload: Vec<u8> = (0..400_000u32)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 24) as u8)
        .collect();
    let frame = compress_with_level(&payload, 1).expect("compress");
    assert!(
        frame.len() > 128 * 1024 / 4,
        "frame too small to be multi-block"
    );

    // The following frame carries neither a checksum nor a content size, so
    // nothing but a clean decoder can catch carried-over bytes.
    let plain = crafted_frame(0x00, 0, 4, None);
    assert_eq!(decompress(&plain).expect("plain frame"), b"aaaa");

    // And one that does carry a checksum, where the symptom used to be a
    // bogus `CrcMismatch` rather than silence.
    let checked = compress_with_level(b"clean payload", 3).expect("compress");

    for cut in [frame.len() / 2, frame.len() * 3 / 4, frame.len() - 5] {
        let mut decoder = ZstdDecoder::new();
        assert!(
            decoder.decode_frame(&frame[..cut]).is_err(),
            "a truncated frame must fail (cut={cut})"
        );
        assert_eq!(
            decoder.decode_frame(&plain).expect("after a failed frame"),
            b"aaaa",
            "partial output survived a failed decode (cut={cut})"
        );
        assert_eq!(
            decoder.decode_frame(&checked).expect("checksummed frame"),
            b"clean payload",
            "partial output survived a failed decode (cut={cut})"
        );
    }

    // The same decoder used across two *successful* frames stays correct.
    let mut decoder = ZstdDecoder::new();
    assert_eq!(
        decoder.decode_frame(&checked).expect("first"),
        b"clean payload"
    );
    assert_eq!(decoder.decode_frame(&plain).expect("second"), b"aaaa");
    assert_eq!(
        decoder.decode_frame(&checked).expect("third"),
        b"clean payload"
    );

    // A dictionary survives the per-call reset (only per-frame state is cleared).
    let dict = b"a dictionary that must outlive the reset".to_vec();
    let mut decoder = ZstdDecoder::new();
    decoder.set_dictionary(&dict);
    assert!(decoder.decode_frame(&frame[..64]).is_err());
    assert_eq!(decoder.decode_frame(&plain).expect("dict kept"), b"aaaa");
}

// ---------------------------------------------------------------------------
// 2. Crafted-frame matrix: exact agreement, no carve-out
// ---------------------------------------------------------------------------

/// Every `Window_Descriptor`, both header-declared block kinds, block sizes
/// straddling `Block_Maximum_Decompressed_Size`, and content sizes that agree
/// and disagree with what the block produces.
///
/// The comparison runs against a `ZstdStream` whose window ceiling is removed,
/// so the documented declared-window split cannot mask a disagreement: this
/// asserts *exact* accept/refuse agreement over 8000+ frames.
#[test]
fn crafted_frame_matrix_agrees_across_paths() {
    let mut mismatches: Vec<String> = Vec::new();
    let mut checked = 0usize;
    let mut accepted = 0usize;

    let mut compare = |what: String, frame: &[u8]| {
        let owned = frame.to_vec();
        let legacy = catch(move || decompress_multi_frame(&owned));
        let owned = frame.to_vec();
        let stream = catch(move || stream_decode(&owned, true, usize::MAX, usize::MAX));
        checked += 1;
        match (&legacy, &stream) {
            (Ok(Ok(a)), Ok(Ok(b))) => {
                accepted += 1;
                if a != b {
                    record(
                        &mut mismatches,
                        format!("[{what}] bytes differ: {} vs {}", a.len(), b.len()),
                    );
                }
            }
            (Ok(Ok(_)), Ok(Err(e))) => {
                record(
                    &mut mismatches,
                    format!("[{what}] legacy accepted, stream refused: {e}"),
                );
            }
            (Ok(Err(e)), Ok(Ok(_))) => {
                record(
                    &mut mismatches,
                    format!("[{what}] legacy refused ({e}), stream accepted"),
                );
            }
            (Err(p), _) => {
                record(&mut mismatches, format!("[{what}] legacy panicked: {p}"));
            }
            (_, Err(p)) => {
                record(&mut mismatches, format!("[{what}] stream panicked: {p}"));
            }
            _ => {}
        }
    };

    for wd in 0u16..=255 {
        let wd = wd as u8;
        let ceiling = window_of(wd).min(128 * 1024) as usize;
        let mut sizes = vec![
            0usize,
            1,
            ceiling.saturating_sub(1),
            ceiling,
            ceiling + 1,
            128 * 1024,
            128 * 1024 + 1,
        ];
        sizes.sort_unstable();
        sizes.dedup();
        for size in sizes {
            if size > (1 << 21) - 1 {
                continue;
            }
            for kind in [0u32, 1] {
                for fcs in [None, Some(size as u32), Some(size as u32 + 1)] {
                    let frame = crafted_frame(wd, kind, size, fcs);
                    compare(
                        format!("wd={wd:#04x} kind={kind} size={size} fcs={fcs:?}"),
                        &frame,
                    );
                }
            }
        }
    }

    // Two-block frames: the ceiling is per block, not per frame.
    for wd in [0x00u8, 0x10, 0x40, 0x6b, 0x80, 0xA8, 0xF8] {
        let ceiling = window_of(wd).min(128 * 1024) as usize;
        for (a, b) in [
            (ceiling, ceiling),
            (ceiling, ceiling + 1),
            (ceiling + 1, 1),
            (1, ceiling),
            (0, 0),
        ] {
            if a > (1 << 21) - 1 || b > (1 << 21) - 1 {
                continue;
            }
            let mut f = Vec::from(ZSTD_MAGIC);
            f.push(0x00);
            f.push(wd);
            f.extend_from_slice(&block_header(false, 0, a as u32));
            f.extend(std::iter::repeat_n(0x61u8, a));
            f.extend_from_slice(&block_header(true, 1, b as u32));
            f.push(0x62);
            compare(format!("multiblock wd={wd:#04x} a={a} b={b}"), &f);
        }
    }

    // Single-segment frames: `Window_Size` *is* `Frame_Content_Size`.
    for content in [
        0u32, 1, 2, 255, 256, 257, 1023, 1024, 65535, 131071, 131072, 131073,
    ] {
        for block in [0u32, 1, content, content + 1, 131072] {
            if block > (1 << 21) - 1 {
                continue;
            }
            let mut f = Vec::from(ZSTD_MAGIC);
            f.push(0xA0);
            f.extend_from_slice(&content.to_le_bytes());
            f.extend_from_slice(&block_header(true, 1, block));
            f.push(0x63);
            compare(
                format!("single-segment content={content} block={block}"),
                &f,
            );
        }
    }

    for m in &mismatches {
        eprintln!("[crafted] {m}");
    }
    assert!(
        mismatches.is_empty(),
        "{} of {checked} crafted frames disagreed across paths",
        mismatches.len()
    );
    assert!(
        checked > 8000 && accepted > 3000,
        "matrix shrank: checked={checked} accepted={accepted}"
    );
}

/// The RFC ceiling itself, pinned by hand so a reverted `charge_block` fails
/// loudly rather than merely agreeing with itself.
#[test]
fn the_block_ceiling_is_min_window_128k() {
    // Window_Descriptor 0x00 -> 1 KiB.
    assert_eq!(window_of(0x00), 1024);
    let ok = crafted_frame(0x00, 0, 1024, None);
    assert_eq!(decompress(&ok).expect("at the ceiling").len(), 1024);
    let over = crafted_frame(0x00, 0, 1025, None);
    let err = decompress(&over).expect_err("over the ceiling").to_string();
    assert!(
        err.contains("block regenerated size 1025 exceeds the frame maximum 1024"),
        "unexpected error: {err}"
    );
    // An RLE block declares its regenerated size too, and must be refused from
    // the header rather than after expanding.
    let rle = crafted_frame(0x00, 1, 65535, None);
    assert!(
        decompress(&rle)
            .expect_err("rle over the ceiling")
            .to_string()
            .contains("exceeds the frame maximum 1024")
    );
    // Above 128 KiB the window stops mattering.
    let big = crafted_frame(0xF8, 0, 128 * 1024, None);
    assert_eq!(decompress(&big).expect("128 KiB block").len(), 128 * 1024);
    let too_big = crafted_frame(0xF8, 1, 128 * 1024 + 1, None);
    assert!(
        decompress(&too_big)
            .expect_err("over 128 KiB")
            .to_string()
            .contains("exceeds maximum")
    );
}

// ---------------------------------------------------------------------------
// 3. Truncation / mutation / splice sweep
// ---------------------------------------------------------------------------

/// Seeds for the sweep: encoder output at several levels, hand-built frames
/// and concatenations.
fn sweep_seeds() -> Vec<(String, Vec<u8>)> {
    let mut out: Vec<(String, Vec<u8>)> = Vec::new();
    let payloads: Vec<(&str, Vec<u8>)> = vec![
        ("empty", Vec::new()),
        ("tiny", b"a".to_vec()),
        (
            "text",
            b"hello hello hello world world world zstd zstd".to_vec(),
        ),
        ("rle", vec![7u8; 5000]),
        ("mixed", (0..4096u32).map(|i| (i % 251) as u8).collect()),
        // Large enough to need several blocks, Huffman literals and FSE
        // sequence tables, so the sweep reaches the compressed-block executor
        // and the block boundaries, not just frame headers.
        (
            "multiblock",
            (0..200_000u32)
                .map(|i| ((i * 31 + i / 7) % 241) as u8)
                .collect(),
        ),
    ];
    for (name, payload) in &payloads {
        for level in [1i32, 9] {
            if let Ok(f) = compress_with_level(payload, level) {
                out.push((format!("{name}-l{level}"), f));
            }
        }
    }
    if let (Ok(a), Ok(b)) = (
        compress_with_level(b"one ", 3),
        compress_with_level(b"two", 3),
    ) {
        let mut cat = a.clone();
        cat.extend_from_slice(&b);
        out.push(("two-frames".into(), cat));
        let mut with_skip = write_skippable_frame(b"meta", 0);
        with_skip.extend_from_slice(&a);
        with_skip.extend_from_slice(&write_skippable_frame(b"tail", 3));
        out.push(("skippable-around".into(), with_skip));
    }
    out.push(("raw-block".into(), crafted_frame(0x00, 0, 4, None)));
    out.push(("rle-block".into(), crafted_frame(0x00, 1, 300, None)));
    out
}

/// Truncations, single-byte mutations, splices and random garbage: the two
/// families must agree on accept/refuse, agree on the bytes when both accept,
/// never panic, never hang, and never depend on how the input was chunked.
#[test]
fn adversarial_truncation_and_mutation_agree_across_paths() {
    let mut findings: Vec<String> = Vec::new();
    let mut checked = 0usize;

    let mut check = |what: String, data: &[u8], findings: &mut Vec<String>| {
        checked += 1;
        let owned = data.to_vec();
        let legacy = match catch(move || decompress_multi_frame(&owned)) {
            Ok(v) => v,
            Err(p) => {
                record(findings, format!("[{what}] legacy panicked: {p}"));
                return;
            }
        };
        let owned = data.to_vec();
        let whole = match catch(move || stream_decode(&owned, true, usize::MAX, 8 << 20)) {
            Ok(v) => v,
            Err(p) => {
                record(findings, format!("[{what}] stream panicked: {p}"));
                return;
            }
        };
        let owned = data.to_vec();
        let byte_wise = match catch(move || stream_decode(&owned, true, 1, 8 << 20)) {
            Ok(v) => v,
            Err(p) => {
                record(findings, format!("[{what}] byte-wise stream panicked: {p}"));
                return;
            }
        };
        match (&whole, &byte_wise) {
            (Ok(a), Ok(b)) if a != b => {
                record(findings, format!("[{what}] schedule-dependent bytes"));
            }
            (Ok(_), Err(e)) => {
                record(findings, format!("[{what}] whole=Ok byte-wise=Err({e})"));
            }
            (Err(e), Ok(_)) => {
                record(findings, format!("[{what}] whole=Err({e}) byte-wise=Ok"));
            }
            _ => {}
        }
        // The single-frame family must agree with itself too.
        let owned = data.to_vec();
        let single = catch(move || decompress(&owned));
        let owned = data.to_vec();
        let single_bounded = catch(move || decompress_with_limit(&owned, 1 << 20));
        match (&single, &single_bounded) {
            (Err(p), _) | (_, Err(p)) => {
                record(findings, format!("[{what}] single-frame panic: {p}"));
                return;
            }
            (Ok(Err(e)), Ok(Ok(_))) => {
                record(
                    findings,
                    format!("[{what}] decompress refused ({e}) but decompress_with_limit accepted"),
                );
            }
            (Ok(Ok(a)), Ok(Ok(b))) if a != b => {
                record(findings, format!("[{what}] single-frame bytes differ"));
            }
            _ => {}
        }
        match (&legacy, &whole) {
            (Ok(a), Ok(b)) if a != b => {
                record(findings, format!("[{what}] multi-frame bytes differ"));
            }
            (Err(e), Ok(_)) => {
                record(
                    findings,
                    format!("[{what}] legacy refused ({e}) but the stream accepted"),
                );
            }
            (Ok(_), Err(e)) if !matches!(e, OxiArcError::MemoryBudgetExceeded { .. }) => {
                record(
                    findings,
                    format!("[{what}] legacy accepted but the stream refused: {e}"),
                );
            }
            _ => {}
        }
    };

    for (name, seed) in &sweep_seeds() {
        for cut in 0..=seed.len().min(300) {
            check(format!("{name}/trunc@{cut}"), &seed[..cut], &mut findings);
        }
        // Block boundaries live far past the header in a multi-block frame.
        let mut cut = 300usize;
        while cut < seed.len() {
            check(format!("{name}/trunc@{cut}"), &seed[..cut], &mut findings);
            cut += 997;
        }
        let mut mutate_at: Vec<usize> = (0..seed.len().min(120)).collect();
        let mut off = 120usize;
        while off < seed.len() {
            mutate_at.push(off);
            off += 1013;
        }
        for off in mutate_at {
            for value in [0x01u8, 0x28, 0x80, 0xff] {
                let mut m = seed.clone();
                m[off] ^= value;
                check(format!("{name}/mut@{off}^{value:02x}"), &m, &mut findings);
            }
        }
        for off in (0..seed.len().min(300)).step_by(5) {
            let mut d = seed.clone();
            d.remove(off);
            check(format!("{name}/del@{off}"), &d, &mut findings);
            let mut ins = seed.clone();
            ins.insert(off, 0xff);
            check(format!("{name}/ins@{off}"), &ins, &mut findings);
        }
        for extra in 1..=6usize {
            let mut t = seed.clone();
            t.extend(std::iter::repeat_n(0xabu8, extra));
            check(format!("{name}/tail{extra}"), &t, &mut findings);
            let mut lead: Vec<u8> = std::iter::repeat_n(0x99u8, extra).collect();
            lead.extend_from_slice(seed);
            check(format!("{name}/lead{extra}"), &lead, &mut findings);
        }
    }

    // Random garbage, with and without a Zstandard magic in front.
    let mut lcg = 0x1234_5678_9abc_def0u64;
    let mut next = move || {
        lcg = lcg
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (lcg >> 33) as u8
    };
    for n in 0..1500usize {
        let len = (n % 40) + 1;
        let mut buf: Vec<u8> = Vec::with_capacity(len + 4);
        if n % 2 == 0 {
            buf.extend_from_slice(&ZSTD_MAGIC);
        }
        for _ in 0..len {
            buf.push(next());
        }
        check(format!("garbage{n}"), &buf, &mut findings);
    }

    for f in &findings {
        eprintln!("[sweep] {f}");
    }
    assert!(
        findings.is_empty(),
        "{} findings over {checked} adversarial inputs",
        findings.len()
    );
    assert!(checked > 5500, "sweep shrank: only {checked} inputs");
}

// ---------------------------------------------------------------------------
// 4/5. Where this crate is deliberately stricter than the reference decoder
// ---------------------------------------------------------------------------

#[cfg(feature = "zstd-oracle")]
mod oracle {
    use super::*;
    use std::io::Write as _;

    /// Decode `frame` with the reference CLI; `None` means it refused.
    fn reference_decode(frame: &[u8]) -> Option<Vec<u8>> {
        let mut child = std::process::Command::new("zstd")
            .args(["-d", "-c"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .ok()?;
        let mut stdin = child.stdin.take()?;
        let owned = frame.to_vec();
        let feeder = std::thread::spawn(move || {
            let _ = stdin.write_all(&owned);
        });
        let out = child.wait_with_output().ok()?;
        let _ = feeder.join();
        if out.status.success() {
            Some(out.stdout)
        } else {
            None
        }
    }

    fn zstd_on_path() -> bool {
        std::process::Command::new("which")
            .arg("zstd")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// RFC 8878 §3.1.1.2.3 caps a block's regenerated size at
    /// `min(Window_Size, 128 KiB)`. The reference decoder lets a frame past
    /// that cap when it also declares a larger `Frame_Content_Size`; this
    /// crate keeps the RFC rule.
    ///
    /// Pinned here so the difference stays a *decision*: this crate must never
    /// be more lenient than the reference, and every case where it is stricter
    /// must be exactly this one.
    #[test]
    fn the_block_ceiling_is_stricter_than_the_reference_only_where_the_rfc_says_so() {
        if !zstd_on_path() {
            eprintln!("[zstd-oracle] `zstd` not on PATH; skipping (self-skip, not a failure)");
            return;
        }
        let mut problems: Vec<String> = Vec::new();
        let mut stricter = 0usize;
        let mut agreed = 0usize;
        for wd in [0x00u8, 0x08, 0x10, 0x28, 0x40, 0x6b, 0x78] {
            let ceiling = window_of(wd).min(128 * 1024) as usize;
            for size in [
                0usize,
                1,
                ceiling.saturating_sub(1),
                ceiling,
                ceiling + 1,
                ceiling * 2,
            ] {
                if size > (1 << 21) - 1 {
                    continue;
                }
                for kind in [0u32, 1] {
                    for fcs in [None, Some(size as u32)] {
                        let frame = crafted_frame(wd, kind, size, fcs);
                        let ours = decompress_multi_frame(&frame);
                        let theirs = reference_decode(&frame);
                        let label = format!("wd={wd:#04x} kind={kind} size={size} fcs={fcs:?}");
                        match (&ours, &theirs) {
                            (Ok(a), Some(b)) => {
                                agreed += 1;
                                if a != b {
                                    problems.push(format!("[{label}] bytes differ"));
                                }
                            }
                            (Ok(_), None) => problems.push(format!(
                                "[{label}] this crate accepted what the reference refused"
                            )),
                            (Err(_), Some(_)) => {
                                stricter += 1;
                                if size <= ceiling {
                                    problems.push(format!(
                                        "[{label}] refused a block within min(Window_Size, 128 KiB)"
                                    ));
                                }
                            }
                            (Err(_), None) => agreed += 1,
                        }
                    }
                }
            }
        }
        for p in &problems {
            eprintln!("[oracle] {p}");
        }
        assert!(
            problems.is_empty(),
            "{} reference disagreements",
            problems.len()
        );
        assert!(
            stricter > 0,
            "no frame in the matrix was refused here and accepted by `zstd -d`. \
             That is not a regression in this crate: it means the reference now \
             enforces the RFC block ceiling too, which is the better outcome. \
             Delete this assertion — the per-case `size <= ceiling` check above \
             is what actually protects the rule."
        );
        assert!(agreed > 40, "matrix shrank: agreed={agreed}");
    }

    /// The RFC ceiling must not reject frames a real encoder produces: a
    /// `--zstd=wlog=10` frame declares a 1 KiB window next to a
    /// `Frame_Content_Size` hundreds of times larger, and its blocks stay
    /// within `min(Window_Size, 128 KiB)`.
    #[test]
    fn reference_frames_with_a_tiny_declared_window_still_decode() {
        if !zstd_on_path() {
            eprintln!("[zstd-oracle] `zstd` not on PATH; skipping (self-skip, not a failure)");
            return;
        }
        let mut failures: Vec<String> = Vec::new();
        let mut checked = 0usize;
        for size in [1025usize, 4096, 20000, 200000, 500000] {
            let payload: Vec<u8> = (0..size).map(|i| ((i * 7 + 13) % 251) as u8).collect();
            let path = std::env::temp_dir().join(format!("oxiarc_zstd_verify_wlog_{size}.bin"));
            std::fs::write(&path, &payload).expect("write fixture");
            for args in [
                ["--zstd=wlog=10", "-1"],
                ["--zstd=wlog=10", "-19"],
                ["--zstd=wlog=11", "-6"],
                ["--zstd=wlog=10", "-3"],
            ] {
                let Ok(out) = std::process::Command::new("zstd")
                    .args(args)
                    .arg("-c")
                    .arg(&path)
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::null())
                    .output()
                else {
                    continue;
                };
                if !out.status.success() || out.stdout.is_empty() {
                    continue;
                }
                checked += 1;
                match decompress(&out.stdout) {
                    Ok(v) if v == payload => {}
                    Ok(v) => failures.push(format!("{size}/{args:?}: {} bytes", v.len())),
                    Err(e) => failures.push(format!("{size}/{args:?}: {e}")),
                }
                match stream_decode(&out.stdout, false, 4096, usize::MAX) {
                    Ok(v) if v == payload => {}
                    Ok(v) => failures.push(format!("stream {size}/{args:?}: {} bytes", v.len())),
                    Err(e) => failures.push(format!("stream {size}/{args:?}: {e}")),
                }
            }
            let _ = std::fs::remove_file(&path);
        }
        for f in &failures {
            eprintln!("[oracle] {f}");
        }
        assert!(
            failures.is_empty(),
            "{} small-window frames rejected",
            failures.len()
        );
        assert!(
            checked >= 16,
            "only {checked} reference frames were produced"
        );
    }
}
