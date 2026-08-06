//! End-to-end integration test for ZIP encryption.
//!
//! Round-trips password-protected archives through both ZipCrypto (traditional)
//! and AES-256 (WinZip AE-2) schemes, proving both encrypt/decrypt paths work
//! via the public API of `oxiarc_archive::zip`.
//!
//! Tests:
//! 1. Write encrypted entries using `ZipWriter::add_encrypted_file`
//!    (AES-256 / WinZip AE-2) and `ZipWriter::add_encrypted_file_traditional`
//!    (PKWARE ZipCrypto).
//! 2. Reopen the archive via `ZipReader::new` and extract each entry via
//!    `ZipReader::extract_encrypted`, confirming byte-exact recovery of the
//!    original plaintext.
//! 3. Confirm that providing the wrong password surfaces an error (the
//!    reader funnels both password-verification and HMAC failures through
//!    `OxiArcError::invalid_header`, so `is_err()` is the correct
//!    granularity for the assertion).

use oxiarc_archive::zip::{ZipReader, ZipWriter};
use std::io::Cursor;

const PASSWORD: &[u8] = b"correct horse battery staple";
const WRONG_PASSWORD: &[u8] = b"wrong password";

#[test]
fn test_aes256_roundtrip() {
    // Step 1: Write an archive with three AES-256 encrypted entries covering
    // small, medium, and empty payload shapes.
    let mut buf = Vec::new();
    {
        let mut writer = ZipWriter::new(&mut buf);
        writer
            .add_encrypted_file("greeting.txt", b"Hello, encrypted world!", PASSWORD)
            .expect("encrypt greeting");
        writer
            .add_encrypted_file(
                "secrets.txt",
                b"very secret data line 1\nvery secret data line 2\n",
                PASSWORD,
            )
            .expect("encrypt secrets");
        writer
            .add_encrypted_file("empty.txt", b"", PASSWORD)
            .expect("encrypt empty");
        writer.finish().expect("finish");
    }

    // Step 2: Reopen and extract each entry with the correct password.
    // `entries()` returns `&[Entry]`; clone to an owned Vec so we can still
    // call `&mut self` extract methods afterwards.
    let mut reader = ZipReader::new(Cursor::new(&buf)).expect("open archive");
    let entries: Vec<_> = reader.entries().to_vec();
    assert_eq!(entries.len(), 3, "expected three entries in archive");

    let greeting_entry = entries
        .iter()
        .find(|e| e.name == "greeting.txt")
        .expect("greeting entry");
    let greeting = reader
        .extract_encrypted(greeting_entry, PASSWORD)
        .expect("extract greeting");
    assert_eq!(greeting, b"Hello, encrypted world!");

    let secrets_entry = entries
        .iter()
        .find(|e| e.name == "secrets.txt")
        .expect("secrets entry");
    let secrets = reader
        .extract_encrypted(secrets_entry, PASSWORD)
        .expect("extract secrets");
    assert_eq!(
        secrets,
        b"very secret data line 1\nvery secret data line 2\n"
    );

    let empty_entry = entries
        .iter()
        .find(|e| e.name == "empty.txt")
        .expect("empty entry");
    let empty = reader
        .extract_encrypted(empty_entry, PASSWORD)
        .expect("extract empty");
    assert_eq!(empty, b"");

    // Step 3: Confirm that the wrong password is rejected.
    let wrong_result = reader.extract_encrypted(greeting_entry, WRONG_PASSWORD);
    assert!(
        wrong_result.is_err(),
        "expected error extracting AES entry with wrong password"
    );
}

#[test]
fn test_zipcrypto_roundtrip() {
    // Step 1: Write an archive with two traditional PKWARE (ZipCrypto)
    // encrypted entries.
    let mut buf = Vec::new();
    {
        let mut writer = ZipWriter::new(&mut buf);
        writer
            .add_encrypted_file_traditional("a.txt", b"ZipCrypto test A", PASSWORD)
            .expect("zipcrypto encrypt a");
        writer
            .add_encrypted_file_traditional(
                "b.txt",
                b"ZipCrypto test B with longer payload",
                PASSWORD,
            )
            .expect("zipcrypto encrypt b");
        writer.finish().expect("finish");
    }

    // Step 2: Reopen and extract each entry with the correct password.
    let mut reader = ZipReader::new(Cursor::new(&buf)).expect("open archive");
    let entries: Vec<_> = reader.entries().to_vec();
    assert_eq!(entries.len(), 2, "expected two entries in archive");

    let a_entry = entries.iter().find(|e| e.name == "a.txt").expect("a entry");
    let a = reader
        .extract_encrypted(a_entry, PASSWORD)
        .expect("extract a");
    assert_eq!(a, b"ZipCrypto test A");

    let b_entry = entries.iter().find(|e| e.name == "b.txt").expect("b entry");
    let b = reader
        .extract_encrypted(b_entry, PASSWORD)
        .expect("extract b");
    assert_eq!(b, b"ZipCrypto test B with longer payload");

    // Step 3: Confirm that the wrong password is rejected.
    let wrong_result = reader.extract_encrypted(a_entry, WRONG_PASSWORD);
    assert!(
        wrong_result.is_err(),
        "expected error extracting ZipCrypto entry with wrong password"
    );
}

