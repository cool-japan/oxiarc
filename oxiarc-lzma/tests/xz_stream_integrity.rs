//! Container-level integrity tests for [`oxiarc_lzma::xz`].
//!
//! These pin behaviour that the round-trip and oracle suites cannot reach,
//! because they need a `.xz` *file* that is not simply "one stream this
//! crate just produced":
//!
//! 1. **Multi-stream files.** A `.xz` file is one or more Streams with
//!    optional Stream Padding (xz spec §2.2) — what `cat a.xz b.xz` and
//!    every parallel compressor produce. Decoding only the first stream is
//!    a silent short read, which is the defect class this repository
//!    already treated as a must-fix for multi-stream `.bz2` and `.gz`.
//!    The exact acceptance rule is pinned against `xz 5.8.3`, which was
//!    measured to accept 4- and 8-byte trailing NUL padding and to reject
//!    1-, 2- and 3-byte padding with "Compressed data is corrupt".
//! 2. **Header fields that are parsed and could be ignored.** libtiff
//!    writes `Compression = 34925` strips with `LZMA_CHECK_NONE`, so on the
//!    TIFF path the block's Uncompressed Size field and the index record
//!    count are the *only* cross-checks the format offers. Each one is
//!    corrupted here (with the covering CRC-32 repaired, so the corruption
//!    is not caught by accident) and must be reported.
//! 3. **The `_into` error contract.** `decompress_into` documents
//!    [`OxiArcError::BufferTooSmall`] for a stream larger than `dst`; the
//!    variant itself is asserted, not just "some error".

use oxiarc_core::crc::Crc32;
use oxiarc_core::error::OxiArcError;
use oxiarc_lzma::LzmaLevel;
use oxiarc_lzma::xz::{self, CheckType, XzWriter};

fn stream_of(payload: &[u8], check: CheckType) -> Vec<u8> {
    XzWriter::new(LzmaLevel::new(6))
        .with_check_type(check)
        .compress(payload)
        .expect("compress")
}

// ---------------------------------------------------------------------------
// 1. Multi-stream files
// ---------------------------------------------------------------------------

#[test]
fn concatenated_streams_all_decode() {
    let parts: [&[u8]; 3] = [
        b"first-stream-payload",
        b"second stream, a different length",
        b"third",
    ];
    let checks = [CheckType::Crc32, CheckType::None, CheckType::Sha256];

    let mut file = Vec::new();
    let mut expected = Vec::new();
    for (part, check) in parts.iter().zip(checks) {
        file.extend_from_slice(&stream_of(part, check));
        expected.extend_from_slice(part);
    }

    let decoded = xz::decompress(&mut std::io::Cursor::new(&file)).expect("multi-stream decode");
    assert_eq!(decoded, expected, "every stream must be decoded");

    // The bounded entry points see the whole file too.
    let mut dst = vec![0u8; expected.len()];
    assert_eq!(
        xz::decompress_into(&file, &mut dst).expect("multi-stream into"),
        expected.len()
    );
    assert_eq!(dst, expected);
    assert_eq!(
        xz::decompress_with_limit(&file, expected.len()).expect("multi-stream limit"),
        expected
    );
}

#[test]
fn stream_padding_between_and_after_streams_is_accepted() {
    let a = stream_of(b"padded-A", CheckType::Crc64);
    let b = stream_of(b"padded-B", CheckType::Crc32);

    for pad in [4usize, 8, 16] {
        let mut file = a.clone();
        file.extend(std::iter::repeat_n(0u8, pad));
        file.extend_from_slice(&b);
        file.extend(std::iter::repeat_n(0u8, pad));
        let decoded =
            xz::decompress(&mut std::io::Cursor::new(&file)).expect("padded multi-stream decode");
        assert_eq!(decoded, b"padded-Apadded-B", "pad {pad}");
    }
}

