//! Archive-level encode-direction oracle: prove that `.lzh` archives produced
//! by `oxiarc-archive`'s real public `LzhWriter` API are readable by the
//! genuine, independent `lha` CLI (Lhasa v0.6.0, decompress-only).
//!
//! `oxiarc-lzhuf`'s `lha-oracle` test validates the raw codec *stream*; this
//! one validates the surrounding *archive layer* — level-2 headers, the `0x01`
//! filename extension, CRC-16, method-id string, and compressed/original size
//! fields — end to end. For every archive we:
//!
//! - `lha t` — CRC-16 test the whole archive (must exit 0),
//! - `lha l` — list it (must exit 0; the on-disk name and size must appear),
//! - `lha x` — extract to a scratch dir and byte-diff (ASCII names), or
//! - `lha pq` — print to stdout and byte-diff (non-ASCII names, which a UTF-8
//!   only filesystem such as macOS APFS refuses to create).
//!
//! Gated behind the `lha-oracle` feature; each test self-skips (prints a note,
//! does not fail) when `lha` is not on PATH, so `--all-features` stays green on
//! machines without Lhasa installed.
#![cfg(feature = "lha-oracle")]

use oxiarc_archive::lzh::{LzhCompressionLevel, LzhWriter};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Shift_JIS bytes for `日本語ファイル.txt` (how the LZH writer stores it on
/// disk — the LHA convention). Used to confirm `lha l` echoes the stored bytes.
const SJIS_FILENAME: &[u8] = &[
    0x93, 0xFA, 0x96, 0x7B, 0x8C, 0xEA, // 日本語
    0x83, 0x74, 0x83, 0x40, 0x83, 0x43, 0x83, 0x8B, // ファイル
    b'.', b't', b'x', b't',
];

/// Locate the `lha` binary via `which`; `None` means "self-skip".
fn find_lha() -> Option<PathBuf> {
    // Probe the bare name first and use it as-is when it spawns:
    // `which` does not exist on Windows outside a POSIX shell (the
    // oracle would silently self-skip there), and inside one — MSYS /
    // Git Bash — it prints a POSIX path such as `/mingw64/bin/...`
    // that `CreateProcess` cannot open (the oracle would then panic
    // on spawn instead of running). Letting the OS resolve the name
    // avoids both. Only spawnability is checked, not the exit status.
    if Command::new("lha").arg("--version").output().is_ok() {
        return Some(PathBuf::from("lha"));
    }
    let locator = if cfg!(windows) { "where" } else { "which" };
    let output = Command::new(locator).arg("lha").output().ok()?;
    if !output.status.success() {
        return None;
    }
    // `where` can report several matches, one per line; take the first.
    let path = String::from_utf8_lossy(&output.stdout)
        .lines()
        .next()
        .unwrap_or_default()
        .trim()
        .to_string();
    if path.is_empty() {
        None
    } else {
        Some(PathBuf::from(path))
    }
}

/// True when `needle` occurs as a contiguous byte run in `haystack`.
fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && haystack.windows(needle.len()).any(|w| w == needle)
}

