//! Regression tests for `--memory-limit`, the decompression-bomb defense the
//! CLI advertises in `--help`.
//!
//! History (CLI-01): `--memory-limit` was **silently unenforced** for the
//! file-based gzip, xz and bzip2 extraction paths — the primary usage mode. A
//! 4 MB bomb compressed to a few hundred bytes expanded in full under
//! `--memory-limit 64K` and the CLI exited 0. Only zstd and lz4 (which carry a
//! declared content size in the frame header) actually rejected it.
//!
//! History (CLI-02): for brotli and snappy — the two formats that declare no
//! uncompressed size *and* had no bounded decoder — the limit was enforced
//! only *after* a full decode: the bomb was materialised in memory and merely
//! never written. Both codecs now expose bounded decoders that check each
//! meta-block's / chunk's declared size against the remaining budget before
//! decoding it, and the CLI routes `--memory-limit` through them. (The codec
//! crates' own `tests/memory_limit.rs` pin the no-allocation property with a
//! heap-tracking allocator; here we pin the user-visible CLI behavior.)
//!
//! These tests pin the fixed behavior for *every* single-file format, in both
//! the file-path and the stdin path:
//!
//! * a bomb under a small `--memory-limit` exits **non-zero**, writes **no**
//!   output file, and prints a clear message (never a panic / exit 101);
//! * a payload comfortably *under* the limit still extracts, byte-for-byte
//!   (no false positives).
//!
//! Also covers CLI-03: extracting a single-file format into a not-yet-existing
//! output directory must create it (it used to fail with a raw
//! `No such file or directory (os error 2)`).

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Bomb payload: 4 MB of zeros. Compresses to a few hundred bytes in every
/// format under test, and dwarfs the 64 KB limit the tests apply.
const BOMB_SIZE: usize = 4 * 1024 * 1024;

/// The memory limit every bomb is run against.
const LIMIT: &str = "64K";

fn cli_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_oxiarc"))
}

