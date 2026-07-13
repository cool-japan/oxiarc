//! TAR-01 regression gate: GNU old-format sparse (typeflag `'S'`) entries
//! must decode to the correct *logical* content in both the seekable
//! `TarReader` and the streaming `TarStreamReader` — never to the raw
//! stored runs with a success return.
//!
//! `data/tar_gnu_sparse.tar` is a byte-crafted GNU old-format sparse
//! archive (realsize 16384; runs (0,100), (500,200), (4000,50),
//! (10000,250); payload byte `i % 251` in stored order) that was verified
//! externally: `bsdtar -xf` (libarchive) extracts exactly the materialized
//! content asserted below.

use oxiarc_archive::{TarReader, TarStreamReader};
use std::io::{Cursor, Read};

const TAR_GNU_SPARSE: &[u8] = include_bytes!("data/tar_gnu_sparse.tar");

const REALSIZE: usize = 16_384;
const RUNS: [(usize, usize); 4] = [(0, 100), (500, 200), (4_000, 50), (10_000, 250)];

fn expected_content() -> Vec<u8> {
    let mut out = vec![0u8; REALSIZE];
    let mut cursor = 0usize;
    for (off, len) in RUNS {
        for i in 0..len {
            out[off + i] = ((cursor + i) % 251) as u8;
        }
        cursor += len;
    }
    out
}

#[test]
fn stream_reader_decodes_gnu_sparse_fixture() {
    let mut stream = TarStreamReader::new(Cursor::new(TAR_GNU_SPARSE));
    let mut entry = stream
        .next_entry()
        .expect("next_entry")
        .expect("sparse entry present");
    assert_eq!(entry.header.name, "sp.bin");
    assert_eq!(
        entry.header.size, REALSIZE as u64,
        "entry must report realsize, not the 600-byte stored size"
    );

    let mut content = Vec::new();
    entry.read_to_end(&mut content).expect("read sparse entry");
    drop(entry);
    assert!(stream.next_entry().expect("final").is_none());

    assert_eq!(
        content,
        expected_content(),
        "content must match bsdtar's output"
    );
}

#[test]
fn seekable_reader_agrees_with_stream_reader_on_sparse_fixture() {
    let mut reader = TarReader::new(Cursor::new(TAR_GNU_SPARSE)).expect("TarReader::new");
    assert_eq!(reader.entries().len(), 1);
    assert_eq!(reader.entries()[0].size, REALSIZE as u64);

    let seekable = reader
        .extract_by_name("sp.bin")
        .expect("extract_by_name")
        .expect("entry present");
    assert_eq!(seekable, expected_content());
}
