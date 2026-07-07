//! Regression test: `oxiarc test` and `oxiarc list` must fail with a
//! non-zero exit code (and a clean error message on stderr) when run
//! against a file whose archive format cannot be recognized, instead of
//! silently printing a "not supported" / "Unsupported format" notice and
//! still exiting 0.
//!
//! `oxiarc detect` is intentionally exempt: reporting `Format: Unknown`
//! with exit 0 is that command's actual job, so it is not covered here.

use std::path::PathBuf;
use std::process::Command;

fn cli_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_oxiarc"))
}

/// Write a file that does not match any known archive/compression magic
/// number, so `ArchiveFormat::detect` resolves it to `Unknown`.
fn write_garbage_fixture(tag: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "oxiarc_unrecognized_format_{}_{}.zip",
        tag,
        std::process::id()
    ));
    std::fs::write(&path, b"not a real archive\n").expect("write garbage fixture");
    path
}

#[test]
fn test_test_command_rejects_unrecognized_format() {
    let fixture = write_garbage_fixture("test");

    let output = Command::new(cli_bin())
        .args(["test", "--color=never"])
        .arg(&fixture)
        .output()
        .expect("run oxiarc test");

    assert!(
        !output.status.success(),
        "oxiarc test on an unrecognized-format file unexpectedly exited 0"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("unsupported") || stderr.contains("unrecognized"),
        "expected an unsupported/unrecognized-format error on stderr, got: {}",
        stderr
    );
    assert!(
        stderr.contains(&fixture.display().to_string()),
        "expected the archive path in the error message, got: {}",
        stderr
    );

    let _ = std::fs::remove_file(&fixture);
}

#[test]
fn test_list_command_rejects_unrecognized_format() {
    let fixture = write_garbage_fixture("list");

    let output = Command::new(cli_bin())
        .args(["list", "--color=never"])
        .arg(&fixture)
        .output()
        .expect("run oxiarc list");

    assert!(
        !output.status.success(),
        "oxiarc list on an unrecognized-format file unexpectedly exited 0"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("unsupported") || stderr.contains("unrecognized"),
        "expected an unsupported/unrecognized-format error on stderr, got: {}",
        stderr
    );
    assert!(
        stderr.contains(&fixture.display().to_string()),
        "expected the archive path in the error message, got: {}",
        stderr
    );

    let _ = std::fs::remove_file(&fixture);
}

#[test]
fn test_list_json_command_rejects_unrecognized_format() {
    let fixture = write_garbage_fixture("list_json");

    let output = Command::new(cli_bin())
        .args(["list", "--json", "--color=never"])
        .arg(&fixture)
        .output()
        .expect("run oxiarc list --json");

    assert!(
        !output.status.success(),
        "oxiarc list --json on an unrecognized-format file unexpectedly exited 0"
    );

    // No JSON payload should be emitted on stdout for a format that could
    // not be recognized - the error belongs on stderr only.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.trim().is_empty(),
        "expected no JSON on stdout for an unrecognized format, got: {}",
        stdout
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("unsupported") || stderr.contains("unrecognized"),
        "expected an unsupported/unrecognized-format error on stderr, got: {}",
        stderr
    );

    let _ = std::fs::remove_file(&fixture);
}
