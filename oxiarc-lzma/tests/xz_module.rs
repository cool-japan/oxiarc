//! Tests for the `.xz` container module that moved into `oxiarc-lzma`.
//!
//! Two groups:
//!
//! 1. **Always-on** — round-trips, every check type, multi-block streams,
//!    the two new bounded entry points ([`oxiarc_lzma::xz::decompress_into`]
//!    and [`oxiarc_lzma::xz::decompress_with_limit`]), a decompression-bomb
//!    budget test, and truncation/corruption sweeps that must never panic
//!    or silently truncate.
//! 2. **`xz-oracle`-gated** — real `xz` CLI output (including
//!    `LZMA_CHECK_NONE`, the check libtiff writes for TIFF
//!    `Compression = 34925`) decoded through the new functions, and libtiff
//!    `tiffcp -c lzma` strips extracted with a dependency-free `python3`
//!    IFD parser and decoded strip by strip. Both self-skip when the tool is
//!    absent.

use oxiarc_lzma::LzmaLevel;
use oxiarc_lzma::xz::{self, CheckType, XzReader, XzWriter};
use std::io::Cursor;

/// Deterministic xorshift payload (incompressible).
fn xorshift_bytes(seed: u64, len: usize) -> Vec<u8> {
    let mut state = seed | 1;
    let mut out = Vec::with_capacity(len + 8);
    while out.len() < len {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        out.extend_from_slice(&state.to_le_bytes());
    }
    out.truncate(len);
    out
}

/// Mixed payload: compressible text interleaved with noise.
fn mixed_bytes(len: usize) -> Vec<u8> {
    let text = b"OxiArc xz module test payload, moderately compressible. ";
    let mut out = Vec::with_capacity(len + 64);
    let mut seed = 0x5DEE_CE66_D125_u64;
    while out.len() < len {
        out.extend_from_slice(text);
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        out.extend_from_slice(&seed.to_le_bytes());
    }
    out.truncate(len);
    out
}

#[test]
fn round_trip_through_every_check_type() {
    let payloads: Vec<Vec<u8>> = vec![
        Vec::new(),
        b"x".to_vec(),
        b"Hello, XZ!".to_vec(),
        vec![b'A'; 100_000],
        mixed_bytes(300_000),
        xorshift_bytes(0xDEAD_BEEF, 64 * 1024),
    ];
    let checks = [
        CheckType::None,
        CheckType::Crc32,
        CheckType::Crc64,
        CheckType::Sha256,
    ];

    for payload in &payloads {
        for check in checks {
            let stream = XzWriter::new(LzmaLevel::new(6))
                .with_check_type(check)
                .compress(payload)
                .unwrap_or_else(|e| panic!("compress with {check:?}: {e}"));

            let decoded = xz::decompress(&mut Cursor::new(&stream))
                .unwrap_or_else(|e| panic!("decompress with {check:?}: {e}"));
            assert_eq!(&decoded, payload, "check {check:?}");

            let mut buffer = vec![0u8; payload.len()];
            let written = xz::decompress_into(&stream, &mut buffer)
                .unwrap_or_else(|e| panic!("decompress_into with {check:?}: {e}"));
            assert_eq!(written, payload.len(), "check {check:?}");
            assert_eq!(&buffer, payload, "check {check:?}");

            let capped = xz::decompress_with_limit(&stream, payload.len())
                .unwrap_or_else(|e| panic!("decompress_with_limit with {check:?}: {e}"));
            assert_eq!(&capped, payload, "check {check:?}");
        }
    }
}

#[test]
fn decompress_into_rejects_an_oversized_stream() {
    let payload = vec![b'Z'; 50_000];
    let stream = xz::compress(&payload, 6).expect("compress");

    // One byte short: must be an error, never a silent truncation.
    let mut small = vec![0u8; payload.len() - 1];
    let err = xz::decompress_into(&stream, &mut small).expect_err("must reject an undersized dst");
    let message = err.to_string().to_lowercase();
    assert!(
        message.contains("budget") || message.contains("buffer") || message.contains("small"),
        "unexpected error for an undersized buffer: {err}"
    );

    // A larger buffer is fine and reports the real length.
    let mut large = vec![0xEEu8; payload.len() + 1000];
    let written = xz::decompress_into(&stream, &mut large).expect("oversized dst");
    assert_eq!(written, payload.len());
    assert_eq!(&large[..written], &payload[..]);
    assert!(large[written..].iter().all(|&b| b == 0xEE));
}