#[test]
fn trailing_bytes_that_are_not_padding_are_rejected() {
    let a = stream_of(b"only-stream", CheckType::Crc32);

    // Non-null trailing bytes: `xz -d` reports an error, and so must we —
    // silently returning the first stream's bytes would be a short read.
    let mut garbage = a.clone();
    garbage.extend_from_slice(b"NOT-XZ-AT-ALL");
    assert!(
        xz::decompress(&mut std::io::Cursor::new(&garbage)).is_err(),
        "trailing garbage must not decode as a clean short read"
    );

    // Stream Padding must be a multiple of four bytes (`xz 5.8.3` rejects
    // 1..=3 trailing null bytes with "Compressed data is corrupt").
    for pad in 1usize..4 {
        let mut file = a.clone();
        file.extend(std::iter::repeat_n(0u8, pad));
        assert!(
            xz::decompress(&mut std::io::Cursor::new(&file)).is_err(),
            "{pad}-byte trailing padding must be rejected"
        );
    }

    // A second stream header that is truncated must be an error, not a
    // silent success on the first stream alone.
    let mut truncated_second = a.clone();
    truncated_second.extend_from_slice(&stream_of(b"second", CheckType::Crc32)[..8]);
    assert!(
        xz::decompress(&mut std::io::Cursor::new(&truncated_second)).is_err(),
        "a truncated second stream must be reported"
    );
}

// ---------------------------------------------------------------------------
// 2. Block-header fields that must not be ignored
// ---------------------------------------------------------------------------

/// Layout of the single block header `XzWriter` emits, verified in-place so
/// that a writer change breaks this test loudly instead of silently making
/// it vacuous. Returns `(header_start, header_len)`.
fn block_header_span(stream: &[u8], payload_len: u8) -> (usize, usize) {
    let start = 12; // stream header
    let size_byte = stream[start];
    let len = (size_byte as usize + 1) * 4;
    assert_eq!(stream[start + 1], 0xC0, "flags: 1 filter + both sizes");
    assert_eq!(
        stream[start + 3],
        payload_len,
        "uncompressed size varint (payload is < 128 bytes, so one byte)"
    );
    assert_eq!(stream[start + 4], 0x21, "LZMA2 filter id");
    (start, len)
}

/// Repair the block header's trailing CRC-32 after patching a field.
fn repair_block_header_crc(stream: &mut [u8], start: usize, len: usize) {
    let crc = Crc32::compute(&stream[start..start + len - 4]);
    stream[start + len - 4..start + len].copy_from_slice(&crc.to_le_bytes());
}

#[test]
fn block_uncompressed_size_mismatch_is_reported() {
    let payload = b"the quick brown fox jumps over the lazy dog";
    // `CheckType::None` is what libtiff writes: nothing else can catch this.
    let mut stream = stream_of(payload, CheckType::None);
    let (start, len) = block_header_span(&stream, payload.len() as u8);

    // Understate the uncompressed size by one and repair the header CRC.
    stream[start + 3] = payload.len() as u8 - 1;
    repair_block_header_crc(&mut stream, start, len);

    let err = xz::decompress(&mut std::io::Cursor::new(&stream))
        .expect_err("a lying Uncompressed Size field must be reported");
    let text = err.to_string();
    assert!(
        text.contains("uncompressed size"),
        "unexpected error: {text}"
    );
}

#[test]
fn block_header_reserved_flag_bits_are_rejected() {
    let payload = b"reserved-bits";
    let mut stream = stream_of(payload, CheckType::Crc32);
    let (start, len) = block_header_span(&stream, payload.len() as u8);

    // Bits 2-5 of the block flags are reserved and must be zero.
    stream[start + 1] = 0xC4;
    repair_block_header_crc(&mut stream, start, len);

    let err = xz::decompress(&mut std::io::Cursor::new(&stream))
        .expect_err("reserved block-header flag bits must be rejected");
    assert!(
        err.to_string().contains("reserved"),
        "unexpected error: {err}"
    );
}