/// A temp directory unique to this process *and* this call.
///
/// Tests in a binary run concurrently on separate threads, so a shared
/// `temp_dir()/oxiarc_<pid>` path would race (this repo has been bitten by
/// exactly that before). The counter + nanosecond stamp guarantee a distinct
/// path per test even within one process.
fn unique_dir(tag: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!(
        "oxiarc_memlimit_{tag}_{}_{seq}_{nanos}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("create unique temp dir");
    dir
}

/// Compress `data` into `format`, using oxiarc's own writers so the fixtures
/// need no external tools.
fn compress(data: &[u8], format: &str) -> Vec<u8> {
    match format {
        "gz" => oxiarc_archive::gzip::compress_with_filename(data, "payload.bin", 6)
            .expect("gzip compress"),
        "xz" => oxiarc_archive::XzWriter::new(oxiarc_lzma::LzmaLevel::new(6))
            .compress(data)
            .expect("xz compress"),
        "bz2" => oxiarc_archive::Bzip2Writer::with_level(9)
            .compress(data)
            .expect("bzip2 compress"),
        "zst" => oxiarc_archive::ZstdWriter::new()
            .compress(data)
            .expect("zstd compress"),
        "br" => oxiarc_archive::BrotliWriter::with_quality(6)
            .compress(data)
            .expect("brotli compress"),
        "sz" => oxiarc_archive::snappy::compress(data).expect("snappy compress"),
        other => panic!("unhandled fixture format: {other}"),
    }
}

/// Count regular files under `dir` (recursively).
fn files_written(dir: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .map(|e| {
            let p = e.path();
            if p.is_dir() { files_written(&p) } else { 1 }
        })
        .sum()
}

/// A bomb in `format` must be refused under `--memory-limit`, with nothing
/// written to disk and no panic.
fn assert_bomb_rejected(format: &str) {
    let dir = unique_dir(&format!("bomb_{format}"));
    let archive = dir.join(format!("bomb.{format}"));
    std::fs::write(&archive, compress(&vec![0u8; BOMB_SIZE], format)).expect("write bomb fixture");

    let out = dir.join("out");
    let output = Command::new(cli_bin())
        .args(["extract", "--color=never", "--memory-limit", LIMIT])
        .arg(&archive)
        .arg("-o")
        .arg(&out)
        .output()
        .expect("run oxiarc extract");

    let stderr = String::from_utf8_lossy(&output.stderr);
    let code = output.status.code();

    assert_ne!(
        code,
        Some(101),
        "{format} bomb PANICKED the CLI under --memory-limit: {stderr}"
    );
    assert!(
        !stderr.contains("panicked at"),
        "{format} bomb panicked under --memory-limit: {stderr}"
    );
    assert!(
        !output.status.success(),
        "{format} bomb expanded under --memory-limit {LIMIT} and exited 0 \
         (this is the CLI-01 regression); stderr: {stderr}"
    );
    assert!(
        !stderr.is_empty(),
        "{format} bomb was rejected but printed no error message"
    );
    assert_eq!(
        files_written(&out),
        0,
        "{format} bomb was rejected but still wrote output files"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// A payload well under the limit must still extract, byte-for-byte.
fn assert_small_payload_extracts(format: &str) {
    let dir = unique_dir(&format!("small_{format}"));
    // ~24 KB of structured data, comfortably under the 64K limit.
    let payload: Vec<u8> = (0..24_000u32).map(|i| (i % 251) as u8).collect();
    let archive = dir.join(format!("small.{format}"));
    std::fs::write(&archive, compress(&payload, format)).expect("write small fixture");

    let out = dir.join("out");
    let output = Command::new(cli_bin())
        .args(["extract", "--color=never", "--memory-limit", LIMIT])
        .arg(&archive)
        .arg("-o")
        .arg(&out)
        .output()
        .expect("run oxiarc extract");

    assert!(
        output.status.success(),
        "{format} payload under --memory-limit {LIMIT} was wrongly rejected: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // gzip restores the recorded original filename; the others use the stem.
    let expected = if format == "gz" {
        out.join("payload.bin")
    } else {
        out.join("small")
    };
    let extracted = std::fs::read(&expected)
        .unwrap_or_else(|e| panic!("{format}: reading {}: {e}", expected.display()));
    assert_eq!(
        extracted, payload,
        "{format} extracted content differs from the original"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_gzip_bomb_rejected_under_memory_limit() {
    assert_bomb_rejected("gz");
}

#[test]
fn test_xz_bomb_rejected_under_memory_limit() {
    assert_bomb_rejected("xz");
}

#[test]
fn test_bzip2_bomb_rejected_under_memory_limit() {
    assert_bomb_rejected("bz2");
}

#[test]
fn test_zstd_bomb_rejected_under_memory_limit() {
    assert_bomb_rejected("zst");
}

/// Brotli declares no output size, so `--memory-limit` is enforced *during*
/// decoding, per meta-block (`oxiarc_brotli::decompress_with_limit`): the
/// bomb's expansion is never allocated. Before that existed, the CLI decoded
/// the whole bomb into memory and only then refused to write it.
#[test]
fn test_brotli_bomb_rejected_under_memory_limit() {
    assert_bomb_rejected("br");
}

/// Same for Snappy: the cap is checked against each chunk's declared size
/// before that chunk is decoded (`oxiarc_snappy::decompress_frame_with_limit`).
#[test]
fn test_snappy_bomb_rejected_under_memory_limit() {
    assert_bomb_rejected("sz");
}

#[test]
fn test_gzip_small_payload_still_extracts_under_limit() {
    assert_small_payload_extracts("gz");
}

#[test]
fn test_xz_small_payload_still_extracts_under_limit() {
    assert_small_payload_extracts("xz");
}

#[test]
fn test_bzip2_small_payload_still_extracts_under_limit() {
    assert_small_payload_extracts("bz2");
}

/// No false positives on the newly bounded paths: an in-budget `.br` payload
/// must still extract byte-for-byte.
#[test]
fn test_brotli_small_payload_still_extracts_under_limit() {
    assert_small_payload_extracts("br");
}

/// No false positives on the newly bounded paths: an in-budget `.sz` payload
/// must still extract byte-for-byte (its 24 KB spans a single 64 KiB chunk).
#[test]
fn test_snappy_small_payload_still_extracts_under_limit() {
    assert_small_payload_extracts("sz");
}

/// The stdin path shares `decompress_single_file_full` with the file path;
/// pin that the limit is enforced there too.
#[test]
fn test_stdin_bomb_rejected_under_memory_limit() {
    for (format, flag) in [
        ("gz", "gzip"),
        ("xz", "xz"),
        ("bz2", "bz2"),
        ("br", "br"),
        ("sz", "snappy"),
    ] {
        let dir = unique_dir(&format!("stdin_bomb_{format}"));
        let out = dir.join("out");
        let bomb = compress(&vec![0u8; BOMB_SIZE], format);

        let mut child = Command::new(cli_bin())
            .args(["extract", "-", "--color=never", "--format", flag])
            .arg("-o")
            .arg(&out)
            .args(["--memory-limit", LIMIT])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn oxiarc extract -");

        child
            .stdin
            .as_mut()
            .expect("child stdin")
            .write_all(&bomb)
            .expect("pipe bomb to stdin");
        let output = child.wait_with_output().expect("wait for oxiarc");

        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_ne!(
            output.status.code(),
            Some(101),
            "stdin {format} bomb panicked the CLI: {stderr}"
        );
        assert!(
            !output.status.success(),
            "stdin {format} bomb expanded under --memory-limit {LIMIT} and exited 0; \
             stderr: {stderr}"
        );
        assert_eq!(
            files_written(&out),
            0,
            "stdin {format} bomb was rejected but still wrote output files"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// CLI-03: `oxiarc extract one.gz -o newdir` must create `newdir`, exactly as
/// the ZIP/TAR paths already did, instead of failing with a raw OS error.
#[test]
fn test_single_file_extract_creates_missing_output_dir() {
    let payload = b"the quick brown fox jumps over the lazy dog\n".repeat(200);

    for format in ["gz", "xz", "bz2", "zst", "br", "sz"] {
        let dir = unique_dir(&format!("mkdir_{format}"));
        let archive = dir.join(format!("data.{format}"));
        std::fs::write(&archive, compress(&payload, format)).expect("write fixture");

        // Deliberately nested and non-existent.
        let out = dir.join("does").join("not").join("exist");
        assert!(!out.exists());

        let output = Command::new(cli_bin())
            .args(["extract", "--color=never"])
            .arg(&archive)
            .arg("-o")
            .arg(&out)
            .output()
            .expect("run oxiarc extract");

        assert!(
            output.status.success(),
            "{format}: extract into a non-existent output dir failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            out.is_dir(),
            "{format}: output directory was not created by extract"
        );

        let expected = if format == "gz" {
            out.join("payload.bin")
        } else {
            out.join("data")
        };
        assert_eq!(
            std::fs::read(&expected).expect("read extracted file"),
            payload,
            "{format}: extracted content differs"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// FINALGATE F1: a valid RFC 1952 §2.2 concatenated `.gz` (`cat a.gz b.gz`,
/// `pigz`, `bgzip`, rsyncable gzips) must extract to the concatenation of
/// every member. It used to fail with a spurious `CRC mismatch` and exit 1,
/// because the archive-layer reader treated the file's *last* 8 bytes as the
/// only trailer and inflated everything before them as one member.
#[test]
fn test_multi_member_gzip_extracts_every_member() {
    let dir = unique_dir("multimember_gz");
    let archive = dir.join("two.gz");

    let first = b"first member payload\n".repeat(400);
    let second = b"second member payload\n".repeat(400);
    let mut stream = compress(&first, "gz");
    stream.extend_from_slice(&compress(&second, "gz"));
    std::fs::write(&archive, &stream).expect("write multi-member fixture");

    let out = dir.join("out");
    let output = Command::new(cli_bin())
        .args(["extract", "--color=never"])
        .arg(&archive)
        .arg("-o")
        .arg(&out)
        .output()
        .expect("run oxiarc extract");

    assert!(
        output.status.success(),
        "multi-member .gz extract failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let mut expected = first.clone();
    expected.extend_from_slice(&second);
    assert_eq!(
        std::fs::read(out.join("payload.bin")).expect("read extracted file"),
        expected,
        "multi-member .gz lost a member"
    );

    // `oxiarc test` and `oxiarc list` go through the same reader.
    for subcommand in ["test", "list"] {
        let output = Command::new(cli_bin())
            .args([subcommand, "--color=never"])
            .arg(&archive)
            .output()
            .expect("run oxiarc subcommand");
        assert!(
            output.status.success(),
            "`oxiarc {subcommand}` on a multi-member .gz failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let _ = std::fs::remove_dir_all(&dir);
}

/// The multi-member fix must not open a `--memory-limit` hole: a bomb split
/// across three members, each individually under the limit, is still refused
/// (the cap now bounds the running total across every member, inside a
/// DEFLATE block, rather than trusting the last member's ISIZE field).
#[test]
fn test_multi_member_gzip_bomb_rejected_under_memory_limit() {
    let dir = unique_dir("multimember_bomb");
    let archive = dir.join("bomb.gz");

    let member = vec![0u8; BOMB_SIZE / 3];
    let mut stream = compress(&member, "gz");
    stream.extend_from_slice(&compress(&member, "gz"));
    stream.extend_from_slice(&compress(&member, "gz"));
    std::fs::write(&archive, &stream).expect("write multi-member bomb");

    let out = dir.join("out");
    let output = Command::new(cli_bin())
        .args(["extract", "--color=never", "--memory-limit", LIMIT])
        .arg(&archive)
        .arg("-o")
        .arg(&out)
        .output()
        .expect("run oxiarc extract");

    assert!(
        !output.status.success(),
        "multi-member gzip bomb was accepted under --memory-limit {LIMIT}"
    );
    assert_ne!(
        output.status.code(),
        Some(101),
        "multi-member gzip bomb panicked instead of erroring cleanly"
    );
    assert_eq!(
        files_written(&out),
        0,
        "multi-member gzip bomb was rejected but still wrote output files"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
