//! Integration tests for the `oxiarc add` subcommand.
//!
//! Each test lives under its own per-process temp directory so the suite
//! stays safe under `cargo nextest`'s parallel runner. The test fixtures
//! use `ZipWriter`, `TarWriter`, and `LzhWriter` directly to build a minimal
//! seed archive, then the compiled `oxiarc` binary is invoked to append
//! additional files, and the result is re-read to assert entry counts and
//! content.

use std::io::Cursor;
use std::path::PathBuf;
use std::process::Command;

use oxiarc_archive::{LzhReader, LzhWriter, TarReader, TarWriter, ZipReader, ZipWriter};

fn cli_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_oxiarc"))
}

fn scratch(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("oxiarc_add_{}_{}", std::process::id(), label));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

#[test]
fn test_cli_add_zip() {
    let wd = scratch("zip");
    let archive = wd.join("a.zip");
    {
        let file = std::fs::File::create(&archive).expect("create");
        let mut w = ZipWriter::new(file);
        w.add_file("one.txt", b"one").expect("add");
        w.add_file("two.txt", b"two").expect("add");
        w.finish().expect("finish");
    }
    let extra = wd.join("extra.txt");
    std::fs::write(&extra, b"three").expect("write extra");

    let status = Command::new(cli_bin())
        .args(["add", "--color=never"])
        .arg(&archive)
        .arg(&extra)
        .status()
        .expect("run add");
    assert!(status.success(), "add failed");

    let bytes = std::fs::read(&archive).expect("read archive");
    let reader = ZipReader::new(Cursor::new(&bytes)).expect("ZipReader::new");
    let names: Vec<String> = reader.entries().iter().map(|e| e.name.clone()).collect();
    assert_eq!(names.len(), 3, "got names: {:?}", names);
    assert!(names.iter().any(|n| n.ends_with("one.txt")));
    assert!(names.iter().any(|n| n.ends_with("two.txt")));
    assert!(names.iter().any(|n| n.ends_with("extra.txt")));

    let _ = std::fs::remove_dir_all(&wd);
}

#[test]
fn test_cli_add_tar() {
    let wd = scratch("tar");
    let archive = wd.join("a.tar");
    {
        let file = std::fs::File::create(&archive).expect("create");
        let mut w = TarWriter::new(file);
        w.add_file("one.txt", b"one").expect("add");
        w.add_file("two.txt", b"two").expect("add");
        w.finish().expect("finish");
    }
    let extra = wd.join("extra.txt");
    std::fs::write(&extra, b"three").expect("write extra");

    let status = Command::new(cli_bin())
        .args(["add", "--color=never"])
        .arg(&archive)
        .arg(&extra)
        .status()
        .expect("run add");
    assert!(status.success(), "add failed");

    let bytes = std::fs::read(&archive).expect("read archive");
    let reader = TarReader::new(Cursor::new(&bytes)).expect("TarReader::new");
    let names: Vec<String> = reader.entries().iter().map(|e| e.name.clone()).collect();
    assert_eq!(names.len(), 3, "got names: {:?}", names);
    assert!(names.iter().any(|n| n.ends_with("one.txt")));
    assert!(names.iter().any(|n| n.ends_with("two.txt")));
    assert!(names.iter().any(|n| n.ends_with("extra.txt")));

    let _ = std::fs::remove_dir_all(&wd);
}

#[test]
fn test_cli_add_lzh() {
    let wd = scratch("lzh");
    let archive = wd.join("a.lzh");
    {
        let file = std::fs::File::create(&archive).expect("create");
        let mut w = LzhWriter::new(file);
        w.add_file("one.txt", b"one").expect("add");
        w.add_file("two.txt", b"two").expect("add");
        w.finish().expect("finish");
    }
    let extra = wd.join("extra.txt");
    std::fs::write(&extra, b"three").expect("write extra");

    let status = Command::new(cli_bin())
        .args(["add", "--color=never"])
        .arg(&archive)
        .arg(&extra)
        .status()
        .expect("run add");
    assert!(status.success(), "add failed");

    let bytes = std::fs::read(&archive).expect("read archive");
    let reader = LzhReader::new(Cursor::new(&bytes)).expect("LzhReader::new");
    let entries = reader.entries();
    let names: Vec<String> = entries.iter().map(|e| e.name.clone()).collect();
    assert_eq!(names.len(), 3, "got names: {:?}", names);
    assert!(names.iter().any(|n| n.ends_with("one.txt")));
    assert!(names.iter().any(|n| n.ends_with("two.txt")));
    assert!(names.iter().any(|n| n.ends_with("extra.txt")));

    let _ = std::fs::remove_dir_all(&wd);
}

#[test]
fn test_cli_add_dry_run() {
    let wd = scratch("dry");
    let archive = wd.join("a.zip");
    {
        let file = std::fs::File::create(&archive).expect("create");
        let mut w = ZipWriter::new(file);
        w.add_file("one.txt", b"one").expect("add");
        w.finish().expect("finish");
    }
    let before = std::fs::read(&archive).expect("read before");

    let extra = wd.join("extra.txt");
    std::fs::write(&extra, b"x").expect("write extra");

    let output = Command::new(cli_bin())
        .args(["add", "--dry-run", "--color=never"])
        .arg(&archive)
        .arg(&extra)
        .output()
        .expect("run add");
    assert!(output.status.success(), "dry run failed");

    let after = std::fs::read(&archive).expect("read after");
    assert_eq!(before, after, "archive must be unchanged after --dry-run");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("[DRY RUN]"),
        "expected DRY RUN marker in stdout:\n{}",
        stdout
    );

    let _ = std::fs::remove_dir_all(&wd);
}

#[test]
fn test_cli_add_unsupported_format() {
    let wd = scratch("bad");
    // Build a .gz file via `create`.
    let input = wd.join("data.txt");
    std::fs::write(&input, b"hello").expect("write");

    let archive = wd.join("data.gz");
    let status = Command::new(cli_bin())
        .args(["create", "--color=never", "--format=gzip"])
        .arg(&archive)
        .arg(&input)
        .status()
        .expect("run create");
    assert!(status.success(), "seed gzip failed");

    let extra = wd.join("extra.txt");
    std::fs::write(&extra, b"x").expect("write extra");

    let output = Command::new(cli_bin())
        .args(["add", "--color=never"])
        .arg(&archive)
        .arg(&extra)
        .output()
        .expect("run add on gz");
    assert!(
        !output.status.success(),
        "add on .gz should fail, got status {:?}",
        output.status
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("does not support") || stderr.contains("Gzip"),
        "expected unsupported-format message, got: {}",
        stderr
    );

    let _ = std::fs::remove_dir_all(&wd);
}