#[test]
fn decompress_with_limit_bounds_a_decompression_bomb() {
    // 8 MiB of zeros compresses to a handful of bytes.
    let bomb_payload = vec![0u8; 8 * 1024 * 1024];
    let stream = xz::compress(&bomb_payload, 6).expect("compress bomb");
    assert!(
        stream.len() < 64 * 1024,
        "the bomb fixture must actually be small: {} bytes",
        stream.len()
    );

    // Under the cap: rejected.
    let err = xz::decompress_with_limit(&stream, 1024).expect_err("bomb must be rejected");
    assert!(
        err.to_string().to_lowercase().contains("budget"),
        "expected a budget error, got: {err}"
    );

    // At the cap: accepted.
    let decoded =
        xz::decompress_with_limit(&stream, bomb_payload.len()).expect("within budget must decode");
    assert_eq!(decoded.len(), bomb_payload.len());
}

#[test]
fn reader_with_max_output_is_enforced_during_decode() {
    let payload = vec![7u8; 4 * 1024 * 1024];
    let stream = xz::compress(&payload, 6).expect("compress");
    let mut reader = XzReader::new(Cursor::new(&stream))
        .expect("reader")
        .with_max_output(1024);
    assert!(
        reader.decompress().is_err(),
        "a stream past the cap must fail"
    );
}

#[test]
fn truncated_streams_error_without_panicking() {
    let payload = mixed_bytes(200_000);
    let stream = xz::compress(&payload, 6).expect("compress");

    let stride = (stream.len() / 400).max(1);
    for cut in (0..stream.len()).step_by(stride) {
        let truncated = &stream[..cut];
        // Every entry point must return, not panic, and never claim success
        // with a wrong-length result.
        if let Ok(decoded) = xz::decompress(&mut Cursor::new(truncated)) {
            assert_eq!(decoded, payload, "cut {cut} silently produced wrong data");
        }
        let mut buffer = vec![0u8; payload.len()];
        if let Ok(written) = xz::decompress_into(truncated, &mut buffer) {
            assert_eq!(written, payload.len(), "cut {cut} silently short");
            assert_eq!(&buffer, &payload, "cut {cut} silently produced wrong data");
        }
        if let Ok(decoded) = xz::decompress_with_limit(truncated, payload.len()) {
            assert_eq!(decoded, payload, "cut {cut} silently produced wrong data");
        }
    }
}

#[test]
fn corrupted_streams_error_without_panicking() {
    let payload = mixed_bytes(60_000);
    let stream = xz::compress(&payload, 6).expect("compress");

    let stride = (stream.len() / 200).max(1);
    for position in (0..stream.len()).step_by(stride) {
        for mask in [0x01u8, 0x20, 0x80, 0xFF] {
            let mut corrupted = stream.clone();
            corrupted[position] ^= mask;
            // Corruption must produce either the exact original (the flipped
            // bit landed somewhere semantically inert) or an error.
            if let Ok(decoded) = xz::decompress(&mut Cursor::new(&corrupted)) {
                assert_eq!(
                    decoded.len(),
                    payload.len(),
                    "corruption at {position} changed the length silently"
                );
            }
            let mut buffer = vec![0u8; payload.len()];
            let _ = xz::decompress_into(&corrupted, &mut buffer);
            let _ = xz::decompress_with_limit(&corrupted, payload.len());
        }
    }
}

#[test]
fn empty_and_degenerate_inputs() {
    // Nothing at all.
    assert!(xz::decompress(&mut Cursor::new(&[][..])).is_err());
    let mut out = [0u8; 8];
    assert!(xz::decompress_into(&[], &mut out).is_err());
    assert!(xz::decompress_with_limit(&[], 8).is_err());

    // The magic alone.
    let magic = [0xFDu8, 0x37, 0x7A, 0x58, 0x5A, 0x00];
    assert!(xz::decompress(&mut Cursor::new(&magic[..])).is_err());

    // A stream of zero bytes decodes to zero bytes, into a zero-length dst.
    let empty = xz::compress(&[], 6).expect("compress empty");
    assert_eq!(
        xz::decompress_into(&empty, &mut out[..0]).expect("empty into empty"),
        0
    );
    assert_eq!(xz::decompress_into(&empty, &mut out).expect("empty"), 0);
    assert_eq!(
        xz::decompress_with_limit(&empty, 0).expect("empty with a zero cap"),
        Vec::<u8>::new()
    );
}

