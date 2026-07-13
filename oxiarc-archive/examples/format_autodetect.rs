//! Auto-detect archive/compression formats from magic bytes using
//! [`ArchiveFormat::detect`].
//!
//! Builds one small sample for several supported formats and shows how
//! `ArchiveFormat::detect` classifies each from a `Read + Seek` source,
//! together with the format's canonical extension and MIME type.
//!
//! Run with:
//! ```sh
//! cargo run -p oxiarc-archive --example format_autodetect
//! ```

use oxiarc_archive::detect::ArchiveFormat;
use oxiarc_archive::tar::TarWriter;
use oxiarc_archive::zip::ZipWriter;
use std::io::Cursor;

fn detect(label: &str, bytes: Vec<u8>) {
    let mut cursor = Cursor::new(bytes);
    let (format, _magic) = ArchiveFormat::detect(&mut cursor).expect("detect");
    println!(
        "{label:<20} -> {format:<10} (.{:<5} {})",
        format.extension(),
        format.mime_type()
    );
}

fn main() {
    // ZIP
    let mut zip_bytes = Vec::new();
    {
        let mut writer = ZipWriter::new(&mut zip_bytes);
        writer.add_file("a.txt", b"hi").expect("add_file");
        writer.finish().expect("finish");
    }
    detect("ZIP", zip_bytes);

    // TAR
    let mut tar_bytes = Vec::new();
    {
        let mut writer = TarWriter::new(&mut tar_bytes);
        writer.add_file("a.txt", b"hi").expect("add_file");
        writer.finish().expect("finish");
    }
    detect("TAR", tar_bytes);

    // GZIP
    let gzip_bytes = oxiarc_deflate::gzip_compress(b"hello gzip", 6).expect("gzip_compress");
    detect("GZIP", gzip_bytes);

    // XZ
    let xz_bytes = oxiarc_archive::xz::compress(b"hello xz", 6).expect("xz compress");
    detect("XZ", xz_bytes);

    // Bzip2
    let bz2_bytes = oxiarc_bzip2::compress(b"hello bzip2", oxiarc_bzip2::CompressionLevel::new(3))
        .expect("bzip2 compress");
    detect("Bzip2", bz2_bytes);

    // Zstandard
    let zstd_bytes = oxiarc_zstd::compress_with_level(b"hello zstd", 3).expect("zstd compress");
    detect("Zstandard", zstd_bytes);

    // LZ4 (frame format carries the detectable magic)
    let lz4_bytes = oxiarc_lz4::compress(b"hello lz4").expect("lz4 compress");
    detect("LZ4", lz4_bytes);

    // Snappy framed
    let snappy_bytes = oxiarc_archive::snappy::compress(b"hello snappy").expect("snappy compress");
    detect("Snappy", snappy_bytes);

    // Unrecognized data
    detect("(random bytes)", vec![0xDE, 0xAD, 0xBE, 0xEF, 0x00, 0x01]);
}