#[test]
fn index_record_count_must_match_the_block_count() {
    let payload = b"index-record-count";
    let mut stream = stream_of(payload, CheckType::None);

    // Index layout: [0x00 indicator][num_records][record...][padding][crc32]
    // immediately before the 12-byte stream footer.
    let footer_start = stream.len() - 12;
    let backward_size = u32::from_le_bytes([
        stream[footer_start + 4],
        stream[footer_start + 5],
        stream[footer_start + 6],
        stream[footer_start + 7],
    ]);
    let index_len = (backward_size as usize + 1) * 4;
    let index_start = footer_start - index_len;
    assert_eq!(stream[index_start], 0x00, "index indicator");
    assert_eq!(stream[index_start + 1], 0x01, "one block was written");

    // Claim two records for a one-block stream, and repair the index CRC so
    // the mismatch is what is being tested rather than the checksum.
    stream[index_start + 1] = 0x02;
    let crc = Crc32::compute(&stream[index_start..footer_start - 4]);
    stream[footer_start - 4..footer_start].copy_from_slice(&crc.to_le_bytes());

    let err = xz::decompress(&mut std::io::Cursor::new(&stream))
        .expect_err("an index that disagrees with the block count must be reported");
    assert!(
        err.to_string().contains("record"),
        "unexpected error: {err}"
    );
}

// ---------------------------------------------------------------------------
// 3. The `_into` error contract
// ---------------------------------------------------------------------------

#[test]
fn decompress_into_reports_buffer_too_small() {
    let payload = vec![b'Z'; 40_000];
    let stream = stream_of(&payload, CheckType::Crc32);

    let mut dst = vec![0u8; payload.len() - 1];
    match xz::decompress_into(&stream, &mut dst) {
        Err(OxiArcError::BufferTooSmall { needed, available }) => {
            assert_eq!(available, payload.len() - 1);
            assert!(
                needed > available,
                "needed ({needed}) must exceed available ({available})"
            );
        }
        other => panic!("expected BufferTooSmall, got {other:?}"),
    }

    // An exactly-sized buffer still succeeds (the cap is inclusive).
    let mut exact = vec![0u8; payload.len()];
    assert_eq!(
        xz::decompress_into(&stream, &mut exact).expect("exact fit"),
        payload.len()
    );
    assert_eq!(exact, payload);
}

// ---------------------------------------------------------------------------
// Reference-tool grounding for the multi-stream rule
// ---------------------------------------------------------------------------

/// Differential check against the real `xz` CLI: a concatenation of
/// `xz`-produced streams (with and without Stream Padding) must decode to
/// the concatenation of the inputs, and 1..=3 bytes of trailing padding must
/// be rejected the way `xz -d` rejects them.
///
/// Self-skips when `xz` is absent, matching the crate's other oracle tests.
#[cfg(feature = "xz-oracle")]
mod oracle {
    use super::*;
    use std::process::Command;
    use std::sync::atomic::{AtomicU64, Ordering};

