//! GeoTIFF tag passthrough against an external oracle: a fixture written by
//! `tifffile` carries four GeoTIFF tags (33550 `ModelPixelScale`, 33922
//! `ModelTiepoint`, 34735 `GeoKeyDirectory`, 34737 `GeoAsciiParams`), our reader loads them via
//! [`Decoder::geo_tags`], our writer re-emits them via
//! [`GeoTags::to_extra_tags`], and `tiffinfo -D` -- an independent reader
//! this crate did not write -- must print byte-identical tag *values* for
//! both files.
//!
//! `tests/roundtrip.rs`'s `arbitrary_tags_and_metadata_blobs_round_trip_byte_identically`
//! already proves the internal round trip (our writer -> our reader); this
//! test is the external half: proof that libtiff's own tag parser agrees
//! the values we wrote are the values that were there, not merely that we
//! agree with ourselves.

#![cfg(feature = "tiff-oracle")]

mod oracle_support;

use oracle_support::{find_tool, scratch_dir};
use oxiarc_tiff::{ColorType, Decoder, Encoder, ImageSpec};
use std::fs;
use std::io::Cursor;
use std::process::Command;

/// The `tiffinfo -D` lines this test cares about: one per GeoTIFF tag.
/// `tiffinfo` does not know these tags by name (they are outside its own
/// registry), so it prints them as `Tag <number>: <value>` -- which is
/// exactly the line a byte-for-byte value comparison wants, independent of
/// any GeoTIFF-specific formatting either side might apply.
const GEOTIFF_TAGS: [u16; 4] = [33550, 33922, 34735, 34737];

fn geotiff_lines(tiffinfo_output: &str) -> Vec<(u16, String)> {
    let mut lines = Vec::new();
    for tag in GEOTIFF_TAGS {
        let prefix = format!("Tag {tag}:");
        if let Some(line) = tiffinfo_output
            .lines()
            .find(|line| line.trim_start().starts_with(&prefix))
        {
            lines.push((tag, line.trim().to_string()));
        }
    }
    lines
}

#[test]
fn geotiff_tags_survive_tifffile_to_oxiarc_to_tiffinfo() {
    let Some(python) = oracle_support::find_python() else {
        eprintln!("skipping: python3 with tifffile is not available");
        return;
    };
    let has_tifffile = Command::new(&python)
        .args(["-c", "import tifffile"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !has_tifffile {
        eprintln!("skipping: python3 tifffile is not available");
        return;
    }
    let Some(tiffinfo) = find_tool("tiffinfo") else {
        eprintln!("skipping: libtiff's tiffinfo is not on PATH");
        return;
    };

    let dir = scratch_dir("geotiff_passthrough");
    let source = dir.join("source.tif");

    // A source fixture only `tifffile` can write here (arbitrary numbered
    // extra tags): the four GeoTIFF tags above, with real-looking values
    // -- not all-zero, so a transcription bug (byte order, count, a
    // truncated string) has something to actually get wrong.
    let script = format!(
        r#"
import tifffile
import numpy as np
pixels = np.arange(64, dtype=np.uint8).reshape(8, 8)
tifffile.imwrite(
    {source:?},
    pixels,
    extratags=[
        (33550, "d", 3, (10.0, 10.0, 0.0), False),
        (33922, "d", 6, (0.0, 0.0, 0.0, 500000.0, 4000000.0, 0.0), False),
        (34735, "H", 16, (1, 1, 0, 4, 1024, 0, 1, 2, 1025, 0, 1, 1, 3072, 0, 1, 32767), False),
        (34737, "s", 16, "WGS 84|Unknown|", False),
    ],
)
"#,
        source = source.display(),
    );
    let output = Command::new(&python)
        .arg("-c")
        .arg(&script)
        .output()
        .expect("spawn python");
    assert!(
        output.status.success(),
        "tifffile fixture write failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Read every GeoTIFF tag with our decoder, exactly as a downstream
    // consumer (oxigeo) would.
    let source_bytes = fs::read(&source).expect("read source fixture");
    let mut decoder = Decoder::new(Cursor::new(source_bytes)).expect("decoder");
    let geo = decoder.geo_tags().expect("geo tags");
    assert!(
        !geo.is_empty(),
        "the fixture's GeoTIFF tags did not round-trip through our own reader"
    );
    let extra_tags = geo.to_extra_tags();
    assert_eq!(
        extra_tags.len(),
        4,
        "expected all four written GeoTIFF tags back: {extra_tags:?}"
    );

    // Re-emit them through our writer's generic `extra_tags` mechanism --
    // the same path any caller uses for arbitrary tags, not a GeoTIFF
    // special case.
    let (width, height) = decoder.dimensions().expect("dimensions");
    let pixels = decoder.read_image().expect("read pixels");
    let spec = ImageSpec::new(width, height, ColorType::Gray(8)).with_extra_tags(extra_tags);
    let mut buffer = Cursor::new(Vec::new());
    let mut encoder = Encoder::new(&mut buffer).expect("encoder");
    encoder
        .write_image(&spec, pixels.as_u8().expect("gray8 pixels"))
        .expect("write");
    encoder.finish().expect("finish");
    let ours = dir.join("ours.tif");
    fs::write(&ours, buffer.into_inner()).expect("write ours.tif");

    // The independent check: libtiff's own tag parser on both files.
    let run_tiffinfo = |path: &std::path::Path| -> String {
        let output = Command::new(&tiffinfo)
            .args(["-D", path.to_str().expect("utf8 path")])
            .output()
            .expect("spawn tiffinfo");
        assert!(
            output.status.success(),
            "tiffinfo rejected {}: {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    };

    let source_info = run_tiffinfo(&source);
    let ours_info = run_tiffinfo(&ours);

    let source_lines = geotiff_lines(&source_info);
    let ours_lines = geotiff_lines(&ours_info);

    // Every row must have actually appeared in *both* outputs -- so this
    // cannot pass vacuously if `tiffinfo`'s formatting for one of these
    // tags ever changes underneath this test.
    assert_eq!(
        source_lines.len(),
        GEOTIFF_TAGS.len(),
        "tiffinfo did not print all four tags for the source fixture:\n{source_info}"
    );
    assert_eq!(
        ours_lines.len(),
        GEOTIFF_TAGS.len(),
        "tiffinfo did not print all four tags for our re-encoded file:\n{ours_info}"
    );
    assert_eq!(
        source_lines, ours_lines,
        "tiffinfo -D reports different GeoTIFF tag values for the source and our re-encode"
    );

    let _ = fs::remove_dir_all(&dir);
}
