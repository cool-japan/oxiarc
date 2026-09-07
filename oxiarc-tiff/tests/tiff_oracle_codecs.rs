//! Differential oracle tests, one suite per codec.
//!
//! The container, geometry and colour suites live in `tiff_oracle.rs`; this
//! file is the codec half: every compression value this crate implements is
//! checked in **both** directions against libtiff (`tiffcp`), Pillow and
//! `tifffile`.
//!
//! Gated behind the `tiff-oracle` feature; every test self-skips when its
//! reference tool is absent.
#![cfg(feature = "tiff-oracle")]

mod oracle_support;
mod support;

use oracle_support::{
    decode_file, find_python, find_tool, read_manifest, run_driver, scratch_dir, write_driver,
};
use oxiarc_tiff::{ColorType, Compression, Decoder, Encoder, ImageSpec, Layout, Samples};
use std::fs;
use std::io::Cursor;
use std::path::Path;
use std::process::Command;
use support::RawTiff;

/// Decodes a bilevel fixture into one byte per pixel.
fn decode_bilevel(path: &Path) -> Vec<u8> {
    decode_file(path)
}

#[test]
fn ccitt_recodings_decode_byte_identically() {
    let Some(python) = find_python() else {
        eprintln!("skipping: python3 with numpy + tifffile is not available");
        return;
    };
    let Some(tiffcp) = find_tool("tiffcp") else {
        eprintln!("skipping: libtiff's tiffcp is not on PATH");
        return;
    };
    let dir = scratch_dir("ccitt");
    let driver = write_driver(&dir);
    run_driver(&python, &driver, "gen", &dir);

    // `tiffcp` refuses the fax codecs for anything but 1-bit data, so only the
    // bilevel fixtures take part — and both photometrics do, because libtiff
    // codes the *bits*, never the colours.
    //
    // The `_fill2` rows are the important ones: the fax codecs are the only
    // ones that consume `FillOrder` *themselves* (libtiff's `Fax3SetupState`
    // sets `TIFF_NOBITREV`), so an encode-then-decode round trip of our own
    // cannot tell a correct bit order from an inverted one. Only libtiff's
    // bytes can.
    let variants: [(&str, &[&str]); 11] = [
        ("rle_via_g3", &["-c", "g3:1d"]),
        ("g3", &["-c", "g3"]),
        ("g3_2d", &["-c", "g3:2d"]),
        ("g3_2d_fill", &["-c", "g3:2d:fill"]),
        ("g3_fill", &["-c", "g3:fill"]),
        ("g4", &["-c", "g4"]),
        ("rle_fill2", &["-c", "g3:1d", "-f", "lsb2msb"]),
        ("g3_2d_fill2", &["-c", "g3:2d", "-f", "lsb2msb"]),
        ("g4_fill2", &["-c", "g4", "-f", "lsb2msb"]),
        (
            "g3_2d_tiles",
            &["-c", "g3:2d", "-t", "-w", "16", "-l", "16"],
        ),
        ("g4_tiles", &["-c", "g4", "-t", "-w", "16", "-l", "16"]),
    ];
    let mut checked = 0usize;
    let mut labels_seen: Vec<&str> = Vec::new();
    let mut methods_seen: Vec<u16> = Vec::new();
    for fixture in read_manifest(&dir) {
        if !fixture.name.starts_with("bilevel") {
            continue;
        }
        let source = dir.join(format!("{}.tif", fixture.name));
        let expected = fs::read(dir.join(format!("{}.raw", fixture.name))).expect("raw");
        for (label, args) in variants {
            let target = dir.join(format!("{}_{label}.tif", fixture.name));
            let output = Command::new(&tiffcp)
                .args(args)
                .arg(&source)
                .arg(&target)
                .output()
                .expect("spawn tiffcp");
            if !output.status.success() {
                continue;
            }
            let bytes = fs::read(&target).expect("read recoded");
            let mut probe = Decoder::new(Cursor::new(bytes)).expect("decoder");
            let info = probe.info().expect("info");
            let method = info.compression.to_u16();
            if !methods_seen.contains(&method) {
                methods_seen.push(method);
            }
            if label.ends_with("_fill2") {
                // Without this the row could pass vacuously: a file libtiff
                // wrote MSB-first proves nothing about the fax bit order.
                let tag = probe
                    .find_tag(oxiarc_tiff::Tag::FillOrder)
                    .expect("fill order tag")
                    .and_then(|value| value.first_u64());
                assert_eq!(
                    tag,
                    Some(2),
                    "`tiffcp {}` did not write FillOrder 2",
                    args.join(" ")
                );
            }
            if label.ends_with("_tiles") {
                assert_eq!(
                    probe.info().expect("info").chunk_type(),
                    oxiarc_tiff::ChunkType::Tile,
                    "`tiffcp {}` did not produce tiles",
                    args.join(" ")
                );
            }
            if !labels_seen.contains(&label) {
                labels_seen.push(label);
            }
            let got = decode_bilevel(&target);
            assert_eq!(
                got,
                expected,
                "{} through `tiffcp {}`",
                fixture.name,
                args.join(" ")
            );
            checked += 1;
        }
    }
    assert!(checked >= 20, "only {checked} CCITT recodings were checked");
    for (label, args) in variants {
        assert!(
            labels_seen.contains(&label),
            "`tiffcp {}` never produced a file, so that row proves nothing",
            args.join(" ")
        );
    }
    for method in [3u16, 4] {
        assert!(
            methods_seen.contains(&method),
            "libtiff never produced compression {method}; seen {methods_seen:?}"
        );
    }
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn jpeg_recodings_decode_close_to_the_reference() {
    let Some(python) = find_python() else {
        eprintln!("skipping: python3 with numpy + tifffile is not available");
        return;
    };
    let Some(tiffcp) = find_tool("tiffcp") else {
        eprintln!("skipping: libtiff's tiffcp is not on PATH");
        return;
    };
    if !cfg!(feature = "jpeg") {
        eprintln!("skipping: built without the `jpeg` feature");
        return;
    }
    let dir = scratch_dir("jpegcp");
    let driver = write_driver(&dir);
    run_driver(&python, &driver, "gen", &dir);

    // libtiff's default `-c jpeg` converts RGB to subsampled YCbCr and
    // rewrites the photometric; `-c jpeg:r` keeps the components as they are.
    // libtiff refuses a `RowsPerStrip` that is not a multiple of `8 * Vmax`
    // (16 for its default 4:2:0 chroma), which is the same rule this crate
    // enforces on write, so the re-codings have to ask for one.
    let variants: [(&str, &[&str]); 3] = [
        ("jpeg", &["-c", "jpeg", "-r", "16"]),
        ("jpeg_r", &["-c", "jpeg:r", "-r", "16"]),
        ("jpeg_tiles", &["-c", "jpeg", "-t", "-w", "16", "-l", "16"]),
    ];
    let mut checked = 0usize;
    for fixture in read_manifest(&dir) {
        if fixture.dtype != "uint8" || fixture.name.starts_with("bilevel") {
            continue;
        }
        if !matches!(fixture.name.as_str(), "gray8_strips" | "rgb8_strips") {
            continue;
        }
        let source = dir.join(format!("{}.tif", fixture.name));
        let expected = fs::read(dir.join(format!("{}.raw", fixture.name))).expect("raw");
        for (label, args) in variants {
            let target = dir.join(format!("{}_{label}.tif", fixture.name));
            let output = Command::new(&tiffcp)
                .args(args)
                .arg(&source)
                .arg(&target)
                .output()
                .expect("spawn tiffcp");
            if !output.status.success() {
                continue;
            }
            // JPEG is lossy, so the reference is not the original image: it
            // is **libtiff's own decode of the same file**, which `tiffcp -c
            // none` materialises. Comparing against that turns a lossy codec
            // into an exact differential — a wrong colour matrix, a shifted
            // chroma plane or a mis-scaled IDCT all show up immediately,
            // where a comparison with the original would drown in JPEG loss.
            let plain = dir.join(format!("{}_{label}_none.tif", fixture.name));
            let recode = Command::new(&tiffcp)
                .args(["-c", "none"])
                .arg(&target)
                .arg(&plain)
                .output()
                .expect("spawn tiffcp");
            assert!(
                recode.status.success(),
                "libtiff could not re-code its own JPEG: {}",
                String::from_utf8_lossy(&recode.stderr)
            );
            let reference = decode_file(&plain);

            let bytes = fs::read(&target).expect("read recoded");
            let mut decoder = Decoder::new(Cursor::new(bytes)).expect("decoder");
            let photometric = decoder.info().expect("info").photometric;
            let got = if photometric == oxiarc_tiff::PhotometricInterpretation::YCbCr {
                decoder.read_image_rgb8().expect("rgb8")
            } else {
                decoder.read_image().expect("decode").to_native_bytes()
            };
            assert_eq!(
                got.len(),
                reference.len(),
                "{} {label}: {} samples against libtiff's {}",
                fixture.name,
                got.len(),
                reference.len()
            );
            let mean = got
                .iter()
                .zip(reference.iter())
                .map(|(a, b)| f64::from(a.abs_diff(*b)))
                .sum::<f64>()
                / got.len().max(1) as f64;
            let worst = got
                .iter()
                .zip(reference.iter())
                .map(|(a, b)| a.abs_diff(*b))
                .max()
                .unwrap_or(0);
            // The entropy decode and the IDCT are exact; what is left is the
            // last rounding step of the colour conversion, where libtiff uses
            // its own fixed-point tables.
            let tolerance = if photometric == oxiarc_tiff::PhotometricInterpretation::YCbCr {
                (1.5, 6u8)
            } else {
                (0.01, 1u8)
            };
            assert!(
                mean <= tolerance.0 && worst <= tolerance.1,
                "{} through `tiffcp {}`: mean |diff| {mean:.3}, worst {worst} \
                 against libtiff's own decode (limits {:?})",
                fixture.name,
                args.join(" "),
                tolerance
            );
            // And a sanity bound against the original: a codec that returned
            // grey would satisfy the differential above only if libtiff did
            // too, which it does not.
            let from_source = got
                .iter()
                .zip(expected.iter())
                .map(|(a, b)| f64::from(a.abs_diff(*b)))
                .sum::<f64>()
                / got.len().max(1) as f64;
            assert!(
                from_source < 64.0,
                "{} {label}: mean |diff| {from_source:.2} from the source image",
                fixture.name
            );
            checked += 1;
        }
    }
    assert!(checked >= 4, "only {checked} JPEG recodings were checked");
    let _ = fs::remove_dir_all(&dir);
}

/// The `JPEGTables` (347) blob of a file on disk, or `None` when it has none.
#[cfg(feature = "jpeg")]
fn tables_tag(path: &Path) -> Option<Vec<u8>> {
    let bytes = fs::read(path).expect("read tiff");
    let mut decoder = Decoder::new(Cursor::new(bytes)).expect("decoder");
    decoder.info().expect("info").jpeg_tables.clone()
}

/// The `JPEGTables` (347) blob this crate writes for one page shape.
#[cfg(feature = "jpeg")]
fn our_tables_tag(dir: &Path, label: &str, spec: ImageSpec, samples: &[u8]) -> Vec<u8> {
    let path = dir.join(format!("ours_{label}.tif"));
    let file = fs::File::create(&path).expect("create");
    let mut encoder = Encoder::new(std::io::BufWriter::new(file)).expect("encoder");
    encoder.write_image(&spec, samples).expect("write");
    encoder.finish().expect("finish");
    tables_tag(&path).expect("our own tag 347")
}

/// Tag 347 must be what libtiff writes, byte for byte, at the same quality.
///
/// The tables are the whole compatibility contract of TTN2's shared-table
/// mode: every strip of the page is an abbreviated datastream that means
/// nothing without them, so a single byte of drift here is a file libtiff
/// decodes differently from this crate. Quality 75 is libtiff's
/// `JPEGQUALITY` default and this crate's, so `tiffcp -c jpeg` and
/// `Compression::Jpeg { quality: 75, .. }` have to agree.
#[test]
fn our_jpeg_tables_tag_matches_libtiffs_byte_for_byte() {
    #[cfg(not(feature = "jpeg"))]
    {
        eprintln!("skipping: built without the `jpeg` feature");
    }
    #[cfg(feature = "jpeg")]
    {
        let Some(python) = find_python() else {
            eprintln!("skipping: python3 with numpy + tifffile is not available");
            return;
        };
        let Some(tiffcp) = find_tool("tiffcp") else {
            eprintln!("skipping: libtiff's tiffcp is not on PATH");
            return;
        };
        let dir = scratch_dir("jpegtables");
        let driver = write_driver(&dir);
        run_driver(&python, &driver, "gen", &dir);

        let mut checked = 0usize;
        for (fixture, label, spec) in [
            (
                "gray8_strips",
                "gray",
                ImageSpec::new(16, 16, ColorType::Gray(8)),
            ),
            (
                // `tiffcp -c jpeg` turns an RGB page into subsampled YCbCr,
                // which is the two-table shape (luma *and* chroma).
                "rgb8_strips",
                "ycbcr",
                ImageSpec::new(16, 16, ColorType::YCbCr(8)).with_ycbcr_subsampling(2, 2),
            ),
        ] {
            let source = dir.join(format!("{fixture}.tif"));
            if !source.exists() {
                continue;
            }
            let target = dir.join(format!("{fixture}_jpeg.tif"));
            let output = Command::new(&tiffcp)
                .args(["-c", "jpeg", "-r", "16"])
                .arg(&source)
                .arg(&target)
                .output()
                .expect("spawn tiffcp");
            if !output.status.success() {
                continue;
            }
            let Some(theirs) = tables_tag(&target) else {
                continue;
            };
            let spp = usize::from(spec.samples_per_pixel);
            let samples = vec![128u8; 16 * 16 * spp];
            let ours = our_tables_tag(
                &dir,
                label,
                spec.with_compression(Compression::Jpeg {
                    quality: 75,
                    shared_tables: true,
                })
                .with_layout(Layout::Strips { rows_per_strip: 16 }),
                &samples,
            );
            assert_eq!(
                ours.len(),
                theirs.len(),
                "{label}: tag 347 is {} bytes, libtiff writes {}",
                ours.len(),
                theirs.len()
            );
            assert_eq!(ours, theirs, "{label}: tag 347 differs from libtiff's");
            checked += 1;
        }
        assert!(
            checked > 0,
            "no fixture exercised the JPEGTables comparison"
        );
    }
}

/// libtiff must read a JPEG page whose strips carry restart markers.
#[test]
fn libtiff_reads_our_jpeg_strips_with_restart_markers() {
    #[cfg(not(feature = "jpeg"))]
    {
        eprintln!("skipping: built without the `jpeg` feature");
    }
    #[cfg(feature = "jpeg")]
    {
        let Some(tiffcp) = find_tool("tiffcp") else {
            eprintln!("skipping: libtiff's tiffcp is not on PATH");
            return;
        };
        let dir = scratch_dir("jpegrestart");
        let pixels: Vec<u8> = (0..64 * 64u32).map(|i| ((i * 7) % 251) as u8).collect();
        let path = dir.join("restart.tif");
        let file = fs::File::create(&path).expect("create");
        let mut encoder = Encoder::new(std::io::BufWriter::new(file)).expect("encoder");
        let spec = ImageSpec::new(64, 64, ColorType::Gray(8))
            .with_compression(Compression::Jpeg {
                quality: 75,
                shared_tables: true,
            })
            .with_layout(Layout::Strips { rows_per_strip: 16 })
            .with_jpeg_restart_rows(1);
        encoder.write_image(&spec, &pixels).expect("write");
        encoder.finish().expect("finish");

        // The strips really carry `DRI`, which is what makes the test about
        // restart markers rather than about JPEG in general.
        let bytes = fs::read(&path).expect("read");
        let mut decoder = Decoder::new(Cursor::new(bytes)).expect("decoder");
        let strip = decoder.read_chunk_raw(0).expect("raw strip");
        assert!(
            strip.windows(2).any(|w| w == [0xFF, 0xDD]),
            "no DRI segment in the strip"
        );

        // libtiff's own decode of the page, through a re-code to uncompressed.
        let plain = dir.join("restart_none.tif");
        let recode = Command::new(&tiffcp)
            .args(["-c", "none"])
            .arg(&path)
            .arg(&plain)
            .output()
            .expect("spawn tiffcp");
        assert!(
            recode.status.success(),
            "libtiff refused our restart-marked JPEG: {}",
            String::from_utf8_lossy(&recode.stderr)
        );
        let reference = decode_file(&plain);
        let ours = decoder.read_image().expect("decode").to_native_bytes();
        assert_eq!(ours.len(), reference.len());
        assert_eq!(ours, reference, "our decode differs from libtiff's");
    }
}

/// Splits a complete JPEG datastream into a TTN2 `JPEGTables` blob and the
/// per-strip stream that refers to it.
///
/// `JPEGTables` is `SOI`, the quantisation and Huffman tables, `EOI`; the
/// strip is `SOI`, the frame header, the scan and its entropy-coded bytes.
/// Application markers belong to neither: TIFF carries no `JFIF`.
fn split_jpeg(bytes: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let mut tables = vec![0xFF, 0xD8];
    let mut strip = vec![0xFF, 0xD8];
    let mut index = 2usize;
    while index + 4 <= bytes.len() {
        assert_eq!(bytes[index], 0xFF, "lost marker sync at {index}");
        let marker = bytes[index + 1];
        let length = 2 + usize::from(u16::from_be_bytes([bytes[index + 2], bytes[index + 3]]));
        let Some(segment) = bytes.get(index..index + 2 + length - 2) else {
            break;
        };
        match marker {
            // Quantisation and Huffman tables: the abbreviated blob.
            0xDB | 0xC4 => tables.extend_from_slice(segment),
            // The scan: everything from here to the end of the file.
            0xDA => {
                strip.extend_from_slice(bytes.get(index..).unwrap_or(&[]));
                break;
            }
            // Application and comment markers are dropped.
            0xE0..=0xEF | 0xFE => {}
            // Frame headers and anything else the encoder emitted.
            _ => strip.extend_from_slice(segment),
        }
        index += length;
    }
    tables.extend_from_slice(&[0xFF, 0xD9]);
    (tables, strip)
}

/// Reads a binary PGM whose maxval exceeds 255 (big-endian samples).
fn read_pgm16(bytes: &[u8]) -> (usize, usize, Vec<u16>) {
    let mut fields: Vec<usize> = Vec::new();
    let mut cursor = 2usize; // past "P5"
    while fields.len() < 3 {
        while bytes.get(cursor).is_some_and(|b| b.is_ascii_whitespace()) {
            cursor += 1;
        }
        if bytes.get(cursor) == Some(&b'#') {
            while bytes.get(cursor).is_some_and(|b| *b != b'\n') {
                cursor += 1;
            }
            continue;
        }
        let start = cursor;
        while bytes.get(cursor).is_some_and(u8::is_ascii_digit) {
            cursor += 1;
        }
        let text = std::str::from_utf8(&bytes[start..cursor]).expect("ascii header");
        fields.push(text.parse().expect("number"));
    }
    cursor += 1; // the single whitespace byte after maxval
    let samples = bytes[cursor..]
        .chunks_exact(2)
        .map(|pair| u16::from_be_bytes([pair[0], pair[1]]))
        .collect();
    (fields[0], fields[1], samples)
}

#[test]
fn a_twelve_bit_jpeg_strip_decodes_like_libjpeg() {
    if !cfg!(feature = "jpeg") {
        eprintln!("skipping: built without the `jpeg` feature");
        return;
    }
    let (Some(cjpeg), Some(djpeg)) = (find_tool("cjpeg"), find_tool("djpeg")) else {
        eprintln!("skipping: libjpeg's cjpeg/djpeg are not on PATH");
        return;
    };
    // `tiffcp -c jpeg` refuses anything but 8-bit samples, so the only way to
    // reach the deep-precision path is to build the strip by hand: a
    // twelve-bit extended-sequential frame (`SOF1`), which is what a scanner
    // or a medical device writes into a TIFF.
    let dir = scratch_dir("jpeg12");
    let (width, height) = (32usize, 24usize);
    let source: Vec<u16> = (0..width * height)
        .map(|i| ((i * 37) % 4096) as u16)
        .collect();
    let mut pgm = format!("P5\n{width} {height}\n4095\n").into_bytes();
    for value in &source {
        pgm.extend_from_slice(&value.to_be_bytes());
    }
    let pgm_path = dir.join("source.pgm");
    fs::write(&pgm_path, &pgm).expect("write pgm");
    let jpeg_path = dir.join("source.jpg");
    let made = Command::new(&cjpeg)
        .args(["-precision", "12", "-quality", "95", "-outfile"])
        .arg(&jpeg_path)
        .arg(&pgm_path)
        .output()
        .expect("spawn cjpeg");
    if !made.status.success() {
        eprintln!("skipping: this cjpeg cannot write twelve-bit JPEG");
        let _ = fs::remove_dir_all(&dir);
        return;
    }

    // libjpeg's own decode of the same stream is the reference.
    let ref_path = dir.join("reference.pgm");
    let decoded = Command::new(&djpeg)
        .arg("-outfile")
        .arg(&ref_path)
        .arg(&jpeg_path)
        .output()
        .expect("spawn djpeg");
    assert!(decoded.status.success(), "djpeg refused its own output");
    let (ref_width, ref_height, reference) =
        read_pgm16(&fs::read(&ref_path).expect("read reference"));
    assert_eq!((ref_width, ref_height), (width, height));

    let jpeg = fs::read(&jpeg_path).expect("read jpeg");
    let (tables, scan) = split_jpeg(&jpeg);
    // Both TIFF shapes: the strip alone, and the TTN2 split with tag 347.
    for (label, strip, blob) in [
        ("self-contained", jpeg.clone(), None),
        ("abbreviated", scan, Some(tables)),
    ] {
        let mut tiff = RawTiff::new();
        let offset = tiff.add_data(&strip);
        tiff.long(256, &[width as u32]);
        tiff.long(257, &[height as u32]);
        tiff.short(258, &[12]);
        tiff.short(259, &[7]);
        tiff.short(262, &[1]);
        tiff.short(277, &[1]);
        tiff.long(278, &[height as u32]);
        tiff.long(273, &[offset as u32]);
        tiff.long(279, &[strip.len() as u32]);
        if let Some(blob) = blob.as_ref() {
            tiff.undefined(347, blob);
        }
        let mut decoder = Decoder::new(Cursor::new(tiff.build())).expect("decoder");
        let samples = match decoder.read_image().expect("decode twelve-bit jpeg") {
            Samples::U16(values) => values,
            other => panic!(
                "{label}: twelve-bit samples came back as {:?}",
                other.sample_type()
            ),
        };
        assert_eq!(samples.len(), reference.len(), "{label}");
        let mut worst = 0i32;
        let mut total = 0i64;
        for (got, want) in samples.iter().zip(reference.iter()) {
            let diff = i32::from(*got) - i32::from(*want);
            worst = worst.max(diff.abs());
            total += i64::from(diff.abs());
            assert!(
                *got < 4096,
                "{label}: sample {got} does not fit in twelve bits"
            );
        }
        let mean = total as f64 / samples.len() as f64;
        // The IDCT is libjpeg's own `islow`, so the two decodes agree to
        // within its rounding — not merely "close enough to look right". On
        // libjpeg-turbo 3.1.4 the measured difference is exactly zero; the
        // slack is for a build whose IDCT rounds differently.
        assert!(
            worst <= 2 && mean <= 0.05,
            "{label}: twelve-bit decode differs from libjpeg (worst {worst}, mean {mean:.3})"
        );
    }
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn libtiff_reads_every_codec_we_write() {
    let Some(tiffcp) = find_tool("tiffcp") else {
        eprintln!("skipping: libtiff's tiffcp is not on PATH");
        return;
    };
    let dir = scratch_dir("codecs");

    let width = 61u32;
    let height = 37u32;
    let gray: Vec<u8> = (0..width * height).map(|i| (i * 7 % 251) as u8).collect();
    let gray16: Vec<u8> = (0..width * height)
        .flat_map(|i| ((i * 37 % 65521) as u16).to_ne_bytes())
        .collect();
    let bilevel: Vec<u8> = (0..width * height)
        .map(|i| u8::from((i / 5 + i / (width * 3)) % 3 == 0))
        .collect();
    let rgb: Vec<u8> = (0..width * height * 3)
        .map(|i| (i * 11 % 253) as u8)
        .collect();

    // (label, spec, pixels, lossless)
    let mut cases: Vec<(&str, ImageSpec, &Vec<u8>, bool)> = vec![
        (
            "lzw",
            ImageSpec::new(width, height, ColorType::Gray(8))
                .with_compression(Compression::Lzw)
                .with_layout(Layout::Strips { rows_per_strip: 7 }),
            &gray,
            true,
        ),
        (
            "lzw_predictor",
            ImageSpec::new(width, height, ColorType::Gray(16))
                .with_compression(Compression::Lzw)
                .with_predictor(oxiarc_tiff::Predictor::Horizontal)
                .with_layout(Layout::Strips { rows_per_strip: 7 }),
            &gray16,
            true,
        ),
        (
            "deflate",
            ImageSpec::new(width, height, ColorType::Rgb(8))
                .with_compression(Compression::Deflate { level: 6 })
                .with_layout(Layout::Strips { rows_per_strip: 5 }),
            &rgb,
            true,
        ),
        (
            "deflate_tiles_predictor",
            ImageSpec::new(width, height, ColorType::Gray(16))
                .with_compression(Compression::Deflate { level: 9 })
                .with_predictor(oxiarc_tiff::Predictor::Horizontal)
                .with_layout(Layout::Tiles {
                    width: 16,
                    length: 16,
                }),
            &gray16,
            true,
        ),
        (
            "ccitt_rle",
            ImageSpec::new(width, height, ColorType::Gray(1))
                .with_compression(Compression::CcittRle)
                .with_layout(Layout::Strips { rows_per_strip: 9 }),
            &bilevel,
            true,
        ),
        (
            "ccitt_g3",
            ImageSpec::new(width, height, ColorType::Gray(1))
                .with_compression(Compression::CcittGroup3 {
                    two_dimensional: false,
                    byte_align_eol: false,
                })
                .with_layout(Layout::Strips { rows_per_strip: 9 }),
            &bilevel,
            true,
        ),
        (
            "ccitt_g3_2d_fill",
            ImageSpec::new(width, height, ColorType::Gray(1))
                .with_compression(Compression::CcittGroup3 {
                    two_dimensional: true,
                    byte_align_eol: true,
                })
                .with_layout(Layout::Strips { rows_per_strip: 9 }),
            &bilevel,
            true,
        ),
        (
            "ccitt_g4",
            ImageSpec::new(width, height, ColorType::Gray(1))
                .with_compression(Compression::CcittGroup4)
                .with_layout(Layout::Strips { rows_per_strip: 9 }),
            &bilevel,
            true,
        ),
    ];
    if cfg!(feature = "zstd") {
        cases.push((
            "zstd",
            ImageSpec::new(width, height, ColorType::Gray(8))
                .with_compression(Compression::Zstd { level: 7 })
                .with_layout(Layout::Strips { rows_per_strip: 7 }),
            &gray,
            true,
        ));
    }
    if cfg!(feature = "lzma") {
        cases.push((
            "lzma",
            ImageSpec::new(width, height, ColorType::Rgb(8))
                .with_compression(Compression::Lzma { preset: 6 })
                .with_layout(Layout::Strips { rows_per_strip: 7 }),
            &rgb,
            true,
        ));
    }
    if cfg!(feature = "jpeg") {
        cases.push((
            "jpeg_gray",
            ImageSpec::new(width, height, ColorType::Gray(8))
                .with_compression(Compression::Jpeg {
                    quality: 92,
                    shared_tables: true,
                })
                .with_layout(Layout::Strips { rows_per_strip: 8 }),
            &gray,
            false,
        ));
        cases.push((
            "jpeg_rgb_inline_tables",
            ImageSpec::new(width, height, ColorType::Rgb(8))
                .with_compression(Compression::Jpeg {
                    quality: 92,
                    shared_tables: false,
                })
                .with_layout(Layout::Strips { rows_per_strip: 8 }),
            &rgb,
            false,
        ));
    }

    for (label, spec, pixels, lossless) in &cases {
        let mut buffer = Cursor::new(Vec::new());
        let mut encoder = Encoder::new(&mut buffer).expect("encoder");
        encoder.write_image(spec, pixels).expect("write");
        encoder.finish().expect("finish");
        let path = dir.join(format!("{label}.tif"));
        fs::write(&path, buffer.into_inner()).expect("save");

        // libtiff must be able to re-code it, which exercises its decoder.
        let plain = dir.join(format!("{label}_none.tif"));
        let output = Command::new(&tiffcp)
            .args(["-c", "none"])
            .arg(&path)
            .arg(&plain)
            .output()
            .expect("spawn tiffcp");
        assert!(
            output.status.success(),
            "libtiff rejected our {label}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stderr = String::from_utf8_lossy(&output.stderr).to_lowercase();
        assert!(
            !stderr.contains("error"),
            "libtiff complained about our {label}: {stderr}"
        );

        // And we must read libtiff's re-coding back to the original pixels.
        let round_trip = decode_file(&plain);
        if *lossless {
            assert_eq!(
                round_trip.len(),
                pixels.len(),
                "{label}: libtiff produced {} bytes",
                round_trip.len()
            );
            assert_eq!(&round_trip, *pixels, "{label}: libtiff round trip differs");
        } else {
            let mean = round_trip
                .iter()
                .zip(pixels.iter())
                .map(|(a, b)| f64::from(a.abs_diff(*b)))
                .sum::<f64>()
                / round_trip.len().max(1) as f64;
            assert!(mean < 12.0, "{label}: mean |diff| {mean:.2} after libtiff");
        }
    }
    assert!(
        cases.len() >= 8,
        "the codec matrix shrank to {}",
        cases.len()
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn pillow_and_tifffile_read_every_codec_we_write() {
    let pillow = Command::new("python3")
        .args(["-c", "import PIL.Image, numpy"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !pillow {
        eprintln!("skipping: python3 with Pillow is not available");
        return;
    }
    let Some(python) = find_python() else {
        eprintln!("skipping: python3 with numpy + tifffile is not available");
        return;
    };
    let dir = scratch_dir("pillow_codecs");
    let width = 40u32;
    let height = 24u32;
    let gray: Vec<u8> = (0..width * height).map(|i| (i * 5 % 251) as u8).collect();
    let bilevel: Vec<u8> = (0..width * height)
        .map(|i| u8::from((i / 3 + i / width) % 4 == 0))
        .collect();
    let rgb: Vec<u8> = (0..width * height * 3)
        .map(|i| (i * 3 % 253) as u8)
        .collect();

    // (label, reader, spec, pixels, channels, white_is_zero)
    // `tifffile` decodes zlib, lzma and zstd from the Python standard library;
    // Pillow goes through libtiff, which is what covers LZW, PackBits and the
    // fax codes. Between them every codec this crate writes is read back by an
    // independent implementation.
    /// One page to write and hand to an independent reader:
    /// label, reader, spec, pixels, channels, whether white is bit zero.
    type ReaderCase<'a> = (&'a str, &'a str, ImageSpec, &'a Vec<u8>, usize, bool);
    let mut cases: Vec<ReaderCase<'_>> = vec![
        (
            "lzw",
            "pillow",
            ImageSpec::new(width, height, ColorType::Gray(8))
                .with_compression(Compression::Lzw)
                .with_layout(Layout::Strips { rows_per_strip: 6 }),
            &gray,
            1,
            false,
        ),
        (
            "packbits",
            "pillow",
            ImageSpec::new(width, height, ColorType::Rgb(8))
                .with_compression(Compression::PackBits)
                .with_layout(Layout::Strips { rows_per_strip: 6 }),
            &rgb,
            3,
            false,
        ),
        (
            "deflate",
            "tifffile",
            ImageSpec::new(width, height, ColorType::Rgb(8))
                .with_compression(Compression::Deflate { level: 6 })
                .with_layout(Layout::Strips { rows_per_strip: 6 }),
            &rgb,
            3,
            false,
        ),
        (
            "g4",
            "pillow",
            ImageSpec::new(width, height, ColorType::Gray(1))
                .with_photometric(oxiarc_tiff::PhotometricInterpretation::WhiteIsZero)
                .with_compression(Compression::CcittGroup4)
                .with_layout(Layout::Strips { rows_per_strip: 6 }),
            &bilevel,
            1,
            true,
        ),
        (
            "g3_2d",
            "pillow",
            ImageSpec::new(width, height, ColorType::Gray(1))
                .with_photometric(oxiarc_tiff::PhotometricInterpretation::WhiteIsZero)
                .with_compression(Compression::CcittGroup3 {
                    two_dimensional: true,
                    byte_align_eol: false,
                })
                .with_layout(Layout::Strips { rows_per_strip: 6 }),
            &bilevel,
            1,
            true,
        ),
    ];
    if cfg!(feature = "lzma") {
        cases.push((
            "lzma",
            "tifffile",
            ImageSpec::new(width, height, ColorType::Gray(8))
                .with_compression(Compression::Lzma { preset: 4 })
                .with_layout(Layout::Strips { rows_per_strip: 6 }),
            &gray,
            1,
            false,
        ));
    }
    if cfg!(feature = "zstd") {
        cases.push((
            "zstd",
            "tifffile",
            ImageSpec::new(width, height, ColorType::Gray(8))
                .with_compression(Compression::Zstd { level: 5 })
                .with_layout(Layout::Strips { rows_per_strip: 6 }),
            &gray,
            1,
            false,
        ));
    }

    let script = r#"
import sys
import numpy as np

path, raw_path, reader, channels = sys.argv[1], sys.argv[2], sys.argv[3], int(sys.argv[4])
white_is_zero = sys.argv[5] == "1"
expected = np.frombuffer(open(raw_path, "rb").read(), dtype=np.uint8)
if reader == "pillow":
    from PIL import Image
    img = Image.open(path)
    if channels == 3:
        got = np.asarray(img.convert("RGB"), dtype=np.uint8)
    else:
        # A bilevel page comes back as mode "1", where Pillow has already
        # applied the photometric: with MinIsWhite a stored 1 bit (black)
        # displays as 0. Undo that so the comparison is against the bits this
        # crate wrote.
        got = np.asarray(img, dtype=np.uint8)
        if img.mode == "1":
            got = (got == 0).astype(np.uint8) if white_is_zero else (got > 0).astype(np.uint8)
else:
    import tifffile
    got = np.asarray(tifffile.imread(path), dtype=np.uint8)
got = got.reshape(-1)
if got.shape != expected.shape:
    print(f"FAIL shape {got.shape} != {expected.shape}")
elif not np.array_equal(got, expected):
    bad = int((got != expected).sum())
    print(f"FAIL {bad} of {got.size} samples differ")
else:
    print("OK")
"#;
    let script_path = dir.join("check_codecs.py");
    fs::write(&script_path, script).expect("write script");

    for (label, reader, spec, pixels, channels, white_is_zero) in &cases {
        let mut buffer = Cursor::new(Vec::new());
        let mut encoder = Encoder::new(&mut buffer).expect("encoder");
        encoder.write_image(spec, pixels).expect("write");
        encoder.finish().expect("finish");
        let path = dir.join(format!("{label}.tif"));
        fs::write(&path, buffer.into_inner()).expect("save");
        let raw = dir.join(format!("{label}.raw"));
        fs::write(&raw, pixels).expect("raw");

        let output = Command::new(&python)
            .arg(&script_path)
            .arg(&path)
            .arg(&raw)
            .arg(reader)
            .arg(channels.to_string())
            .arg(if *white_is_zero { "1" } else { "0" })
            .output()
            .expect("spawn python");
        assert!(
            output.status.success(),
            "{reader} failed on our {label}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            "OK",
            "{reader} read different pixels from our {label}: {}",
            String::from_utf8_lossy(&output.stdout)
        );
    }
    assert!(cases.len() >= 5);
    let _ = fs::remove_dir_all(&dir);
}
