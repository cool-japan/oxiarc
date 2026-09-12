//! End-to-end tests for the CLI-layer image detection fallback: `oxiarc
//! detect`/`oxiarc info` recognise PNG/JPEG/TIFF by magic when
//! `ArchiveFormat` reports `Unknown`, and every other subcommand still
//! refuses an image with a clear "not an archive" error rather than a bare
//! "unrecognized format" message.
//!
//! Fixtures are generated programmatically with each format's own oxiarc
//! encoder (`CONTRIBUTING.md`'s "generate fixtures, don't commit binary
//! blobs" policy) and written under `std::env::temp_dir()`.

use std::path::PathBuf;
use std::process::Command;

fn cli_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_oxiarc"))
}

fn fixture_path(tag: &str, ext: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "oxiarc_cli_image_detect_{tag}_{ext}_{}.{ext}",
        std::process::id()
    ))
}

/// A tiny 4x3 RGB8 PNG.
fn write_png_fixture(tag: &str) -> PathBuf {
    use oxiarc_png::{BitDepth, ColorType, Encoder};

    let (width, height) = (4u32, 3u32);
    let pixels: Vec<u8> = (0..(width * height * 3) as usize)
        .map(|i| (i * 17) as u8)
        .collect();

    let mut bytes = Vec::new();
    {
        let mut enc = Encoder::new(&mut bytes, width, height);
        enc.set_color(ColorType::Rgb);
        enc.set_depth(BitDepth::Eight);
        let mut writer = enc.write_header().expect("png header");
        writer.write_image_data(&pixels).expect("png image data");
        writer.finish().expect("png finish");
    }

    let path = fixture_path(tag, "png");
    std::fs::write(&path, &bytes).expect("write png fixture");
    path
}

/// A tiny 8x8 RGB JPEG (one 8x8 block: the smallest non-degenerate baseline
/// frame).
fn write_jpeg_fixture(tag: &str) -> PathBuf {
    use oxiarc_jpeg::InputColor;

    let (width, height) = (8u16, 8u16);
    let pixels: Vec<u8> = (0..(width as usize * height as usize * 3))
        .map(|i| (i * 3) as u8)
        .collect();
    let bytes = oxiarc_jpeg::encode_to_vec(&pixels, width, height, InputColor::Rgb, 80)
        .expect("jpeg encode");

    let path = fixture_path(tag, "jpg");
    std::fs::write(&path, &bytes).expect("write jpeg fixture");
    path
}

/// A tiny 5x4 RGB8 uncompressed TIFF.
fn write_tiff_fixture(tag: &str) -> PathBuf {
    use oxiarc_tiff::{ColorType, Encoder, ImageSpec};
    use std::io::Cursor;

    let (width, height) = (5u32, 4u32);
    let pixels: Vec<u8> = (0..(width * height * 3) as usize)
        .map(|i| (i * 13) as u8)
        .collect();
    let spec = ImageSpec::new(width, height, ColorType::Rgb(8));

    let mut bytes = Vec::new();
    {
        let mut enc = Encoder::new(Cursor::new(&mut bytes)).expect("tiff encoder");
        enc.write_image(&spec, &pixels).expect("tiff write_image");
        let _ = enc.finish().expect("tiff finish");
    }

    let path = fixture_path(tag, "tif");
    std::fs::write(&path, &bytes).expect("write tiff fixture");
    path
}

fn run(args: &[&str]) -> std::process::Output {
    Command::new(cli_bin())
        .args(args)
        .output()
        .expect("run oxiarc")
}

// ─── detect / info succeed and report the right thing ──────────────────

#[test]
fn detect_recognises_png() {
    let path = write_png_fixture("detect");
    let output = run(&["detect", "--color=never", path.to_str().expect("utf8 path")]);
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("PNG image"), "stdout: {stdout}");
    assert!(stdout.contains("4x3"), "stdout: {stdout}");
    assert!(!stdout.contains("Archive"), "stdout: {stdout}");
    let _ = std::fs::remove_file(&path);
}

#[test]
fn detect_recognises_jpeg() {
    let path = write_jpeg_fixture("detect");
    let output = run(&["detect", "--color=never", path.to_str().expect("utf8 path")]);
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("JPEG image"), "stdout: {stdout}");
    assert!(stdout.contains("8x8"), "stdout: {stdout}");
    let _ = std::fs::remove_file(&path);
}

