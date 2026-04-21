//! Integration tests for the `oxiarc` `--lenient` CLI flag.
//!
//! These tests verify that the `--lenient` flag is accepted by the
//! `list` and `extract` subcommands without error on clean archives.
//! Detailed lenient-mode behavior (CRC mismatch recovery, bad-checksum
//! skipping, scan-forward recovery) is unit-tested in the
//! `oxiarc-archive` crate; here we just confirm the CLI flag is wired.

use std::path::PathBuf;
use std::process::Command;

use oxiarc_archive::ZipWriter;

fn cli_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_oxiarc"))
}

#[test]
fn test_cli_extract_lenient_flag_accepted() {
    // Build a minimal clean ZIP fixture in the per-process temp dir so
    // parallel test runs can't stomp on each other.
    let fixture =
        std::env::temp_dir().join(format!("oxiarc_lenient_fixture_{}.zip", std::process::id()));

    let mut buf = Vec::new();
    {
        let mut w = ZipWriter::new(&mut buf);
        w.add_file("hello.txt", b"Hello, lenient!")
            .expect("add file to zip");
        w.finish().expect("finish zip");
    }
    std::fs::write(&fixture, &buf).expect("write fixture zip");

    // `list --lenient` — should succeed on a clean archive and emit no
    // warnings (warnings list stays empty because no corruption is
    // present).
    let list_output = Command::new(cli_bin())
        .args(["list", "--lenient"])
        .arg(&fixture)
        .output()
        .expect("run list --lenient");
    assert!(
        list_output.status.success(),
        "list --lenient failed: status={:?} stderr={}",
        list_output.status,
        String::from_utf8_lossy(&list_output.stderr)
    );

    // `extract --lenient` — should succeed and produce the expected
    // extracted file. A separate per-process output dir avoids clashes.
    let tmpdir =
        std::env::temp_dir().join(format!("oxiarc_lenient_extract_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmpdir);
    std::fs::create_dir_all(&tmpdir).expect("create extract dir");

    let extract_output = Command::new(cli_bin())
        .args(["extract", "--lenient", "-o"])
        .arg(&tmpdir)
        .arg(&fixture)
        .output()
        .expect("run extract --lenient");
    assert!(
        extract_output.status.success(),
        "extract --lenient failed: status={:?} stderr={}",
        extract_output.status,
        String::from_utf8_lossy(&extract_output.stderr)
    );

    // Verify extraction landed the expected file with the expected
    // content.
    let extracted = tmpdir.join("hello.txt");
    assert!(
        extracted.exists(),
        "extracted hello.txt not found under {}",
        tmpdir.display()
    );
    let contents = std::fs::read(&extracted).expect("read extracted file");
    assert_eq!(contents, b"Hello, lenient!");

    // Cleanup (best-effort).
    let _ = std::fs::remove_dir_all(&tmpdir);
    let _ = std::fs::remove_file(&fixture);
}

#[test]
fn test_cli_list_lenient_flag_accepted() {
    // Independent test so `extract` failure doesn't mask `list` breakage.
    let fixture = std::env::temp_dir().join(format!(
        "oxiarc_lenient_list_fixture_{}.zip",
        std::process::id()
    ));

    let mut buf = Vec::new();
    {
        let mut w = ZipWriter::new(&mut buf);
        w.add_file("a.txt", b"aaa").expect("add a.txt");
        w.add_file("b.txt", b"bbb").expect("add b.txt");
        w.finish().expect("finish zip");
    }
    std::fs::write(&fixture, &buf).expect("write fixture zip");

    let output = Command::new(cli_bin())
        .args(["list", "--lenient"])
        .arg(&fixture)
        .output()
        .expect("run list --lenient");
    assert!(
        output.status.success(),
        "list --lenient failed: status={:?} stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("a.txt"),
        "expected 'a.txt' in list output, got:\n{}",
        stdout
    );
    assert!(
        stdout.contains("b.txt"),
        "expected 'b.txt' in list output, got:\n{}",
        stdout
    );

    let _ = std::fs::remove_file(&fixture);
}
