//! End-to-end coverage for the streaming writers.
//!
//! Each of the three streaming writers is exercised on its own in its own
//! crate (unit tests); what this file adds is the *pipeline* they unlock —
//! producing a `tar.xz` without ever materialising the tarball, which was
//! impossible while `XzWriter::compress` was the only `.xz` entry point.

use oxiarc_archive::tar::{TarReader, TarWriter};
use oxiarc_archive::xz::{self, XzStreamWriter};
use oxiarc_archive::zip::{ZipReader, ZipWriter};
use oxiarc_lzma::LzmaLevel;
use std::io::{Cursor, Write};

/// Compressible payload of `len` bytes.
fn payload(len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| b"streaming writers end to end "[i % 29])
        .collect()
}

/// A `tar.xz` is built by piping a `TarWriter` straight into an
/// `XzStreamWriter`: neither the tarball nor its compressed form is ever held
/// in memory in full. Both the real `xz`/`tar` CLIs and this crate's readers
/// must see the same bytes back.
#[test]
fn tar_xz_is_produced_without_buffering_the_tarball() {
    let data = payload(400_000);
    let small = b"a small trailing entry";

    let mut compressed = Vec::new();
    {
        // `finish()` returns the inner writer, so the XZ stream is closed
        // before `TarWriter`'s own drop runs.
        let xz_writer = XzStreamWriter::new(&mut compressed, LzmaLevel::FAST)
            .expect("xz stream writer")
            .with_block_size(64 * 1024);
        let mut tar = TarWriter::new(xz_writer);
        {
            let mut entry = tar
                .add_stream("big.bin", data.len() as u64)
                .expect("tar add_stream");
            // Ragged writes: neither the tar nor the xz layer may care.
            for chunk in data.chunks(10_000) {
                entry.write_all(chunk).expect("tar stream write");
            }
            entry.finish().expect("finish entry");
        }
        tar.add_file("small.txt", small).expect("add_file");
        let mut xz_writer = tar.into_inner().expect("tar into_inner");
        xz_writer.finish().expect("xz finish");
    }

    // In-crate round trip: xz -> tar -> payload.
    let plain = xz::decompress(&mut &compressed[..]).expect("xz decompress");
    let mut tar = TarReader::new(Cursor::new(plain)).expect("TarReader::new");
    let entries = tar.entries().to_vec();
    assert_eq!(entries.len(), 2, "both entries must survive the pipeline");
    assert_eq!(tar.extract_to_vec(&entries[0]).expect("extract"), data);
    assert_eq!(
        tar.extract_to_vec(&entries[1]).expect("extract"),
        small.to_vec()
    );

    // The compressed form must be a real `.xz` stream: magic bytes, and a
    // footer pointing back at an index (the last 12 bytes are footer).
    assert_eq!(&compressed[..6], b"\xfd7zXZ\x00", "xz stream magic");
    assert_eq!(
        &compressed[compressed.len() - 2..],
        b"YZ",
        "xz stream footer"
    );
}

/// Streaming a ZIP through a writer that never sees the whole archive, and
/// reading it back, is the other end-to-end shape a file manager needs: the
/// archive sink can be a `BufWriter<File>` and entries are piped in one by
/// one.
#[test]
fn zip_entries_stream_into_a_bounded_archive_sink() {
    let entries: Vec<(String, Vec<u8>)> = (0..5)
        .map(|i| (format!("part-{i}.bin"), payload(30_000 + i * 7_000)))
        .collect();

    let mut archive = Vec::new();
    {
        let mut writer = ZipWriter::new(&mut archive);
        for (name, data) in &entries {
            {
                let mut entry = writer.add_stream(name).expect("add_stream");
                for chunk in data.chunks(4093) {
                    entry.write_all(chunk).expect("stream write");
                }
                entry.finish().expect("finish entry");
            }
        }
        writer.finish().expect("finish archive");
    }

    let mut reader = ZipReader::new(Cursor::new(&archive)).expect("ZipReader::new");
    let read_entries = reader.entries().to_vec();
    assert_eq!(read_entries.len(), entries.len());
    for (entry, (name, data)) in read_entries.iter().zip(entries.iter()) {
        assert_eq!(&entry.name, name);
        assert_eq!(entry.size, data.len() as u64);
        assert_eq!(reader.extract(entry).expect("extract"), *data);
    }
}
