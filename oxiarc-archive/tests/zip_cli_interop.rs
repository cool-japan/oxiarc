//! Live ZIP differential tests against Info-ZIP (`zip`/`unzip`) and Python
//! `zipfile` (feature `zip-oracle`).
//!
//! Both directions are exercised:
//!
//! * Externally-created archives — stored, deflate, Zip64,
//!   data-descriptor (streamed), and `zip -e`/`zip -P` ZipCrypto — must be
//!   read correctly, with encryption *detected from the general-purpose
//!   bit flags* (ZIP-01) and ZipCrypto extraction accepting the Info-ZIP
//!   DOS-mtime check byte for streamed entries (ZIP-04).
//! * oxiarc-written archives must pass `unzip -t` (plain and ZipCrypto),
//!   carry the correct calendar date (ZIP-05), and — for AES — list in
//!   Python with the encryption flag set and CRC = 0 (AE-2, ZIP-03).
//!
//! Every test self-skips with a printed note when a required tool is not
//! on PATH, mirroring the `lha-oracle` pattern. Each test uses its own
//! unique temp directory (test name + pid + counter) so the default
//! parallel runner cannot race.

#![cfg(feature = "zip-oracle")]

use oxiarc_archive::zip::{
    ZipCompressionLevel, ZipReader, ZipWriter, is_entry_encrypted, is_entry_traditional_encrypted,
};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

static DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

fn unique_temp_dir(test_name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "oxiarc_zip_oracle_{}_{}_{}",
        test_name,
        std::process::id(),
        DIR_COUNTER.fetch_add(1, Ordering::SeqCst)
    ));
    fs::create_dir_all(&dir).expect("create unique temp dir");
    dir
}

