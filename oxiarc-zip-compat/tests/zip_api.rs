//! zip-shaped API behaviour: candle-core's npz/pth call patterns,
//! ndarray-npy's writer pattern, archives produced by CPython's `zipfile`
//! and Info-ZIP (`tests/data/`), and our archives checked by `unzip -t` /
//! `zipfile` (self-skipping when the tools are absent).

use std::io::{BufReader, Cursor, Read, Seek, Write};
use std::process::Command;

use oxiarc_zip_compat::result::ZipError;
use oxiarc_zip_compat::write::{FileOptions, SimpleFileOptions};
use oxiarc_zip_compat::{CompressionMethod, DateTime, ZipArchive, ZipWriter};

type TestResult = Result<(), Box<dyn std::error::Error>>;

const PYTHON_ZIP: &[u8] = include_bytes!("data/python.zip");
const INFO_ZIP: &[u8] = include_bytes!("data/info-zip.zip");

fn temp_path(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("oxiarc-zip-compat-{}-{name}", std::process::id()))
}

/// candle-core `Tensor::write_npz`: `FileOptions<()>` + Stored, entries
/// written through `&mut zip`, archive finalized by drop (no `finish`).
#[test]
fn candle_write_npz_then_read_npz() -> TestResult {
    let path = temp_path("candle.npz");
    let arrays: Vec<(String, Vec<u8>)> = (0..5)
        .map(|i| (format!("t{i}"), vec![i as u8; 1000 + i * 333]))
        .collect();
    {
        let mut zip = ZipWriter::new(std::fs::File::create(&path)?);
        let options: FileOptions<()> =
            FileOptions::default().compression_method(CompressionMethod::Stored);
        for (name, data) in arrays.iter() {
            zip.start_file(format!("{name}.npy"), options)?;
            // candle passes `&mut zip` as the `Write` (`tensor.write(&mut zip)`).
            Write::write_all(&mut zip, data)?;
        }
    }

    // `Tensor::read_npz`: by_index + name() + Read.
    let mut zip = ZipArchive::new(BufReader::new(std::fs::File::open(&path)?))?;
    assert_eq!(zip.len(), arrays.len());
    for (i, expected) in arrays.iter().enumerate() {
        let mut reader = zip.by_index(i)?;
        let name = {
            let name = reader.name();
            name.strip_suffix(".npy").unwrap_or(name).to_owned()
        };
        assert_eq!(reader.compression(), CompressionMethod::Stored);
        let mut data = Vec::new();
        reader.read_to_end(&mut data)?;
        assert_eq!((name, data), expected.clone());
    }
    // `read_npz_by_name` miss path.
    assert!(matches!(
        zip.by_name("missing.npy"),
        Err(ZipError::FileNotFound)
    ));
    std::fs::remove_file(&path)?;
    Ok(())
}

/// candle-core `read_pth_tensor_info` / `PthTensors::get`: `file_names()`,
/// `by_name`, `BufReader` over the entry, and `by_ref().take()` skipping.
#[test]
fn candle_pth_access_pattern() -> TestResult {
    let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
    let deflated = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    zip.start_file("archive/data.pkl", deflated)?;
    zip.write_all(b"\x80\x02pickle-bytes")?;
    zip.start_file("archive/data/0", deflated)?;
    let tensor: Vec<u8> = (0..40_000u32).flat_map(|v| v.to_le_bytes()).collect();
    zip.write_all(&tensor)?;
    let bytes = zip.finish()?.into_inner();

    let mut zip = ZipArchive::new(BufReader::new(Cursor::new(bytes)))?;
    let names: Vec<String> = zip.file_names().map(|f| f.to_string()).collect();
    assert_eq!(names, ["archive/data.pkl", "archive/data/0"]);
    let reader = zip.by_name("archive/data.pkl")?;
    let mut pkl = Vec::new();
    BufReader::new(reader).read_to_end(&mut pkl)?;
    assert_eq!(pkl, b"\x80\x02pickle-bytes");

    let mut reader = zip.by_name("archive/data/0")?;
    assert_eq!(reader.compression(), CompressionMethod::Deflated);
    std::io::copy(&mut reader.by_ref().take(16), &mut std::io::sink())?;
    let mut rest = Vec::new();
    reader.read_to_end(&mut rest)?;
    assert_eq!(rest, &tensor[16..]);
    Ok(())
}