    static DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn unique_temp_dir(label: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "oxiarc_xz_stream_integrity_{label}_{}_{}",
            std::process::id(),
            DIR_COUNTER.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn tool_available(tool: &str, args: &[&str]) -> bool {
        Command::new(tool)
            .args(args)
            .output()
            .map(|out| out.status.success())
            .unwrap_or(false)
    }

    fn xz_compress(dir: &std::path::Path, name: &str, data: &[u8], args: &[&str]) -> Vec<u8> {
        let raw = dir.join(name);
        std::fs::write(&raw, data).expect("write raw");
        let out = Command::new("xz")
            .args(args)
            .arg("-c")
            .arg(&raw)
            .output()
            .expect("run xz");
        assert!(out.status.success(), "xz failed: {:?}", out.status);
        out.stdout
    }

    #[test]
    fn xz_cli_concatenated_streams_decode_like_xz_d() {
        if !tool_available("xz", &["--version"]) {
            eprintln!("[xz-oracle] `xz` not on PATH; skipping (self-skip, not a failure)");
            return;
        }
        let dir = unique_temp_dir("concat");

        let a = b"stream A: the quick brown fox jumps over the lazy dog\n".repeat(40);
        let b = b"stream B: a completely different payload, compressed apart\n".repeat(7);
        let c = vec![0x5Au8; 3000];

        let sa = xz_compress(&dir, "a", &a, &["-6", "--check=crc64"]);
        let sb = xz_compress(&dir, "b", &b, &["-1", "--check=none"]);
        let sc = xz_compress(&dir, "c", &c, &["-9", "--check=sha256"]);

        let mut expected = Vec::new();
        expected.extend_from_slice(&a);
        expected.extend_from_slice(&b);
        expected.extend_from_slice(&c);

        for pad in [0usize, 4, 8] {
            let mut file = Vec::new();
            for stream in [&sa, &sb, &sc] {
                file.extend_from_slice(stream);
                file.extend(std::iter::repeat_n(0u8, pad));
            }

            // `xz -d` accepts this exact file; assert that first so the
            // expectation is grounded in the reference tool, not in memory.
            let path = dir.join(format!("cat_{pad}.xz"));
            std::fs::write(&path, &file).expect("write concatenation");
            let reference = Command::new("xz")
                .arg("-dc")
                .arg(&path)
                .output()
                .expect("run xz -dc");
            assert!(
                reference.status.success(),
                "the reference tool rejected the fixture (pad {pad})"
            );
            assert_eq!(reference.stdout, expected, "reference output (pad {pad})");

            let decoded =
                xz::decompress(&mut std::io::Cursor::new(&file)).expect("oxiarc multi-stream");
            assert_eq!(decoded, expected, "oxiarc output (pad {pad})");
        }

        // 1..=3 trailing null bytes: the reference tool rejects these, and
        // so must we.
        for pad in 1usize..4 {
            let mut file = sa.clone();
            file.extend(std::iter::repeat_n(0u8, pad));
            let path = dir.join(format!("shortpad_{pad}.xz"));
            std::fs::write(&path, &file).expect("write short padding");
            let reference = Command::new("xz")
                .arg("-dc")
                .arg(&path)
                .output()
                .expect("run xz -dc");
            assert!(
                !reference.status.success(),
                "the reference tool must reject {pad}-byte padding"
            );
            assert!(
                xz::decompress(&mut std::io::Cursor::new(&file)).is_err(),
                "oxiarc must reject {pad}-byte padding"
            );
        }

        let _ = std::fs::remove_dir_all(&dir);
    }
}

// ---------------------------------------------------------------------------
// A real zero-block stream
// ---------------------------------------------------------------------------

/// `xz 5.8.3` compressing an empty input, byte for byte (32 bytes: header,
/// an index with **zero** records, footer — no block at all).
///
/// Committed because the exact bytes *are* the test: a stream with no
/// blocks is the one case where the new "index record count must equal the
/// block count" cross-check is compared against zero, and this crate's own
/// writer cannot produce it (`XzWriter` always emits one block, even for an
/// empty payload).
const XZ_CLI_EMPTY_STREAM: [u8; 32] = [
    0xFD, 0x37, 0x7A, 0x58, 0x5A, 0x00, 0x00, 0x04, 0xE6, 0xD6, 0xB4, 0x46, 0x00, 0x00, 0x00, 0x00,
    0x1C, 0xDF, 0x44, 0x21, 0x1F, 0xB6, 0xF3, 0x7D, 0x01, 0x00, 0x00, 0x00, 0x00, 0x04, 0x59, 0x5A,
];

#[test]
fn a_stream_with_no_blocks_decodes_to_nothing() {
    let decoded = xz::decompress(&mut std::io::Cursor::new(&XZ_CLI_EMPTY_STREAM[..]))
        .expect("a zero-block stream is valid");
    assert!(decoded.is_empty());

    let mut dst = [0u8; 4];
    assert_eq!(
        xz::decompress_into(&XZ_CLI_EMPTY_STREAM, &mut dst).expect("into"),
        0
    );
    assert!(
        xz::decompress_with_limit(&XZ_CLI_EMPTY_STREAM, 0)
            .expect("a zero cap is enough for zero bytes")
            .is_empty()
    );

    // Empty streams are also legal *between* streams carrying data.
    let mut file = XZ_CLI_EMPTY_STREAM.to_vec();
    file.extend_from_slice(&stream_of(b"payload", CheckType::Crc32));
    file.extend_from_slice(&XZ_CLI_EMPTY_STREAM);
    assert_eq!(
        xz::decompress(&mut std::io::Cursor::new(&file)).expect("mixed empty/non-empty"),
        b"payload"
    );
}