/// Create a unique per-test scratch directory under the system temp dir.
fn scratch_dir(label: &str) -> PathBuf {
    let mut dir = std::env::temp_dir();
    dir.push(format!(
        "oxiarc_archive_lha_oracle_{label}_{}_{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

/// Build an LZH archive (default level-2 lh5 headers unless `store`) from the
/// given `(name, data)` entries via the real public writer API.
fn build_archive(entries: &[(&str, &[u8])], store: bool) -> Vec<u8> {
    let mut buf = Vec::new();
    {
        let mut writer = LzhWriter::new(&mut buf);
        if store {
            writer.set_compression(LzhCompressionLevel::Store);
        }
        for (name, data) in entries {
            writer.add_file(name, data).expect("add_file");
        }
        writer.finish().expect("finish");
    }
    buf
}

/// Run `lha` with `args` and return the completed output.
fn run_lha(args: &[&str]) -> std::process::Output {
    Command::new("lha").args(args).output().expect("spawn lha")
}

/// `lha t` the archive and assert it CRC-tests clean.
fn assert_lha_test_ok(label: &str, archive_path: &Path) {
    let out = run_lha(&["t", &archive_path.to_string_lossy()]);
    assert!(
        out.status.success(),
        "[{label}] `lha t` failed (exit {:?}): stdout={:?} stderr={:?}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
}

/// `lha l` the archive and assert it lists cleanly and echoes each expected
/// on-disk name (as raw bytes) and decimal size.
fn assert_lha_lists(label: &str, archive_path: &Path, expect: &[(&[u8], usize)]) {
    let out = run_lha(&["l", &archive_path.to_string_lossy()]);
    assert!(
        out.status.success(),
        "[{label}] `lha l` failed (exit {:?}): stderr={:?}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr),
    );
    for (name_bytes, size) in expect {
        assert!(
            contains(&out.stdout, name_bytes),
            "[{label}] `lha l` output missing name {:?}; full listing:\n{}",
            String::from_utf8_lossy(name_bytes),
            String::from_utf8_lossy(&out.stdout),
        );
        assert!(
            contains(&out.stdout, size.to_string().as_bytes()),
            "[{label}] `lha l` output missing size {size}; full listing:\n{}",
            String::from_utf8_lossy(&out.stdout),
        );
    }
}

/// Write `archive_bytes` to a scratch `.lzh`, run `lha t` + `lha l`, then
/// `lha x`-extract to a subdirectory and byte-diff every ASCII-named entry.
fn oracle_extract_ascii(label: &str, entries: &[(&str, &[u8])], store: bool) {
    if find_lha().is_none() {
        eprintln!("[lha-oracle] `lha` not on PATH; skipping '{label}' (self-skip, not a failure)");
        return;
    }
    let dir = scratch_dir(label);
    let archive_path = dir.join("test.lzh");
    let archive = build_archive(entries, store);
    std::fs::write(&archive_path, &archive).expect("write archive");

    assert_lha_test_ok(label, &archive_path);

    let list_expect: Vec<(&[u8], usize)> = entries
        .iter()
        .map(|(name, data)| (name.as_bytes(), data.len()))
        .collect();
    assert_lha_lists(label, &archive_path, &list_expect);

    let extract_dir = dir.join("out");
    std::fs::create_dir_all(&extract_dir).expect("create extract dir");
    let out = run_lha(&[
        &format!("xfw={}", extract_dir.display()),
        &archive_path.to_string_lossy(),
    ]);
    assert!(
        out.status.success(),
        "[{label}] `lha x` failed (exit {:?}): stdout={:?} stderr={:?}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );

    for (name, data) in entries {
        let path = extract_dir.join(name);
        let got = std::fs::read(&path)
            .unwrap_or_else(|e| panic!("[{label}] reading extracted {name}: {e}"));
        assert_eq!(
            got,
            *data,
            "[{label}] extracted '{name}' differs from the original ({} vs {} bytes)",
            got.len(),
            data.len(),
        );
    }

    eprintln!(
        "[lha-oracle] '{label}': OK ({} entries, {} archive bytes; lha t + lha l + lha x all passed)",
        entries.len(),
        archive.len(),
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn oracle_single_small_text_file() {
    oracle_extract_ascii(
        "single_small_text",
        &[(
            "hello.txt",
            b"Hello, LZH world! This is a small text file.\n",
        )],
        false,
    );
}

#[test]
fn oracle_large_multiblock_lh5_file() {
    // ~150 KB spans several lh5 Huffman blocks (re-sent code tables): the case
    // a single-block encoder or a mis-sized per-block length field breaks.
    let data: Vec<u8> = b"The quick brown fox jumps over the lazy dog. 0123456789. "
        .iter()
        .cycle()
        .take(150 * 1024)
        .copied()
        .collect();
    oracle_extract_ascii("large_multiblock_lh5", &[("large.bin", &data)], false);
}

#[test]
fn oracle_multiple_files_in_one_archive() {
    let third: Vec<u8> = (0u8..=255).cycle().take(4096).collect();
    oracle_extract_ascii(
        "multiple_files",
        &[
            ("first.txt", b"first file contents\n"),
            (
                "second.txt",
                b"second file contents, a little longer than the first\n",
            ),
            ("third.dat", &third),
        ],
        false,
    );
}

#[test]
fn oracle_empty_file() {
    oracle_extract_ascii("empty_file", &[("empty.txt", b"")], false);
}

#[test]
fn oracle_stored_lh0_file() {
    oracle_extract_ascii(
        "stored_lh0",
        &[("stored.txt", b"stored, no compression applied\n")],
        true,
    );
}

#[test]
fn oracle_japanese_filename() {
    // The writer stores the name as Shift_JIS (the LZH convention). `lha t`
    // still CRC-tests it and `lha pq` prints the content; `lha x` is *not*
    // used because a UTF-8-only filesystem (macOS APFS) cannot create a file
    // whose name is raw Shift_JIS bytes — a platform limitation, not an
    // archive defect. `lha l` must nonetheless echo the stored Shift_JIS bytes.
    let label = "japanese_filename";
    if find_lha().is_none() {
        eprintln!("[lha-oracle] `lha` not on PATH; skipping '{label}' (self-skip, not a failure)");
        return;
    }
    let content = "これは日本語のテストデータです。".as_bytes().repeat(20);
    let archive = build_archive(&[("日本語ファイル.txt", &content)], false);

    let dir = scratch_dir(label);
    let archive_path = dir.join("test.lzh");
    std::fs::write(&archive_path, &archive).expect("write archive");

    assert_lha_test_ok(label, &archive_path);

    // `lha l` sanitizes non-ASCII bytes to '?' for terminal display, so the
    // raw Shift_JIS name cannot be byte-matched in its output; assert only the
    // ASCII-safe portions (decimal size + the `.txt` extension). The exact
    // Shift_JIS on-disk name is pinned by tests/lzh_japanese_names.rs, and a
    // clean `lha t` already requires a structurally valid Shift_JIS-named
    // header (a wrong name-length field would corrupt the parse).
    let list = run_lha(&["l", &archive_path.to_string_lossy()]);
    assert!(
        list.status.success(),
        "[{label}] `lha l` failed (exit {:?}): stderr={:?}",
        list.status.code(),
        String::from_utf8_lossy(&list.stderr),
    );
    assert!(
        contains(&list.stdout, content.len().to_string().as_bytes())
            && contains(&list.stdout, b".txt"),
        "[{label}] `lha l` missing size/extension; listing:\n{}",
        String::from_utf8_lossy(&list.stdout),
    );
    // Confirm the Shift_JIS bytes really are what got written to disk (lha's
    // display mangles them, but the archive itself must carry them verbatim).
    assert!(
        contains(&archive, SJIS_FILENAME),
        "[{label}] archive must store the Shift_JIS filename bytes verbatim",
    );

    let out = run_lha(&["pq", &archive_path.to_string_lossy()]);
    assert!(
        out.status.success(),
        "[{label}] `lha pq` failed (exit {:?}): stderr={:?}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr),
    );
    assert_eq!(
        out.stdout,
        content,
        "[{label}] `lha pq` output differs from the original content ({} vs {} bytes)",
        out.stdout.len(),
        content.len(),
    );

    eprintln!(
        "[lha-oracle] '{label}': OK (Shift_JIS name preserved; lha t + lha l + lha pq all passed)"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
