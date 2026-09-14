//! tar-shaped API behaviour: tract-nnef's exact write/read pattern
//! (including gzip'd archives via oxiarc-flate2-compat), archives from
//! CPython's `tarfile` (`tests/data/`), header byte layout against
//! `tarfile`, and our archives checked by GNU `tar` (self-skipping).

use std::io::{Cursor, Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};

use oxiarc_tar_compat::{Archive, Builder, EntryType, Header};

type TestResult = Result<(), Box<dyn std::error::Error>>;

const PY_GNU: &[u8] = include_bytes!("data/python_gnu.tar");
const PY_PAX: &[u8] = include_bytes!("data/python_pax.tar");

/// Build an archive exactly the way `tract_nnef::framework::Nnef` does.
fn tract_style_archive<W: Write>(w: W, timestamp: u64) -> std::io::Result<W> {
    let mut ar = Builder::new(w);
    let graph_data = b"version 1.0;\ngraph g(input) -> (output) { }\n".to_vec();
    let mut header = Header::new_gnu();
    header.set_path("graph.nnef")?;
    header.set_size(graph_data.len() as u64);
    header.set_mode(0o644);
    header.set_mtime(timestamp);
    header.set_cksum();
    ar.append(&header, &mut &*graph_data)?;

    let quant_data = b"\"x\": zero_point_linear_quantize(zero_point = 0);\n".to_vec();
    header.set_path("graph.quant")?;
    header.set_size(quant_data.len() as u64);
    header.set_mode(0o644);
    header.set_mtime(timestamp);
    header.set_cksum();
    ar.append(&header, &mut &*quant_data)?;

    for (label, len) in [("weights/conv1", 3000usize), (&"w".repeat(130), 17)] {
        let mut label = label.to_string() + ".dat";
        if label.starts_with('/') {
            label.insert(0, '.');
        }
        let data: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
        let mut header = Header::new_gnu();
        header.set_size(data.len() as u64);
        header.set_mode(0o644);
        header.set_mtime(timestamp);
        header.set_cksum();
        ar.append_data(&mut header, std::path::Path::new(&label), &mut &*data)?;
    }
    ar.into_inner()
}

/// `tract_nnef::framework::Nnef::proto_model_for_read`: sniff gzip, wrap in
/// `Box<dyn Read>`, iterate `entries()`, strip `./`, read each entry.
fn tract_style_read(bytes: &[u8]) -> std::io::Result<Vec<(PathBuf, Vec<u8>)>> {
    let mut reader: &[u8] = bytes;
    let mut buffer = vec![0u8; 2];
    reader.read_exact(&mut buffer)?;
    let header = Cursor::new(buffer.clone());
    let stream = header.chain(reader);
    let mut tar = if buffer == [0x1f, 0x8b] {
        let f = oxiarc_flate2_compat::read::GzDecoder::new(stream);
        Archive::new(Box::new(f) as Box<dyn Read>)
    } else {
        Archive::new(Box::new(stream) as Box<dyn Read>)
    };
    let mut out = Vec::new();
    for entry in tar.entries()? {
        let mut entry = entry?;
        let mut path = entry.path()?.to_path_buf();
        if path.starts_with("./") {
            path = path
                .strip_prefix("./")
                .map_err(|e| std::io::Error::other(e.to_string()))?
                .to_path_buf();
        }
        let mut data = Vec::new();
        entry.read_to_end(&mut data)?;
        out.push((path, data));
    }
    Ok(out)
}

#[test]
fn tract_roundtrip_plain_and_gzip() -> TestResult {
    let plain = tract_style_archive(Vec::new(), 1_700_000_000)?;
    assert_eq!(plain.len() % 512, 0);
    let entries = tract_style_read(&plain)?;
    let names: Vec<String> = entries
        .iter()
        .map(|(p, _)| p.to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        names,
        [
            "graph.nnef".to_string(),
            "graph.quant".to_string(),
            "weights/conv1.dat".to_string(),
            format!("{}.dat", "w".repeat(130)),
        ]
    );
    assert_eq!(entries[2].1.len(), 3000);

    // Nested .nnef.tgz written through a GzEncoder that is only dropped
    // (never finished), as tract does for compressed submodels.
    let mut tgz = Vec::new();
    {
        let encoder = oxiarc_flate2_compat::write::GzEncoder::new(
            &mut tgz,
            oxiarc_flate2_compat::Compression::default(),
        );
        let _encoder = tract_style_archive(encoder, 0)?;
    }
    let from_gz = tract_style_read(&tgz)?;
    assert_eq!(from_gz.len(), 4);
    assert_eq!(from_gz[0].1, entries[0].1);
    Ok(())
}

#[test]
fn partial_reads_skip_remaining_data() -> TestResult {
    let plain = tract_style_archive(Vec::new(), 0)?;
    let mut archive = Archive::new(&plain[..]);
    let mut seen = Vec::new();
    for entry in archive.entries()? {
        let mut entry = entry?;
        let mut first = [0u8; 3];
        let n = entry.read(&mut first)?;
        seen.push((entry.path()?.into_owned(), n));
    }
    assert_eq!(seen.len(), 4);
    Ok(())
}

