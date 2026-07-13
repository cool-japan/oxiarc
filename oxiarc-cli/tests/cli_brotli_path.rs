//! Regression tests for CLI-02: magic-less formats reached via a file path.
//!
//! Raw Brotli has no magic bytes at all, so `ArchiveFormat::detect` could only
//! ever report `Unknown` for a `.br` file. Every file-path read command
//! therefore failed — including on the CLI's *own* `oxiarc create mine.br`
//! output, and including every `oxiarc extract data.br` example in `--help`.
//! Only `cat mine.br | oxiarc extract - --format br` worked.
//!
//! The read commands now detect via `ArchiveFormat::detect_with_path`, which
//! keeps magic authoritative and falls back to the filename extension
//! (`.br`/`.brotli`, `.sz`/`.snappy`) only when the content is unrecognized.
//!
//! These tests pin the full `create -> detect | list | test | extract | info |
//! convert` loop over a real `.br` file.

use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

fn cli_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_oxiarc"))
}

/// Temp directory unique to this process *and* this call — tests within a
/// binary run concurrently, so a shared path would race.
fn unique_dir(tag: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!(
        "oxiarc_brotli_path_{tag}_{}_{seq}_{nanos}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("create unique temp dir");
    dir
}

/// Run `oxiarc <args...>`, asserting success and returning stdout.
fn run_ok(args: &[&std::ffi::OsStr], what: &str) -> String {
    let output = Command::new(cli_bin())
        .arg("--color=never")
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("spawning oxiarc for {what}: {e}"));
    assert!(
        output.status.success(),
        "`oxiarc {what}` failed (exit {:?}); stderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// The whole point of CLI-02: everything the CLI can do to a `.br` file it
/// produced itself must actually work through the file path.
#[test]
fn test_brotli_file_path_roundtrip_all_commands() {
    use std::ffi::OsStr;

    let dir = unique_dir("roundtrip");
    let source = dir.join("payload.txt");
    let payload = "brotli has no magic bytes; detection must fall back to the extension\n"
        .repeat(300)
        .into_bytes();
    std::fs::write(&source, &payload).expect("write source");

    let archive = dir.join("mine.br");

    // create
    run_ok(
        &[
            OsStr::new("create"),
            archive.as_os_str(),
            source.as_os_str(),
        ],
        "create mine.br",
    );
    assert!(archive.is_file(), "oxiarc create did not produce mine.br");

    // detect — must name Brotli, not Unknown
    let detected = run_ok(
        &[OsStr::new("detect"), archive.as_os_str()],
        "detect mine.br",
    );
    assert!(
        detected.contains("Brotli"),
        "detect reported the wrong format for mine.br: {detected}"
    );
    assert!(
        !detected.contains("Unknown"),
        "detect still reports Unknown for mine.br (CLI-02 regression): {detected}"
    );

    // list / info / test
    run_ok(&[OsStr::new("list"), archive.as_os_str()], "list mine.br");
    run_ok(&[OsStr::new("info"), archive.as_os_str()], "info mine.br");
    run_ok(&[OsStr::new("test"), archive.as_os_str()], "test mine.br");

    // list --json must produce parseable JSON naming the format
    let json = run_ok(
        &[
            OsStr::new("list"),
            OsStr::new("--json"),
            archive.as_os_str(),
        ],
        "list --json mine.br",
    );
    let parsed: serde_json::Value = serde_json::from_str(&json).expect("list --json emits JSON");
    assert_eq!(
        parsed["format"], "Brotli",
        "list --json reported the wrong format: {json}"
    );

    // extract — content must round-trip byte-for-byte
    let out = dir.join("out");
    run_ok(
        &[
            OsStr::new("extract"),
            archive.as_os_str(),
            OsStr::new("-o"),
            out.as_os_str(),
        ],
        "extract mine.br",
    );
    let extracted = std::fs::read(out.join("mine")).expect("read extracted mine");
    assert_eq!(
        extracted, payload,
        "extract mine.br produced different bytes than were compressed"
    );

    // convert — the .br input must be readable as a conversion source
    let converted = dir.join("mine.gz");
    run_ok(
        &[
            OsStr::new("convert"),
            archive.as_os_str(),
            converted.as_os_str(),
        ],
        "convert mine.br -> mine.gz",
    );
    run_ok(
        &[OsStr::new("test"), converted.as_os_str()],
        "test mine.gz (converted from .br)",
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// Magic stays authoritative: a file *named* `.br` whose content is really a
/// gzip stream must be detected as gzip, not mis-trusted from its extension.
#[test]
fn test_extension_fallback_does_not_override_magic() {
    use std::ffi::OsStr;

    let dir = unique_dir("magic_wins");
    let payload = b"content is king\n".repeat(100);
    let mislabeled = dir.join("actually_gzip.br");
    std::fs::write(
        &mislabeled,
        oxiarc_archive::gzip::compress_with_filename(&payload, "actually_gzip", 6)
            .expect("gzip compress"),
    )
    .expect("write mislabeled fixture");

    let detected = run_ok(
        &[OsStr::new("detect"), mislabeled.as_os_str()],
        "detect actually_gzip.br",
    );
    assert!(
        detected.contains("GZIP") || detected.contains("Gzip"),
        "content magic must win over the .br extension, got: {detected}"
    );

    let out = dir.join("out");
    run_ok(
        &[
            OsStr::new("extract"),
            mislabeled.as_os_str(),
            OsStr::new("-o"),
            out.as_os_str(),
        ],
        "extract actually_gzip.br",
    );
    assert_eq!(
        std::fs::read(out.join("actually_gzip")).expect("read extracted"),
        payload,
        "mislabeled gzip extracted incorrectly"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// An unrecognized file with an unrecognized extension must still be refused
/// with a non-zero exit — the extension fallback must not turn every unknown
/// blob into a "Brotli" stream.
#[test]
fn test_unknown_extension_still_rejected() {
    let dir = unique_dir("unknown");
    let junk = dir.join("mystery.dat");
    std::fs::write(&junk, b"\x01\x02\x03\x04 definitely not an archive\n").expect("write junk");

    for cmd in ["list", "test", "info"] {
        let output = Command::new(cli_bin())
            .args(["--color=never", cmd])
            .arg(&junk)
            .output()
            .expect("run oxiarc");
        assert!(
            !output.status.success(),
            "`oxiarc {cmd}` on an unrecognized file unexpectedly exited 0"
        );
        assert_ne!(
            output.status.code(),
            Some(101),
            "`oxiarc {cmd}` panicked on an unrecognized file"
        );
    }

    let _ = std::fs::remove_dir_all(&dir);
}
