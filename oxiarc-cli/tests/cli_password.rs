//! Integration tests for the `--password` flag on `oxiarc extract`.
//!
//! Builds a small encrypted ZIP fixture using `ZipWriter::add_encrypted_file`
//! (AES-256) and then runs the compiled binary with `--password=...` to
//! verify both the correct-password success path and the wrong-password
//! failure path (which must exit with status 2 and carry a "password"-ish
//! message on stderr).

use std::path::PathBuf;
use std::process::Command;

use oxiarc_archive::zip::ZipWriter;

fn cli_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_oxiarc"))
}

fn workdir(label: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("oxiarc_password_{}_{}", std::process::id(), label));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create workdir");
    dir
}

fn make_encrypted_zip(archive: &PathBuf, password: &[u8], entry_name: &str, body: &[u8]) {
    let mut buf: Vec<u8> = Vec::new();
    {
        let mut w = ZipWriter::new(&mut buf);
        w.add_encrypted_file(entry_name, body, password)
            .expect("add encrypted");
        w.finish().expect("finish");
    }
    std::fs::write(archive, &buf).expect("write archive");
}

#[test]
fn test_cli_extract_with_correct_password() {
    let wd = workdir("correct");
    let archive = wd.join("enc.zip");
    let out = wd.join("out");
    let pw = b"hunter2";
    let body = b"top secret contents";
    make_encrypted_zip(&archive, pw, "secret.txt", body);

    let output = Command::new(cli_bin())
        .args(["extract", "--color=never", "--password=hunter2", "-o"])
        .arg(&out)
        .arg(&archive)
        .output()
        .expect("run extract");
    assert!(
        output.status.success(),
        "extract failed: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let extracted = std::fs::read(out.join("secret.txt")).expect("read secret.txt");
    assert_eq!(extracted, body);

    let _ = std::fs::remove_dir_all(&wd);
}

#[test]
fn test_cli_extract_wrong_password() {
    let wd = workdir("wrong");
    let archive = wd.join("enc.zip");
    let out = wd.join("out");
    make_encrypted_zip(&archive, b"correct", "secret.txt", b"stuff");

    let output = Command::new(cli_bin())
        .args(["extract", "--color=never", "--password=wrong", "-o"])
        .arg(&out)
        .arg(&archive)
        .output()
        .expect("run extract");
    assert!(
        !output.status.success(),
        "extract should have failed with wrong password"
    );
    let code = output.status.code().unwrap_or(-1);
    assert_eq!(code, 2, "expected exit code 2, got {}", code);
    let stderr = String::from_utf8_lossy(&output.stderr).to_lowercase();
    assert!(
        stderr.contains("password") || stderr.contains("decrypt"),
        "expected password-related error, got: {}",
        stderr
    );

    let _ = std::fs::remove_dir_all(&wd);
}