#[test]
fn detect_recognises_tiff() {
    let path = write_tiff_fixture("detect");
    let output = run(&["detect", "--color=never", path.to_str().expect("utf8 path")]);
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("TIFF image"), "stdout: {stdout}");
    assert!(stdout.contains("5x4"), "stdout: {stdout}");
    let _ = std::fs::remove_file(&path);
}

#[test]
fn info_reports_png_chunk_summary() {
    let path = write_png_fixture("info");
    let output = run(&["info", "--color=never", path.to_str().expect("utf8 path")]);
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("PNG image"), "stdout: {stdout}");
    assert!(
        stdout.contains("IHDR"),
        "stdout should list chunks: {stdout}"
    );
    assert!(
        stdout.contains("IDAT"),
        "stdout should list chunks: {stdout}"
    );
    assert!(
        stdout.contains("IEND"),
        "stdout should list chunks: {stdout}"
    );
    let _ = std::fs::remove_file(&path);
}

#[test]
fn info_reports_jpeg_segment_summary() {
    let path = write_jpeg_fixture("info");
    let output = run(&["info", "--color=never", path.to_str().expect("utf8 path")]);
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("JPEG image"), "stdout: {stdout}");
    assert!(
        stdout.contains("SOI"),
        "stdout should list segments: {stdout}"
    );
    assert!(
        stdout.contains("SOS"),
        "stdout should list segments: {stdout}"
    );
    let _ = std::fs::remove_file(&path);
}

#[test]
fn info_reports_tiff_ifd_summary() {
    let path = write_tiff_fixture("info");
    let output = run(&["info", "--color=never", path.to_str().expect("utf8 path")]);
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("TIFF image"), "stdout: {stdout}");
    assert!(
        stdout.contains("ImageWidth") || stdout.contains("Width"),
        "stdout should list IFD tags: {stdout}"
    );
    let _ = std::fs::remove_file(&path);
}

// ─── the non-image path must not regress ────────────────────────────────

/// `detect` on an unrecognised, non-image blob must still print the normal
/// `Unknown` report (extension / MIME / magic bytes) and exit 0 — the image
/// fallback is a fallback, not a replacement.
#[test]
fn detect_still_reports_unknown_for_a_non_image_blob() {
    // Deliberately free of the substring "image": the assertions below check
    // that the image hint is absent from the message, and the message quotes
    // the path.
    let path =
        std::env::temp_dir().join(format!("oxiarc_cli_plain_blob_{}.bin", std::process::id()));
    let blob: Vec<u8> = (0..4096u32).map(|i| (i % 251) as u8).collect();
    std::fs::write(&path, &blob).expect("write blob fixture");

    let output = run(&["detect", "--color=never", path.to_str().expect("utf8 path")]);
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Unknown"), "stdout: {stdout}");
    assert!(stdout.contains("MIME type"), "stdout: {stdout}");
    assert!(stdout.contains("Magic bytes"), "stdout: {stdout}");
    assert!(!stdout.contains("Image"), "stdout: {stdout}");

    // `info` on the same blob still fails loudly, and says nothing about
    // images.
    let output = run(&["info", "--color=never", path.to_str().expect("utf8 path")]);
    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("unrecognized"), "stderr: {stderr}");
    assert!(!stderr.contains("image"), "stderr: {stderr}");
    assert!(!stderr.contains("not an archive"), "stderr: {stderr}");

    // And `list` on it gets the plain error with no image hint appended.
    let output = run(&["list", "--color=never", path.to_str().expect("utf8 path")]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stderr.contains("not an archive"), "stderr: {stderr}");

    let _ = std::fs::remove_file(&path);
}