fn tool_available(tool: &str, probe_args: &[&str]) -> bool {
    Command::new(tool)
        .args(probe_args)
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

fn require_tools(tools: &[(&str, &[&str])]) -> bool {
    for (tool, args) in tools {
        if !tool_available(tool, args) {
            eprintln!("skipping zip-oracle test: `{tool}` not available on PATH");
            return false;
        }
    }
    true
}

fn run_in(dir: &Path, tool: &str, args: &[&str]) -> std::process::Output {
    Command::new(tool)
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap_or_else(|e| panic!("failed to run `{tool}`: {e}"))
}

/// Deterministic pseudo-random bytes (xorshift64) for binary payloads.
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

fn text_payload() -> Vec<u8> {
    b"Interop is proven against reference tools, not by self-round-trips.\n"
        .iter()
        .cycle()
        .take(4096)
        .copied()
        .collect()
}

fn open_zip(path: &Path) -> ZipReader<fs::File> {
    ZipReader::new(fs::File::open(path).expect("open archive file")).expect("parse archive")
}

// =========================================================================
// External -> oxiarc
// =========================================================================

#[test]
fn info_zip_stored_and_deflate_archives_read_byte_exact() {
    if !require_tools(&[("zip", &["-v"])]) {
        return;
    }
    let dir = unique_temp_dir("stored_deflate");
    let alpha = text_payload();
    let beta = xorshift_bytes(0xBEEF_CAFE_1234_5678, 4096);
    fs::write(dir.join("alpha.txt"), &alpha).expect("write alpha");
    fs::write(dir.join("beta.bin"), &beta).expect("write beta");

    for (archive, level) in [("deflate.zip", "-9"), ("stored.zip", "-0")] {
        let out = run_in(
            &dir,
            "zip",
            &["-X", "-q", level, archive, "alpha.txt", "beta.bin"],
        );
        assert!(out.status.success(), "zip {level} failed: {out:?}");

        let mut reader = open_zip(&dir.join(archive));
        let entries = reader.entries().to_vec();
        assert_eq!(entries.len(), 2, "{archive}: entry count");
        for entry in &entries {
            assert!(
                !is_entry_encrypted(entry),
                "{archive}: plain entry misdetected as encrypted"
            );
            let expected: &[u8] = if entry.name == "alpha.txt" {
                &alpha
            } else {
                &beta
            };
            let data = reader.extract(entry).expect("extract");
            assert_eq!(data, expected, "{archive}: {} content mismatch", entry.name);
        }
    }
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn info_zip_encrypted_archive_detected_and_extracted() {
    // ZIP-01 + ZIP-04 headline differential: a standard `zip -P` (ZipCrypto)
    // archive was previously reported unencrypted and mis-extracted.
    if !require_tools(&[("zip", &["-v"])]) {
        return;
    }
    let dir = unique_temp_dir("zipcrypto");
    let alpha = text_payload();
    let beta = xorshift_bytes(0x00DD_BA11_5EED_7777, 2048);
    fs::write(dir.join("alpha.txt"), &alpha).expect("write alpha");
    fs::write(dir.join("beta.bin"), &beta).expect("write beta");

    let password = "correct-horse";
    let out = run_in(
        &dir,
        "zip",
        &["-q", "-P", password, "enc.zip", "alpha.txt", "beta.bin"],
    );
    assert!(out.status.success(), "zip -P failed: {out:?}");

    let mut reader = open_zip(&dir.join("enc.zip"));
    let entries = reader.entries().to_vec();
    assert_eq!(entries.len(), 2);
    for entry in &entries {
        assert!(
            is_entry_encrypted(entry),
            "external ZipCrypto entry {} not detected as encrypted",
            entry.name
        );
        assert!(
            is_entry_traditional_encrypted(entry),
            "external ZipCrypto entry {} not detected as traditional",
            entry.name
        );

        let expected: &[u8] = if entry.name == "alpha.txt" {
            &alpha
        } else {
            &beta
        };
        let data = reader
            .extract_encrypted(entry, password.as_bytes())
            .expect("extract with correct password");
        assert_eq!(data, expected, "{}: decrypted content mismatch", entry.name);

        assert!(
            reader.extract_encrypted(entry, b"wrong-password").is_err(),
            "{}: wrong password must be rejected",
            entry.name
        );
    }
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn python_zipfile_data_descriptor_and_zip64_archives_read() {
    if !require_tools(&[("python3", &["-c", "import zipfile"])]) {
        return;
    }
    let dir = unique_temp_dir("py_dd_zip64");
    let script = r#"
import io, zipfile

payload = bytes(range(256)) * 64  # 16 KiB, deterministic

class Unseekable(io.RawIOBase):
    def __init__(self, f):
        self.f = f
    def writable(self):
        return True
    def write(self, b):
        return self.f.write(b)
    def seekable(self):
        return False

# Streamed output -> local headers carry zero sizes + data descriptors.
with open("dd.zip", "wb") as raw:
    with zipfile.ZipFile(Unseekable(raw), "w", zipfile.ZIP_DEFLATED) as z:
        z.writestr("stream.bin", payload)

# Forced Zip64 extra fields even for a small member.
with zipfile.ZipFile("z64.zip", "w", zipfile.ZIP_DEFLATED) as z:
    with z.open(zipfile.ZipInfo("big.bin"), "w", force_zip64=True) as f:
        f.write(payload)
"#;
    let out = run_in(&dir, "python3", &["-c", script]);
    assert!(
        out.status.success(),
        "python zip generation failed: {out:?}"
    );

    let payload: Vec<u8> = (0u16..256).map(|v| v as u8).collect::<Vec<_>>().repeat(64);

    for (archive, name) in [("dd.zip", "stream.bin"), ("z64.zip", "big.bin")] {
        let mut reader = open_zip(&dir.join(archive));
        let entries = reader.entries().to_vec();
        assert_eq!(entries.len(), 1, "{archive}: entry count");
        assert_eq!(entries[0].name, name, "{archive}: entry name");
        assert!(!is_entry_encrypted(&entries[0]), "{archive}: not encrypted");
        let data = reader.extract(&entries[0]).expect("extract");
        assert_eq!(data, payload, "{archive}: content mismatch");
    }
    let _ = fs::remove_dir_all(&dir);
}

// =========================================================================
// oxiarc -> external
// =========================================================================

#[test]
fn oxiarc_written_zip_passes_unzip_t_and_has_correct_date() {
    if !require_tools(&[("unzip", &["-v"]), ("python3", &["-c", "import zipfile"])]) {
        return;
    }
    let dir = unique_temp_dir("oxi_plain");
    let alpha = text_payload();
    let beta = xorshift_bytes(0x5151_A0A0_C3C3_9F9F, 4096);

    let path = dir.join("oxi.zip");
    {
        let file = fs::File::create(&path).expect("create archive");
        let mut writer = ZipWriter::new(file);
        writer.add_file("alpha.txt", &alpha).expect("add deflate");
        writer
            .add_file_with_options("beta.bin", &beta, ZipCompressionLevel::Store)
            .expect("add stored");
        writer.finish().expect("finish");
    }

    // Independent integrity check by Info-ZIP.
    let out = run_in(&dir, "unzip", &["-t", "oxi.zip"]);
    assert!(
        out.status.success(),
        "unzip -t rejected the oxiarc archive: {}",
        String::from_utf8_lossy(&out.stdout)
    );

    // ZIP-05 differential: the stored DOS date must be *today* (UTC, with
    // one day of tolerance for a midnight race), not weeks off as with
    // the old 365/30-day calendar.
    let date_check = r#"
import zipfile, datetime, sys
z = zipfile.ZipFile("oxi.zip")
info = z.getinfo("alpha.txt")
written = datetime.date(*info.date_time[:3])
today = datetime.datetime.now(datetime.timezone.utc).date()
delta = abs((written - today).days)
print(f"written={written} today={today} delta={delta}")
sys.exit(0 if delta <= 1 else 1)
"#;
    let out = run_in(&dir, "python3", &["-c", date_check]);
    assert!(
        out.status.success(),
        "oxiarc-written DOS date is wrong: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn oxiarc_zipcrypto_archive_passes_unzip_t_with_password() {
    if !require_tools(&[("unzip", &["-v"])]) {
        return;
    }
    let dir = unique_temp_dir("oxi_zipcrypto");
    let secret = text_payload();

    let path = dir.join("oxienc.zip");
    {
        let file = fs::File::create(&path).expect("create archive");
        let mut writer = ZipWriter::new(file);
        writer
            .add_encrypted_file_traditional("secret.txt", &secret, b"hunter2!")
            .expect("add ZipCrypto entry");
        writer.finish().expect("finish");
    }

    let out = run_in(&dir, "unzip", &["-t", "-P", "hunter2!", "oxienc.zip"]);
    assert!(
        out.status.success(),
        "unzip -t -P rejected the oxiarc ZipCrypto archive: {}",
        String::from_utf8_lossy(&out.stdout)
    );

    // And the wrong password must fail the integrity test.
    let out = run_in(&dir, "unzip", &["-t", "-P", "wrong", "oxienc.zip"]);
    assert!(
        !out.status.success(),
        "unzip -t accepted a wrong password (check byte broken?)"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn oxiarc_aes_archive_lists_as_encrypted_with_crc_zero() {
    // ZIP-03 differential: AE-2 entries must expose CRC = 0 and the
    // encryption flag to external readers (Python cannot decrypt AES but
    // reads all the header metadata).
    if !require_tools(&[("python3", &["-c", "import zipfile"])]) {
        return;
    }
    let dir = unique_temp_dir("oxi_aes");
    let path = dir.join("oxiaes.zip");
    {
        let file = fs::File::create(&path).expect("create archive");
        let mut writer = ZipWriter::new(file);
        writer
            .add_encrypted_file("secret.txt", b"top secret payload", b"passw0rd")
            .expect("add AES entry");
        writer.finish().expect("finish");
    }

    let check = r#"
import zipfile, sys
z = zipfile.ZipFile("oxiaes.zip")
info = z.getinfo("secret.txt")
assert info.flag_bits & 0x1, f"encryption flag not set: {info.flag_bits:#x}"
assert info.compress_type == 99, f"AES method expected, got {info.compress_type}"
assert info.CRC == 0, f"AE-2 requires CRC 0, got {info.CRC:#010x}"
print("AES entry metadata OK")
"#;
    let out = run_in(&dir, "python3", &["-c", check]);
    assert!(
        out.status.success(),
        "python zipfile AES metadata check failed: {} {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    // The archive must still decrypt with oxiarc itself (HMAC path,
    // CRC field intentionally zero).
    let mut reader = open_zip(&path);
    let entry = reader.entries()[0].clone();
    assert!(is_entry_encrypted(&entry));
    let data = reader
        .extract_encrypted(&entry, b"passw0rd")
        .expect("decrypt AE-2 entry");
    assert_eq!(data, b"top secret payload");
    let _ = fs::remove_dir_all(&dir);
}
