//! Regression test: `oxiarc create x.7z ...` (and other extensions with no
//! writer, like .cab/.iso) must fail with a clear error instead of silently
//! writing ZIP-format data under the wrong file name.

use std::path::PathBuf;
use std::process::Command;

fn cli_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_oxiarc"))
}

fn workdir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "oxiarc_create_format_guard_{}_{}",
        tag,
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create workdir");
    dir
}

/// Run `oxiarc create <archive> <input>` and assert it fails without leaving
/// an output file (in particular, without leaving a ZIP-magic file) behind.
fn assert_create_refused(ext: &str) {
    let wd = workdir(ext);
    let input = wd.join("payload.txt");
    std::fs::write(&input, b"hello world").expect("write input");

    let archive = wd.join(format!("out.{}", ext));
    let output = Command::new(cli_bin())
        .args(["create", "--color=never"])
        .arg(&archive)
        .arg(&input)
        .output()
        .expect("run oxiarc create");

    assert!(
        !output.status.success(),
        "create out.{} unexpectedly succeeded",
        ext
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("not supported") && stderr.contains("zip"),
        "stderr should name the unsupported format and list supported ones, got: {}",
        stderr
    );

    // No file may be left behind — and certainly not one with ZIP magic.
    if let Ok(bytes) = std::fs::read(&archive) {
        assert!(
            !bytes.starts_with(b"PK"),
            "out.{} was written with ZIP magic despite the error",
            ext
        );
        panic!("out.{} was created despite the error", ext);
    }

    let _ = std::fs::remove_dir_all(&wd);
}

#[test]
fn test_create_7z_is_refused_and_leaves_no_file() {
    assert_create_refused("7z");
}

#[test]
fn test_create_cab_is_refused_and_leaves_no_file() {
    assert_create_refused("cab");
}

#[test]
fn test_create_iso_is_refused_and_leaves_no_file() {
    assert_create_refused("iso");
}

#[test]
fn test_create_no_extension_is_refused() {
    let wd = workdir("noext");
    let input = wd.join("payload.txt");
    std::fs::write(&input, b"hello world").expect("write input");

    let archive = wd.join("outfile");
    let output = Command::new(cli_bin())
        .args(["create", "--color=never"])
        .arg(&archive)
        .arg(&input)
        .output()
        .expect("run oxiarc create");

    assert!(!output.status.success(), "extension-less create must fail");
    assert!(!archive.exists(), "no output file may be left behind");

    let _ = std::fs::remove_dir_all(&wd);
}

#[test]
fn test_create_zip_still_works() {
    let wd = workdir("zipok");
    let input = wd.join("payload.txt");
    std::fs::write(&input, b"hello world").expect("write input");

    let archive = wd.join("out.zip");
    let status = Command::new(cli_bin())
        .args(["create", "--color=never"])
        .arg(&archive)
        .arg(&input)
        .status()
        .expect("run oxiarc create");

    assert!(status.success(), "zip creation must keep working");
    let bytes = std::fs::read(&archive).expect("read archive");
    assert!(bytes.starts_with(b"PK"), "out.zip must have ZIP magic");

    let _ = std::fs::remove_dir_all(&wd);
}