/// A file whose first bytes are an image magic but whose body is garbage is
/// still recognised by magic — `detect` reports the kind and warns instead of
/// crashing, and `info` fails cleanly (non-zero) rather than panicking.
#[test]
fn a_truncated_image_is_reported_not_crashed_on() {
    let full = std::fs::read(write_png_fixture("trunc")).expect("read png fixture");
    let path = std::env::temp_dir().join(format!(
        "oxiarc_cli_image_detect_trunc_{}.png",
        std::process::id()
    ));

    // Every prefix from "just the signature" to "one byte short of whole".
    for cut in 8..full.len() {
        std::fs::write(&path, &full[..cut]).expect("write truncated png");

        let output = run(&["detect", "--color=never", path.to_str().expect("utf8 path")]);
        assert!(
            output.status.success(),
            "detect on a {cut}-byte PNG prefix exited {:?}",
            output.status
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("PNG image"), "cut {cut}, stdout: {stdout}");

        let output = run(&["info", "--color=never", path.to_str().expect("utf8 path")]);
        // Success or a clean non-zero exit are both fine; a signal (a panic
        // aborts, an assert unwinds to a 101) is not.
        let code = output.status.code();
        assert!(
            code == Some(0) || code == Some(1),
            "info on a {cut}-byte PNG prefix exited {:?}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(fixture_path("trunc", "png"));
}

// ─── every other subcommand refuses an image, clearly ───────────────────

#[test]
fn list_refuses_an_image_with_a_clear_error() {
    let path = write_png_fixture("list");
    let output = run(&["list", "--color=never", path.to_str().expect("utf8 path")]);
    assert!(
        !output.status.success(),
        "oxiarc list on a PNG unexpectedly exited 0"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("PNG image") && stderr.contains("not an archive"),
        "expected a PNG-aware error, got: {stderr}"
    );
    let _ = std::fs::remove_file(&path);
}

#[test]
fn list_json_refuses_an_image_with_no_stdout_payload() {
    let path = write_png_fixture("list_json");
    let output = run(&[
        "list",
        "--json",
        "--color=never",
        path.to_str().expect("utf8 path"),
    ]);
    assert!(!output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.trim().is_empty(), "stdout: {stdout}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("PNG image"), "stderr: {stderr}");
    let _ = std::fs::remove_file(&path);
}

#[test]
fn test_command_refuses_an_image_with_a_clear_error() {
    let path = write_jpeg_fixture("test");
    let output = run(&["test", "--color=never", path.to_str().expect("utf8 path")]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("JPEG image") && stderr.contains("not an archive"),
        "expected a JPEG-aware error, got: {stderr}"
    );
    let _ = std::fs::remove_file(&path);
}

#[test]
fn extract_refuses_an_image_with_a_clear_error() {
    let path = write_tiff_fixture("extract");
    let out_dir = std::env::temp_dir().join(format!(
        "oxiarc_cli_image_detect_extract_out_{}",
        std::process::id()
    ));
    let output = run(&[
        "extract",
        "--color=never",
        "-o",
        out_dir.to_str().expect("utf8 path"),
        path.to_str().expect("utf8 path"),
    ]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("TIFF image") && stderr.contains("not an archive"),
        "expected a TIFF-aware error, got: {stderr}"
    );
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir_all(&out_dir);
}

#[test]
fn convert_refuses_an_image_with_a_clear_error() {
    let path = write_png_fixture("convert");
    let out_path = std::env::temp_dir().join(format!(
        "oxiarc_cli_image_detect_convert_out_{}.zip",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&out_path);

    let output = run(&[
        "convert",
        "--color=never",
        path.to_str().expect("utf8 path"),
        out_path.to_str().expect("utf8 path"),
    ]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("PNG image") && stderr.contains("not an archive"),
        "expected a PNG-aware error, got: {stderr}"
    );
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(&out_path);
}

#[test]
fn add_refuses_an_image_with_a_clear_error() {
    let path = write_png_fixture("add");
    let extra = std::env::temp_dir().join(format!(
        "oxiarc_cli_image_detect_add_extra_{}.txt",
        std::process::id()
    ));
    std::fs::write(&extra, b"hello").expect("write extra file");

    let output = run(&[
        "add",
        "--color=never",
        path.to_str().expect("utf8 path"),
        extra.to_str().expect("utf8 path"),
    ]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("PNG image") && stderr.contains("not an archive"),
        "expected a PNG-aware error, got: {stderr}"
    );
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(&extra);
}

/// FINALGATE F12: `oxiarc --help` listed only the archive formats, so a user
/// had no way to learn from the banner that `detect`/`info` also recognise
/// PNG, JPEG and TIFF — the feature this whole test file covers.
#[test]
fn help_banner_mentions_the_image_formats() {
    let output = run(&["--help"]);
    assert!(output.status.success(), "`oxiarc --help` failed");
    let stdout = String::from_utf8_lossy(&output.stdout);
    for needle in ["PNG", "JPEG", "TIFF", "detect", "info"] {
        assert!(
            stdout.contains(needle),
            "`oxiarc --help` never mentions {needle}:\n{stdout}"
        );
    }
}
