use super::*;
use std::io::Cursor;

/// Build a TAR with a symlink entry, add another file via
/// `add_entry_from_header`, then verify the symlink target round-trips.
#[test]
fn test_tar_add_preserves_symlink() {
    let mut src_bytes = Vec::new();
    {
        let mut tw = TarWriter::new(&mut src_bytes);
        // Add a regular file
        tw.add_file("real.txt", b"symlink target content")
            .expect("add_file");
        // Add a symlink
        tw.add_symlink("link.txt", "real.txt").expect("add_symlink");
        tw.finish().expect("finish");
    }

    // Read back, then rewrite via add_entry_from_header
    let mut src_reader = TarReader::new(Cursor::new(&src_bytes)).expect("TarReader::new src");
    let entries = src_reader.entries().to_vec();
    assert_eq!(entries.len(), 2);

    // Gather (header, data) pairs
    let pairs: Vec<(TarHeader, Vec<u8>)> = entries
        .iter()
        .map(|e| {
            let hdr = src_reader.header_for(e).expect("header_for").clone();
            let data = if e.entry_type == EntryType::File {
                src_reader.extract_to_vec(e).expect("extract_to_vec")
            } else {
                Vec::new()
            };
            (hdr, data)
        })
        .collect();
    drop(src_reader);

    let mut dst_bytes = Vec::new();
    {
        let mut tw2 = TarWriter::new(&mut dst_bytes);
        // Append a new file as well
        tw2.add_file("new.txt", b"new content")
            .expect("add_file new");
        for (hdr, data) in &pairs {
            tw2.add_entry_from_header(hdr, data)
                .expect("add_entry_from_header");
        }
        tw2.finish().expect("finish dst");
    }

    // Verify symlink entry is preserved
    let dst_reader = TarReader::new(Cursor::new(&dst_bytes)).expect("TarReader::new dst");
    let dst_entries = dst_reader.entries().to_vec();

    let symlink_entry = dst_entries
        .iter()
        .find(|e| e.name == "link.txt")
        .expect("link.txt must be present");
    assert_eq!(symlink_entry.entry_type, EntryType::Symlink);
    let link_target = symlink_entry
        .link_target
        .as_ref()
        .expect("link_target must be Some");
    assert_eq!(
        link_target.to_string_lossy(),
        "real.txt",
        "symlink target must round-trip"
    );
}

/// Build a TAR with a file that has mode 0o755 and a non-default uname,
/// then verify both fields survive the add_entry_from_header round-trip.
#[test]
fn test_tar_add_preserves_mode_and_uname() {
    let mut src_bytes = Vec::new();
    {
        // Build a header manually with mode 0o755 and uname "alice"
        let mut header = TarHeader::new_file("script.sh", 5, 0o755);
        header.uname = "alice".to_string();
        header.gname = "staff".to_string();
        header.uid = 1234;
        header.gid = 5678;

        let mut tw = TarWriter::new(&mut src_bytes);
        tw.add_entry_from_header(&header, b"#!/sh")
            .expect("add_entry_from_header");
        tw.finish().expect("finish");
    }

    // Read back and verify
    let mut reader = TarReader::new(Cursor::new(&src_bytes)).expect("TarReader::new");
    let entries = reader.entries().to_vec();
    assert_eq!(entries.len(), 1);
    let entry = &entries[0];

    // Verify via header_for
    let hdr = reader.header_for(entry).expect("header_for");
    assert_eq!(hdr.mode, 0o755, "mode must be preserved");
    assert_eq!(hdr.uname, "alice", "uname must be preserved");
    assert_eq!(hdr.gname, "staff", "gname must be preserved");
    assert_eq!(hdr.uid, 1234, "uid must be preserved");
    assert_eq!(hdr.gid, 5678, "gid must be preserved");

    // Also verify the file content
    let data = reader.extract_to_vec(entry).expect("extract_to_vec");
    assert_eq!(data, b"#!/sh");
}