#[test]
fn reads_python_gnu_and_pax_archives() -> TestResult {
    let mut archive = Archive::new(PY_GNU);
    let mut entries = archive.entries()?;
    let dir = entries.next().ok_or("dir")??;
    assert_eq!(dir.header().entry_type(), EntryType::Directory);
    assert_eq!(dir.header().mode()?, 0o755);
    drop(dir);
    let mut graph = entries.next().ok_or("graph")??;
    assert_eq!(graph.path()?.to_str(), Some("dir/graph.nnef"));
    assert_eq!(graph.header().mtime()?, 1_600_000_000);
    let mut text = String::new();
    graph.read_to_string(&mut text)?;
    assert_eq!(text, "graph data\n".repeat(30));
    drop(graph);
    let mut long = entries.next().ok_or("long")??;
    assert_eq!(
        long.path()?.to_string_lossy(),
        format!("{}tensor.dat", "deep/".repeat(30))
    );
    let mut data = Vec::new();
    long.read_to_end(&mut data)?;
    assert_eq!(data, (0..250u8).collect::<Vec<_>>().repeat(4));
    drop(long);
    let link = entries.next().ok_or("link")??;
    assert!(link.header().entry_type().is_symlink());
    assert_eq!(
        link.link_name()?.map(|p| p.into_owned()),
        Some(PathBuf::from("dir/graph.nnef"))
    );
    drop(link);
    assert!(entries.next().is_none());

    let mut pax = Archive::new(PY_PAX);
    let mut it = pax.entries()?;
    let mut e = it.next().ok_or("pax entry")??;
    assert_eq!(
        e.path()?.to_string_lossy(),
        format!("ü/{}.bin", "p".repeat(150))
    );
    let mut s = String::new();
    e.read_to_string(&mut s)?;
    assert_eq!(s, "hello");
    Ok(())
}

#[test]
fn corrupt_checksum_is_rejected() -> TestResult {
    let mut bytes = tract_style_archive(Vec::new(), 0)?;
    bytes[10] ^= 1; // inside the first header's name field
    let mut archive = Archive::new(&bytes[..]);
    let first = archive.entries()?.next().ok_or("entry")?;
    assert!(first.is_err());
    Ok(())
}

/// Header bytes match CPython's `tarfile` GNU encoding field-for-field (the
/// checksum field differs only in formatting, `%07o\0` vs `%06o\0 `, and
/// must agree numerically).
#[test]
fn header_bytes_match_python_tarfile() -> TestResult {
    let mut h = Header::new_gnu();
    h.set_path("model/graph.nnef")?;
    h.set_size(4321);
    h.set_mode(0o644);
    h.set_uid(1000);
    h.set_gid(100);
    h.set_mtime(1_700_000_123);
    h.set_entry_type(EntryType::Regular);
    h.set_cksum();
    let script = "import tarfile,sys\n\
        t=tarfile.TarInfo('model/graph.nnef'); t.size=4321; t.mode=0o644; t.uid=1000; t.gid=100\n\
        t.mtime=1700000123; t.type=tarfile.REGTYPE; t.uname=''; t.gname=''\n\
        sys.stdout.buffer.write(t.tobuf(tarfile.GNU_FORMAT))";
    let out = match Command::new("python3").args(["-c", script]).output() {
        Ok(out) if out.status.success() => out.stdout,
        _ => {
            eprintln!("python3 not available; skipping");
            return Ok(());
        }
    };
    assert_eq!(out.len(), 512);
    let ours = h.as_bytes();
    for i in 0..512 {
        if (148..156).contains(&i) || (329..345).contains(&i) {
            continue; // cksum formatting; python writes devmajor/minor digits
        }
        assert_eq!(ours[i], out[i], "byte {i} differs");
    }
    let theirs = std::str::from_utf8(&out[148..154])?;
    assert_eq!(u32::from_str_radix(theirs, 8)?, h.cksum()?);
    Ok(())
}

/// GNU tar lists and extracts our archive, including the `././@LongLink`
/// long name.
#[test]
fn gnu_tar_accepts_our_archive() -> TestResult {
    let plain = tract_style_archive(Vec::new(), 1_700_000_000)?;
    let child = Command::new("tar")
        .args(["-tvf", "-"])
        .env("TZ", "UTC")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let Ok(mut child) = child else {
        eprintln!("tar not available; skipping");
        return Ok(());
    };
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(&plain)?;
    }
    let out = child.wait_with_output()?;
    let listing = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "tar failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(listing.contains("graph.nnef"));
    assert!(listing.contains(&format!("{}.dat", "w".repeat(130))));
    assert!(listing.contains("2023-11-14"));
    Ok(())
}

#[test]
fn unpack_in_skips_traversal() -> TestResult {
    let mut raw = Vec::new();
    {
        let mut ar = Builder::new(&mut raw);
        let mut h = Header::new_gnu();
        h.set_size(2);
        h.set_mode(0o600);
        // Bypass set_path's validation to plant a malicious name.
        h.as_mut_bytes()[..9].copy_from_slice(b"../escape");
        h.set_cksum();
        ar.append(&h, &b"no"[..])?;
        let mut ok = Header::new_gnu();
        ok.set_size(3);
        ok.set_mode(0o640);
        ok.set_cksum();
        ar.append_data(&mut ok, "inside/file.txt", &b"yes"[..])?;
        ar.finish()?;
    }
    let dst = std::env::temp_dir().join(format!("oxiarc-tar-compat-{}", std::process::id()));
    let mut archive = Archive::new(&raw[..]);
    archive.unpack(&dst)?;
    assert_eq!(std::fs::read(dst.join("inside/file.txt"))?, b"yes");
    assert!(!dst.parent().ok_or("parent")?.join("escape").exists());
    std::fs::remove_dir_all(&dst)?;
    Ok(())
}
