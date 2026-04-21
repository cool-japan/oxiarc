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