#[test]
fn archive_and_lzma_paths_are_the_same_code() {
    // The archive crate re-exports this module; that is checked in
    // oxiarc-archive's own suites. Here, prove that both writer entry points
    // (`xz::compress` and `XzWriter`) agree, so the re-export cannot drift.
    let payload = mixed_bytes(9000);
    let via_fn = xz::compress(&payload, 6).expect("free fn");
    let via_writer = XzWriter::new(LzmaLevel::new(6))
        .compress(&payload)
        .expect("writer");
    assert_eq!(via_fn, via_writer);
}

// ---------------------------------------------------------------------------
// Oracle-gated: real `xz` CLI and libtiff `tiffcp -c lzma`
// ---------------------------------------------------------------------------

#[cfg(feature = "xz-oracle")]
mod oracle {
    use super::*;
    use std::path::PathBuf;
    use std::process::Command;
    use std::sync::atomic::{AtomicU64, Ordering};

    static DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn unique_temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "oxiarc_xz_module_{label}_{}_{}",
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

    /// `xz` CLI output at several presets and check types (including the
    /// `--check=none` that libtiff writes) must decode through the new
    /// bounded entry points.
    #[test]
    fn xz_cli_streams_decode_through_the_bounded_entry_points() {
        if !tool_available("xz", &["--version"]) {
            eprintln!("[xz-oracle] `xz` not on PATH; skipping (self-skip, not a failure)");
            return;
        }
        let dir = unique_temp_dir("cli");
        let payload = mixed_bytes(700_000);
        let raw = dir.join("payload.bin");
        std::fs::write(&raw, &payload).expect("write payload");

        let mut checked = 0usize;
        for check in ["none", "crc32", "crc64", "sha256"] {
            for preset in ["-1", "-6"] {
                let out = dir.join(format!("payload_{check}_{preset}.xz"));
                let status = Command::new("xz")
                    .args(["-T1", "-k", "-f", preset, &format!("--check={check}")])
                    .arg("-c")
                    .arg(&raw)
                    .output()
                    .expect("spawn xz");
                assert!(
                    status.status.success(),
                    "xz --check={check} {preset} failed: {}",
                    String::from_utf8_lossy(&status.stderr)
                );
                std::fs::write(&out, &status.stdout).expect("write xz output");

                let stream = status.stdout;
                let decoded = xz::decompress(&mut Cursor::new(&stream))
                    .unwrap_or_else(|e| panic!("[{check} {preset}] decompress: {e}"));
                assert_eq!(decoded, payload, "[{check} {preset}]");

                let mut buffer = vec![0u8; payload.len()];
                let written = xz::decompress_into(&stream, &mut buffer)
                    .unwrap_or_else(|e| panic!("[{check} {preset}] decompress_into: {e}"));
                assert_eq!(written, payload.len());
                assert_eq!(buffer, payload, "[{check} {preset}]");

                let capped = xz::decompress_with_limit(&stream, payload.len())
                    .unwrap_or_else(|e| panic!("[{check} {preset}] decompress_with_limit: {e}"));
                assert_eq!(capped, payload, "[{check} {preset}]");
                checked += 1;
            }
        }
        eprintln!("[xz-oracle] {checked} real `xz` streams decoded through xz::decompress_into");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Every BCJ / Delta filter the module implements, validated against
    /// the real `xz` CLI: `xz --<filter> --lzma2` output of a synthetic
    /// instruction stream must decode byte-identically. This is the only
    /// ground truth for the branch converters, so it is what proves the
    /// transforms are the *reference* transforms and not merely
    /// self-consistent.
    #[test]
    fn xz_cli_filter_chains_decode_byte_identical() {
        if !tool_available("xz", &["--version"]) {
            eprintln!("[xz-oracle] `xz` not on PATH; skipping (self-skip, not a failure)");
            return;
        }
        let dir = unique_temp_dir("filters");

        // Synthetic streams carrying each architecture's branch encoding at
        // a high density, so the converters actually fire. The 16-byte
        // stride keeps IA-64 bundles aligned.
        let markers: [(&str, [u8; 4]); 7] = [
            ("x86-call", [0xE8, 0x00, 0x00, 0x00]),
            ("x86-jmp", [0xE9, 0xFF, 0xFF, 0xFF]),
            ("ppc-bl", [0x48, 0x00, 0x10, 0x01]),
            ("arm-bl", [0x10, 0x20, 0x30, 0xEB]),
            ("thumb-bl", [0x11, 0xF0, 0x22, 0xF8]),
            ("sparc-call", [0x40, 0x00, 0x12, 0x34]),
            ("arm64-bl", [0x11, 0x22, 0x33, 0x94]),
        ];

        let mut payloads: Vec<(String, Vec<u8>)> = Vec::new();
        for (name, marker) in markers {
            let mut data = Vec::with_capacity(64 * 1024);
            let mut seed = 0x9E37_79B9_7F4A_7C15u64;
            while data.len() < 64 * 1024 {
                data.extend_from_slice(&marker);
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                data.extend_from_slice(&seed.to_le_bytes());
                data.extend_from_slice(&marker);
                // IA-64 bundle template bytes with branch slots.
                data.extend_from_slice(&[0x16, 0x00, 0x00, 0x00]);
            }
            data.truncate(64 * 1024);
            payloads.push((name.to_string(), data));
        }
        // A byte-gradient image, which is what Delta is actually for.
        payloads.push((
            "gradient".to_string(),
            (0..64u32 * 1024).map(|i| (i / 7) as u8).collect(),
        ));

        let filter_flags = [
            "--x86",
            "--powerpc",
            "--ia64",
            "--arm",
            "--armthumb",
            "--sparc",
            "--arm64",
            "--delta=dist=1",
            "--delta=dist=3",
            "--delta=dist=256",
        ];

        let mut checked = 0usize;
        for (name, payload) in &payloads {
            let raw = dir.join(format!("{name}.bin"));
            std::fs::write(&raw, payload).expect("write payload");
            for flag in filter_flags {
                let output = Command::new("xz")
                    .args(["-T1", "-c", flag, "--lzma2=preset=1"])
                    .arg(&raw)
                    .output()
                    .expect("spawn xz");
                assert!(
                    output.status.success(),
                    "xz {flag} failed: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
                let stream = output.stdout;

                let decoded = xz::decompress(&mut Cursor::new(&stream))
                    .unwrap_or_else(|e| panic!("[{name} {flag}] decompress: {e}"));
                assert_eq!(&decoded, payload, "[{name} {flag}] wrong bytes");

                let mut buffer = vec![0u8; payload.len()];
                let written = xz::decompress_into(&stream, &mut buffer)
                    .unwrap_or_else(|e| panic!("[{name} {flag}] decompress_into: {e}"));
                assert_eq!(written, payload.len());
                assert_eq!(&buffer, payload, "[{name} {flag}] wrong bytes (into)");
                checked += 1;
            }
        }

        eprintln!(
            "[xz-oracle] {checked} `xz` filter-chain streams (7 BCJ converters + Delta) \
             decoded byte-identically"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The one filter the module deliberately does not implement must be
    /// reported as unsupported, never silently mis-decoded.
    #[test]
    fn xz_cli_riscv_filter_is_reported_unsupported() {
        if !tool_available("xz", &["--version"]) {
            eprintln!("[xz-oracle] `xz` not on PATH; skipping (self-skip, not a failure)");
            return;
        }
        let dir = unique_temp_dir("riscv");
        let payload: Vec<u8> = (0..32u32 * 1024).map(|i| (i % 251) as u8).collect();
        let raw = dir.join("payload.bin");
        std::fs::write(&raw, &payload).expect("write payload");

        let output = Command::new("xz")
            .args(["-T1", "-c", "--riscv", "--lzma2=preset=1"])
            .arg(&raw)
            .output()
            .expect("spawn xz");
        if !output.status.success() {
            eprintln!("[xz-oracle] this `xz` has no --riscv filter; skipping");
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }

        let err = xz::decompress(&mut Cursor::new(&output.stdout))
            .expect_err("the RISC-V filter must be reported, not guessed at");
        assert!(
            err.to_string().contains("RISC-V"),
            "expected a named RISC-V error, got: {err}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Dependency-free TIFF IFD parser: prints the strip table of a
    /// libtiff-written file. `tifffile` cannot be used here because it
    /// requires `imagecodecs` for TIFF LZMA.
    const PY_EXTRACT: &str = r#"
import struct
import sys

TYPE_SIZES = {1: 1, 2: 1, 3: 2, 4: 4, 5: 8, 6: 1, 7: 1, 8: 2, 9: 4, 10: 8, 11: 4, 12: 8}


def main(path):
    with open(path, "rb") as f:
        data = f.read()
    prefix = "<" if data[:2] == b"II" else ">"
    magic, ifd_off = struct.unpack(prefix + "HI", data[2:8])
    if magic != 42:
        sys.exit("not a classic TIFF")
    (count,) = struct.unpack(prefix + "H", data[ifd_off : ifd_off + 2])
    tags = {}
    for i in range(count):
        base = ifd_off + 2 + i * 12
        tag, typ, n = struct.unpack(prefix + "HHI", data[base : base + 8])
        size = TYPE_SIZES.get(typ, 0) * n
        payload = (
            data[base + 8 : base + 8 + size]
            if size <= 4
            else data[
                struct.unpack(prefix + "I", data[base + 8 : base + 12])[0] :
            ][:size]
        )
        fmt = {1: "B", 3: "H", 4: "I"}.get(typ)
        tags[tag] = list(struct.unpack(prefix + fmt * n, payload)) if fmt else []
    if tags[259][0] != 34925:
        sys.exit(f"expected Compression=34925 (LZMA), got {tags[259][0]}")
    height = tags[257][0]
    print(
        f"{tags[256][0]}\t{height}\t{tags.get(278, [height])[0]}"
        f"\t{tags.get(317, [1])[0]}"
    )
    for off, cnt in zip(tags[273], tags[279]):
        print(f"{off}\t{cnt}")


if __name__ == "__main__":
    main(sys.argv[1])
"#;

    /// Minimal uncompressed grayscale TIFF used as the `tiffcp` input.
    fn uncompressed_tiff(raw: &[u8], width: usize, height: usize) -> Vec<u8> {
        let strip_offset: u32 = 8;
        let mut body = raw.to_vec();
        if body.len() % 2 == 1 {
            body.push(0);
        }
        let ifd_offset = strip_offset + body.len() as u32;
        let mut out = Vec::new();
        out.extend_from_slice(b"II");
        out.extend_from_slice(&42u16.to_le_bytes());
        out.extend_from_slice(&ifd_offset.to_le_bytes());
        out.extend_from_slice(&body);
        let entries: [(u16, u16, u32, u32); 9] = [
            (256, 4, 1, width as u32),
            (257, 4, 1, height as u32),
            (258, 3, 1, 8),
            (259, 3, 1, 1),
            (262, 3, 1, 1),
            (273, 4, 1, strip_offset),
            (277, 3, 1, 1),
            (278, 4, 1, height as u32),
            (279, 4, 1, raw.len() as u32),
        ];
        out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        for (tag, typ, count, value) in entries {
            out.extend_from_slice(&tag.to_le_bytes());
            out.extend_from_slice(&typ.to_le_bytes());
            out.extend_from_slice(&count.to_le_bytes());
            out.extend_from_slice(&value.to_le_bytes());
        }
        out.extend_from_slice(&0u32.to_le_bytes());
        out
    }

    fn image_bytes(width: usize, height: usize) -> Vec<u8> {
        let mut data = Vec::with_capacity(width * height);
        let mut seed: u64 = 0x2545_F491_4F6C_DD1D;
        for y in 0..height {
            for x in 0..width {
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                data.push((((x + y * 3) / 2 + ((seed >> 59) as usize & 7)) % 256) as u8);
            }
        }
        data
    }

    /// TIFF `Compression = 34925`: every strip is a complete `.xz` stream,
    /// written by libtiff with `LZMA_CHECK_NONE`. Decode each of them
    /// straight into an exactly-sized row buffer.
    #[test]
    fn tiffcp_lzma_strips_decode_into_byte_identical() {
        if !tool_available("which", &["tiffcp"]) {
            eprintln!("[xz-oracle] `tiffcp` not on PATH; skipping (self-skip, not a failure)");
            return;
        }
        if !tool_available("python3", &["-c", "import struct"]) {
            eprintln!("[xz-oracle] `python3` not available; skipping (self-skip, not a failure)");
            return;
        }

        let dir = unique_temp_dir("tiffcp");
        let script = dir.join("extract.py");
        std::fs::write(&script, PY_EXTRACT).expect("write extractor");

        let (width, height) = (257usize, 61usize);
        let raw = image_bytes(width, height);
        let src = dir.join("src.tif");
        std::fs::write(&src, uncompressed_tiff(&raw, width, height)).expect("write src tiff");

        let mut checked_strips = 0usize;
        let mut saw_check_none = false;
        for rows in ["1", "4", "20", "61"] {
            let dst = dir.join(format!("out_r{rows}.tif"));
            let output = Command::new("tiffcp")
                .args(["-c", "lzma", "-r", rows])
                .arg(&src)
                .arg(&dst)
                .output()
                .expect("spawn tiffcp");
            if !output.status.success() {
                eprintln!(
                    "[xz-oracle] tiffcp -c lzma -r {rows} failed: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
                continue;
            }

            let listing = Command::new("python3")
                .arg(&script)
                .arg(&dst)
                .output()
                .expect("spawn extractor");
            assert!(
                listing.status.success(),
                "extractor failed: {}",
                String::from_utf8_lossy(&listing.stderr)
            );
            let stdout = String::from_utf8_lossy(&listing.stdout).to_string();
            let mut lines = stdout.lines();
            let header: Vec<usize> = lines
                .next()
                .expect("header")
                .split('\t')
                .map(|f| f.parse().expect("numeric"))
                .collect();
            let rows_per_strip = header[2];
            // libtiff enables horizontal differencing for `-c lzma` by
            // default; the codec output is the *differenced* rows, so undo
            // the predictor before comparing against the source pixels.
            let predictor = header[3];
            let file = std::fs::read(&dst).expect("read tiffcp output");

            for (index, line) in lines.enumerate() {
                let mut fields = line.split('\t');
                let offset: usize = fields.next().expect("offset").parse().expect("int");
                let count: usize = fields.next().expect("count").parse().expect("int");
                let strip = &file[offset..offset + count];

                // libtiff writes LZMA_CHECK_NONE: magic then flags 00 00.
                assert_eq!(&strip[..6], &[0xFD, 0x37, 0x7A, 0x58, 0x5A, 0x00]);
                if strip[6] == 0x00 && strip[7] == 0x00 {
                    saw_check_none = true;
                }

                let first_row = index * rows_per_strip;
                let rows_here = rows_per_strip.min(height - first_row);
                let expected_len = rows_here * width;

                let mut decoded = vec![0u8; expected_len];
                let written = xz::decompress_into(strip, &mut decoded)
                    .unwrap_or_else(|e| panic!("[r={rows}] strip {index} failed to decode: {e}"));
                assert_eq!(written, expected_len, "[r={rows}] strip {index}");

                // The bounded growable path must agree, byte for byte,
                // before the predictor is undone.
                let capped = xz::decompress_with_limit(strip, expected_len)
                    .unwrap_or_else(|e| panic!("[r={rows}] strip {index} with limit: {e}"));
                assert_eq!(capped, decoded);

                if predictor == 2 {
                    for row in decoded.chunks_mut(width) {
                        for i in 1..row.len() {
                            row[i] = row[i].wrapping_add(row[i - 1]);
                        }
                    }
                }
                assert_eq!(
                    decoded,
                    raw[first_row * width..first_row * width + expected_len],
                    "[r={rows}] strip {index} decoded to the wrong pixels"
                );
                checked_strips += 1;
            }
        }

        assert!(
            checked_strips > 50,
            "expected many libtiff LZMA strips, got {checked_strips}"
        );
        assert!(saw_check_none, "libtiff should write LZMA_CHECK_NONE");
        eprintln!(
            "[xz-oracle] tiffcp: {checked_strips} Compression=34925 strips decoded \
             byte-identically through xz::decompress_into"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