/// ndarray-npy `NpzWriter`: `SimpleFileOptions`, generic `FileOptions<'_, U>`,
/// `BufWriter::new(&mut zip)`, explicit `finish()`.
#[test]
fn ndarray_npy_writer_pattern() -> TestResult {
    fn add<W: Write + Seek, U: oxiarc_zip_compat::write::FileOptionExtension>(
        zip: &mut ZipWriter<W>,
        name: String,
        data: &[u8],
        options: FileOptions<'_, U>,
    ) -> Result<(), ZipError> {
        zip.start_file(name + ".npy", options)?;
        let mut w = std::io::BufWriter::new(zip);
        w.write_all(data)?;
        w.flush()?;
        Ok(())
    }
    let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    add(&mut zip, "a".into(), &[1u8; 5000], options)?;
    add(&mut zip, "b".into(), &[2u8; 7], options)?;
    let mut writer = zip.finish()?;
    writer.flush()?;
    let mut archive = ZipArchive::new(Cursor::new(writer.into_inner()))?;
    let mut b = Vec::new();
    archive.by_name("b.npy")?.read_to_end(&mut b)?;
    assert_eq!(b, [2u8; 7]);
    Ok(())
}

#[test]
fn reads_python_zipfile_archive() -> TestResult {
    let mut zip = ZipArchive::new(Cursor::new(PYTHON_ZIP))?;
    assert_eq!(zip.len(), 3);
    {
        let dir = zip.by_index(0)?;
        assert!(dir.is_dir());
        assert_eq!(dir.name(), "dir/");
    }
    let mut stored = zip.by_name("dir/stored.txt")?;
    assert_eq!(stored.compression(), CompressionMethod::Stored);
    let dt = stored.last_modified().ok_or("mtime")?;
    assert_eq!((dt.year(), dt.month(), dt.day()), (2020, 5, 17));
    let mut text = String::new();
    stored.read_to_string(&mut text)?;
    assert_eq!(text, "stored payload\n".repeat(10));
    drop(stored);
    let mut deflated = zip.by_name("deflated.bin")?;
    assert_eq!(deflated.compression(), CompressionMethod::Deflated);
    assert_eq!(deflated.unix_mode().map(|m| m & 0o777), Some(0o755));
    let mut data = Vec::new();
    deflated.read_to_end(&mut data)?;
    assert_eq!(data, (0..=255u8).collect::<Vec<_>>().repeat(40));
    Ok(())
}

#[test]
fn reads_info_zip_archive_and_detects_corruption() -> TestResult {
    let mut zip = ZipArchive::new(Cursor::new(INFO_ZIP))?;
    let mut text = String::new();
    zip.by_name("a.txt")?.read_to_string(&mut text)?;
    assert_eq!(text, "hello from info-zip\n".repeat(50));

    // Flip a byte of the stored/deflated payload: reading must fail (CRC
    // or DEFLATE error), never return wrong data silently.
    let data_start = zip.by_name("a.txt")?.data_start() as usize;
    let mut bad = INFO_ZIP.to_vec();
    bad[data_start + 3] ^= 0x40;
    if let Ok(mut zip) = ZipArchive::new(Cursor::new(bad)) {
        let mut out = Vec::new();
        let res = zip.by_name("a.txt").map(|mut f| f.read_to_end(&mut out));
        assert!(
            matches!(res, Ok(Err(_)) | Err(_)),
            "corruption went unnoticed"
        );
    }
    Ok(())
}

