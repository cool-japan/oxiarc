//! Integration test for `oxiarc list --tree` hierarchical rendering.
//!
//! Creates a ZIP fixture with nested `dir1/a.txt`, `dir1/b.txt`,
//! `dir2/sub/c.txt`, `dir2/sub/d.txt`, and a top-level `root.txt`, then
//! runs `oxiarc list --tree` against it and verifies that tree connectors
//! (`├`/`└`) appear and all five filenames are present in stdout.
//!
//! This locks in the verify-and-close behaviour for the CLI tree view:
//! `print_tree` in `src/utils.rs` is the single source of truth, and this
//! test guards against regressions in its hierarchical rendering.

use std::process::Command;

use oxiarc_archive::zip::ZipWriter;

fn cli_bin() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_oxiarc"))
}

#[test]
fn test_list_tree_renders_hierarchy() {
    let fixture =
        std::env::temp_dir().join(format!("oxiarc_tree_fixture_{}.zip", std::process::id()));
    let mut buf: Vec<u8> = Vec::new();
    {
        let mut w = ZipWriter::new(&mut buf);
        w.add_file("root.txt", b"root").expect("add");
        w.add_file("dir1/a.txt", b"a").expect("add");
        w.add_file("dir1/b.txt", b"b").expect("add");
        w.add_file("dir2/sub/c.txt", b"c").expect("add");
        w.add_file("dir2/sub/d.txt", b"d").expect("add");
        w.finish().expect("finish");
    }
    std::fs::write(&fixture, &buf).expect("write fixture");

    let output = Command::new(cli_bin())
        .args(["list", "--tree", "--color=never"])
        .arg(&fixture)
        .output()
        .expect("run list");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);

    // Tree connectors
    assert!(
        stdout.contains('\u{251c}') || stdout.contains('\u{2514}'),
        "no tree connectors found in: {}",
        stdout
    );

    // All 5 filenames appear
    for name in &["root.txt", "a.txt", "b.txt", "c.txt", "d.txt"] {
        assert!(
            stdout.contains(name),
            "missing {} in tree output:\n{}",
            name,
            stdout
        );
    }

    let _ = std::fs::remove_file(&fixture);
}