/// Locate every local file header in a ZIP image and return the first
/// `take` bytes of each entry's raw (still encrypted) payload.
///
/// The scan is deliberately dumb — signature match, then the fixed 30-byte
/// local header layout — because the point is to inspect the bytes actually
/// written to disk rather than anything the reader re-derives.
fn raw_entry_payload_prefixes(archive: &[u8], take: usize) -> Vec<Vec<u8>> {
    const LOCAL_HEADER_SIG: [u8; 4] = [0x50, 0x4B, 0x03, 0x04];
    const LOCAL_HEADER_LEN: usize = 30;

    let mut prefixes = Vec::new();
    let mut index = 0usize;
    while index + LOCAL_HEADER_LEN <= archive.len() {
        if archive[index..index + 4] != LOCAL_HEADER_SIG {
            index += 1;
            continue;
        }
        let name_len = u16::from_le_bytes([archive[index + 26], archive[index + 27]]) as usize;
        let extra_len = u16::from_le_bytes([archive[index + 28], archive[index + 29]]) as usize;
        let data_start = index + LOCAL_HEADER_LEN + name_len + extra_len;
        let data_end = data_start + take;
        if data_end > archive.len() {
            break;
        }
        prefixes.push(archive[data_start..data_end].to_vec());
        index = data_start;
    }
    prefixes
}

/// Regression test: the ZipCrypto encryption header must come from the OS
/// CSPRNG, not from values that are stored in cleartext next to it.
///
/// `ZipWriter::add_encrypted_file_traditional` used to seed an LCG with
/// `mtime * 1000 + mdate` and `crc32 ^ compressed_len`. Every one of those
/// inputs is written into the local file header in the clear, so an attacker
/// could reconstruct the 11 "random" header bytes exactly — which is precisely
/// the known plaintext ZipCrypto's keystream must not leak. Worse, two entries
/// with identical content written in the same second produced byte-identical
/// encryption headers.
///
/// Writing eight identical entries in one pass makes that collision certain
/// under the old code and impossible under the fixed code.
#[test]
fn zipcrypto_header_is_csprng_sourced_not_derived_from_public_fields() {
    const ENTRY_COUNT: usize = 8;
    let payload = b"identical payload for every entry";

    let mut buf = Vec::new();
    {
        let mut writer = ZipWriter::new(&mut buf);
        for i in 0..ENTRY_COUNT {
            writer
                .add_encrypted_file_traditional(&format!("dup{i}.txt"), payload, PASSWORD)
                .expect("zipcrypto encrypt");
        }
        writer.finish().expect("finish");
    }

    // 12 bytes = the full ZipCrypto encryption header.
    let headers = raw_entry_payload_prefixes(&buf, 12);
    assert_eq!(headers.len(), ENTRY_COUNT, "expected one header per entry");

    let unique: std::collections::HashSet<_> = headers.iter().collect();
    assert_eq!(
        unique.len(),
        ENTRY_COUNT,
        "ZipCrypto encryption headers repeated across identical entries — \
         the random prefix is being derived from public header fields"
    );

    // The archive must still round-trip.
    let mut reader = ZipReader::new(Cursor::new(&buf)).expect("open archive");
    let entries: Vec<_> = reader.entries().to_vec();
    for entry in &entries {
        let data = reader
            .extract_encrypted(entry, PASSWORD)
            .expect("extract entry");
        assert_eq!(data.as_slice(), payload.as_slice());
    }
}

/// Regression test: WinZip-AES salts must be unique per entry.
///
/// The salt is the sole value that differentiates PBKDF2-SHA1 key derivation
/// between entries; a repeat means keystream reuse under AES-CTR. The salt is
/// stored in the clear as the first `salt_len` bytes of the entry payload, so
/// it can be read straight out of the archive image.
#[test]
fn aes_salts_are_unique_across_identical_entries() {
    const ENTRY_COUNT: usize = 8;
    // AES-256 (the default for `add_encrypted_file`) uses a 16-byte salt.
    const SALT_LEN: usize = 16;
    let payload = b"identical payload for every entry";

    let mut buf = Vec::new();
    {
        let mut writer = ZipWriter::new(&mut buf);
        for i in 0..ENTRY_COUNT {
            writer
                .add_encrypted_file(&format!("dup{i}.txt"), payload, PASSWORD)
                .expect("aes encrypt");
        }
        writer.finish().expect("finish");
    }

    let salts = raw_entry_payload_prefixes(&buf, SALT_LEN);
    assert_eq!(salts.len(), ENTRY_COUNT, "expected one salt per entry");

    let unique: std::collections::HashSet<_> = salts.iter().collect();
    assert_eq!(
        unique.len(),
        ENTRY_COUNT,
        "WinZip-AES salts repeated across entries — key derivation is not unique"
    );
    for salt in &salts {
        assert!(
            salt.iter().any(|&byte| byte != salt[0]),
            "salt must not be a constant byte pattern"
        );
    }

    let mut reader = ZipReader::new(Cursor::new(&buf)).expect("open archive");
    let entries: Vec<_> = reader.entries().to_vec();
    for entry in &entries {
        let data = reader
            .extract_encrypted(entry, PASSWORD)
            .expect("extract entry");
        assert_eq!(data.as_slice(), payload.as_slice());
    }
}