#[test]
fn writer_rejects_duplicates_and_unsupported_methods() -> TestResult {
    let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
    zip.start_file("x", SimpleFileOptions::default())?;
    assert!(zip.start_file("x", SimpleFileOptions::default()).is_err());
    let zstd = SimpleFileOptions::default().compression_method(CompressionMethod::Zstd);
    assert!(matches!(
        zip.start_file("y", zstd),
        Err(ZipError::UnsupportedArchive(_))
    ));
    let mut no_file = ZipWriter::new(Cursor::new(Vec::new()));
    assert!(no_file.write_all(b"data").is_err());
    Ok(())
}

#[test]
fn metadata_roundtrip_and_system_tools() -> TestResult {
    let path = temp_path("tools.zip");
    let when = DateTime::from_date_and_time(2022, 12, 31, 23, 59, 58).map_err(|e| e.to_string())?;
    {
        let mut zip = ZipWriter::new(std::fs::File::create(&path)?);
        let options = SimpleFileOptions::default()
            .last_modified_time(when)
            .unix_permissions(0o750);
        zip.add_directory("sub", options)?;
        zip.start_file("sub/script.sh", options)?;
        zip.write_all(&b"#!/bin/sh\necho hi\n".repeat(100))?;
        zip.start_file(
            "lzma.txt",
            options.compression_method(CompressionMethod::Lzma),
        )?;
        zip.write_all(&b"lzma entry ".repeat(300))?;
        zip.finish()?;
    }
    let mut zip = ZipArchive::new(std::fs::File::open(&path)?)?;
    {
        let f = zip.by_name("sub/script.sh")?;
        assert_eq!(f.last_modified(), Some(when));
        assert_eq!(f.unix_mode().map(|m| m & 0o777), Some(0o750));
    }
    let mut lz = String::new();
    zip.by_name("lzma.txt")?.read_to_string(&mut lz)?;
    assert_eq!(lz, "lzma entry ".repeat(300));

    let out_dir = temp_path("extract");
    zip.extract(&out_dir)?;
    assert_eq!(
        std::fs::read(out_dir.join("sub/script.sh"))?,
        b"#!/bin/sh\necho hi\n".repeat(100)
    );
    std::fs::remove_dir_all(&out_dir)?;

    // Info-ZIP's unzip has no LZMA (method 14) support; test the rest.
    match Command::new("unzip")
        .arg("-tq")
        .arg(&path)
        .args(["-x", "lzma.txt"])
        .output()
    {
        Ok(out) => assert!(
            out.status.success(),
            "unzip -t failed: {}",
            String::from_utf8_lossy(&out.stdout)
        ),
        Err(_) => eprintln!("unzip not available; skipping"),
    }
    let check = "import sys,zipfile; z=zipfile.ZipFile(sys.argv[1]); \
                 assert z.testzip() is None; \
                 assert z.getinfo('sub/script.sh').date_time==(2022,12,31,23,59,58)";
    match Command::new("python3")
        .args(["-c", check])
        .arg(&path)
        .output()
    {
        Ok(out) => assert!(
            out.status.success(),
            "python zipfile failed: {}",
            String::from_utf8_lossy(&out.stderr)
        ),
        Err(_) => eprintln!("python3 not available; skipping"),
    }
    std::fs::remove_file(&path)?;
    Ok(())
}

#[test]
fn enclosed_name_rejects_traversal() -> TestResult {
    let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
    zip.start_file("../evil", SimpleFileOptions::default())?;
    zip.write_all(b"x")?;
    zip.start_file("ok/../fine", SimpleFileOptions::default())?;
    zip.write_all(b"y")?;
    let bytes = zip.finish()?.into_inner();
    let mut archive = ZipArchive::new(Cursor::new(bytes))?;
    assert!(archive.by_index(0)?.enclosed_name().is_none());
    assert!(archive.by_index(1)?.enclosed_name().is_some());
    Ok(())
}
