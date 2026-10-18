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

/// A sink that records the size of every `write` it receives, so the number
/// of calls it takes to get a header onto the wire is an assertion rather
/// than a guess.
#[derive(Default)]
struct CallCounter {
    calls: Vec<usize>,
}

impl Write for CallCounter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.calls.push(buf.len());
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Headers must reach the sink as whole records, not field by field.
///
/// Both the local file header and the central directory entry used to be
/// emitted one `write` per integer field — eleven and seventeen writes of two
/// to four bytes respectively, thirty calls per entry before any payload.
/// That is invisible behind a `BufWriter`, and expensive behind anything that
/// charges per call: a file, a socket, or a JNI channel that crosses into
/// another language and allocates a byte array for every one of them.
///
/// Asserted as a per-entry bound rather than an exact count, so an
/// implementation that coalesces differently is still accepted; the previous
/// behaviour blew past it by more than four times.
#[test]
fn zip_entry_headers_reach_the_sink_as_whole_records() {
    const ENTRIES: usize = 200;
    // Local header (1) + name (1) + payload (1) + descriptor (1) + central
    // directory entry (1) + name (1), plus the archive's own trailer.
    const MAX_CALLS_PER_ENTRY: usize = 8;

    let data = payload(512);
    let mut sink = CallCounter::default();
    {
        let mut writer = ZipWriter::new(&mut sink);
        for i in 0..ENTRIES {
            {
                let mut entry = writer
                    .add_stream(&format!("entry-{i}.bin"))
                    .expect("add_stream");
                entry.write_all(&data).expect("stream write");
                entry.finish().expect("finish entry");
            }
        }
        writer.finish().expect("finish archive");
    }

    assert!(
        sink.calls.len() <= ENTRIES * MAX_CALLS_PER_ENTRY,
        "{} entries cost {} writes to the sink ({:.1} per entry), more than the {MAX_CALLS_PER_ENTRY} \
         the headers allow; the fixed parts of the local header and the central directory entry are \
         meant to be assembled and written once each",
        ENTRIES,
        sink.calls.len(),
        sink.calls.len() as f64 / ENTRIES as f64,
    );
}

/// The same bound must hold for an archive of many tiny entries, where the
/// per-entry cost is everything there is.
#[test]
fn zip_many_empty_entries_stay_cheap_to_sink() {
    const ENTRIES: usize = 500;
    const MAX_CALLS_PER_ENTRY: usize = 8;

    let mut sink = CallCounter::default();
    {
        let mut writer = ZipWriter::new(&mut sink);
        for i in 0..ENTRIES {
            let mut entry = writer.add_stream(&format!("e{i}")).expect("add_stream");
            entry.finish().expect("finish entry");
        }
        writer.finish().expect("finish archive");
    }

    assert!(
        sink.calls.len() <= ENTRIES * MAX_CALLS_PER_ENTRY,
        "{} empty entries cost {} writes ({:.1} per entry)",
        ENTRIES,
        sink.calls.len(),
        sink.calls.len() as f64 / ENTRIES as f64,
    );
}
