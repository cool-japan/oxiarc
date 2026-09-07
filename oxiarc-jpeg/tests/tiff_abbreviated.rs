//! TIFF `Compression = 7` interop: the `JPEGTables` tag and per-strip
//! scan-only datastreams.
//!
//! The always-run half uses the crate's embedded abbreviated pair and
//! hand-patched streams. The `jpeg-oracle` half builds real files with
//! libtiff's `tiffcp`, pulls the tag and the strips out with `tifffile`, and
//! checks every strip against Pillow's read of the same TIFF.

use oxiarc_jpeg::{
    ColorSpace, DecodeOptions, Decoder, JpegError, TableSet, TablesMode, decode_abbreviated_into,
    sample, tiff,
};

/// Offset of the `SOF` marker.
fn sof_offset(data: &[u8]) -> usize {
    let mut i = 2usize;
    while i + 4 <= data.len() {
        let marker = data[i + 1];
        if marker == 0xD8 || marker == 0xD9 {
            i += 2;
            continue;
        }
        let length = usize::from(u16::from_be_bytes([data[i + 2], data[i + 3]]));
        if (0xC0..=0xCF).contains(&marker) && marker != 0xC4 && marker != 0xCC {
            return i;
        }
        i += 2 + length;
    }
    panic!("no SOF");
}

/// Offset of the `SOS` marker.
fn sos_offset(data: &[u8]) -> usize {
    let mut i = 2usize;
    while i + 4 <= data.len() {
        let marker = data[i + 1];
        if marker == 0xD8 || marker == 0xD9 {
            i += 2;
            continue;
        }
        let length = usize::from(u16::from_be_bytes([data[i + 2], data[i + 3]]));
        if marker == 0xDA {
            return i;
        }
        i += 2 + length;
    }
    panic!("no SOS");
}

/// Rewrite the three component identifiers in both the `SOF` and the `SOS`.
fn with_component_ids(data: &[u8], ids: [u8; 3]) -> Vec<u8> {
    let mut out = data.to_vec();
    let sof = sof_offset(&out);
    for (k, &id) in ids.iter().enumerate() {
        out[sof + 10 + 3 * k] = id;
    }
    let sos = sos_offset(&out);
    for (k, &id) in ids.iter().enumerate() {
        out[sos + 5 + 2 * k] = id;
    }
    out
}

/// Strip the `JFIF` `APP0` segment so the identifier heuristic is reachable.
fn without_jfif(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    out.extend_from_slice(&data[..2]);
    let mut i = 2usize;
    while i + 4 <= data.len() {
        let marker = data[i + 1];
        if marker == 0xD8 || marker == 0xD9 {
            out.extend_from_slice(&data[i..i + 2]);
            i += 2;
            continue;
        }
        let length = usize::from(u16::from_be_bytes([data[i + 2], data[i + 3]]));
        if marker != 0xE0 {
            out.extend_from_slice(&data[i..i + 2 + length]);
        }
        if marker == 0xDA {
            out.extend_from_slice(&data[i + 2 + length..]);
            return out;
        }
        i += 2 + length;
    }
    out
}

/// Append an Adobe `APP14` segment with the given transform code.
fn with_adobe(data: &[u8], transform: u8) -> Vec<u8> {
    let mut out = data.to_vec();
    let sof = sof_offset(&out);
    let segment = [
        0xFF, 0xEE, 0x00, 0x0E, b'A', b'd', b'o', b'b', b'e', 0x00, 0x64, 0x00, 0x00, 0x00, 0x00,
        transform,
    ];
    out.splice(sof..sof, segment);
    out
}

#[test]
fn tif1_tables_blob_round_trips_byte_identically() {
    let tables = tiff::parse_jpeg_tables(&sample::RGB_8X8_420_TABLES).expect("parse");
    assert!(tables.quant[0].is_some() && tables.quant[1].is_some());
    assert!(tables.dc_huffman[0].is_some() && tables.dc_huffman[1].is_some());
    assert!(tables.ac_huffman[0].is_some() && tables.ac_huffman[1].is_some());
    assert_eq!(
        tiff::build_jpeg_tables(&tables, TablesMode::BOTH),
        sample::RGB_8X8_420_TABLES.to_vec()
    );
    assert_eq!(sample::RGB_8X8_420_TABLES.len(), 574, "libtiff's blob size");
}

#[test]
fn tif2_strip_decodes_without_concatenation() {
    let tables = tiff::parse_jpeg_tables(&sample::RGB_8X8_420_TABLES).expect("parse");
    let mut out = vec![0u8; 8 * 8 * 3];
    let info = decode_abbreviated_into(
        Some(&tables),
        &sample::RGB_8X8_420_SCAN,
        &DecodeOptions::raw(),
        &mut out,
    )
    .expect("strip");
    assert_eq!((info.width, info.height), (8, 8));
    assert_eq!(info.num_components, 3);
    assert_eq!(info.subsampling, (2, 2));
}

#[test]
fn tif4_a_one_row_strip_with_v2_chroma_decodes_to_one_row() {
    // libtiff's last strip carries the true remaining height, so a 1-row-high
    // SOF with V = 2 chroma is legal and common: the MCU is 16 rows tall and
    // 15 of them are padding to be discarded.
    let mut stream = sample::RGB_8X8_420.to_vec();
    let sof = sof_offset(&stream);
    stream[sof + 5] = 0;
    stream[sof + 6] = 1;
    let mut decoder = Decoder::new(stream.as_slice());
    let info = decoder.read_info().expect("info");
    assert_eq!((info.width, info.height), (8, 1));
    let pixels = decoder.decode().expect("decode");
    assert_eq!(pixels.len(), 8 * 3);

    // The row must match the first row of the full-height decode.
    let mut full = Decoder::new(&sample::RGB_8X8_420[..]);
    full.read_info().expect("info");
    let full = full.decode().expect("decode");
    assert_eq!(pixels, full[..24]);
}

#[test]
fn tif5_component_ids_drive_the_colour_heuristic() {
    let base = without_jfif(&sample::RGB_8X8_420);

    let ycbcr = with_component_ids(&base, [1, 2, 3]);
    let mut decoder = Decoder::new(ycbcr.as_slice());
    assert_eq!(
        decoder.read_info().expect("info").input_color_space,
        ColorSpace::Ycbcr
    );

    let rgb = with_component_ids(&base, [b'R', b'G', b'B']);
    let mut decoder = Decoder::new(rgb.as_slice());
    let info = decoder.read_info().expect("info");
    assert_eq!(info.input_color_space, ColorSpace::Rgb);
    assert_eq!(info.output_color_space, ColorSpace::Rgb);

    // Unknown identifiers fall back to YCbCr with no error.
    let odd = with_component_ids(&base, [7, 8, 9]);
    let mut decoder = Decoder::new(odd.as_slice());
    assert_eq!(
        decoder.read_info().expect("info").input_color_space,
        ColorSpace::Ycbcr
    );

    // JFIF beats the identifiers.
    let jfif_rgb = with_component_ids(&sample::RGB_8X8_420, [b'R', b'G', b'B']);
    let mut decoder = Decoder::new(jfif_rgb.as_slice());
    assert_eq!(
        decoder.read_info().expect("info").input_color_space,
        ColorSpace::Ycbcr
    );

    // Adobe transform 0 means RGB, 1 means YCbCr.
    let adobe_rgb = with_adobe(&base, 0);
    let mut decoder = Decoder::new(adobe_rgb.as_slice());
    let info = decoder.read_info().expect("info");
    assert_eq!(info.input_color_space, ColorSpace::Rgb);
    assert_eq!(info.adobe_transform, Some(0));
    assert!(info.has_adobe);

    let adobe_ycbcr = with_adobe(&base, 1);
    let mut decoder = Decoder::new(adobe_ycbcr.as_slice());
    assert_eq!(
        decoder.read_info().expect("info").input_color_space,
        ColorSpace::Ycbcr
    );
}

#[test]
fn tif6_raw_components_are_untransformed() {
    let rgb_ids = with_component_ids(&without_jfif(&sample::RGB_8X8_420), [b'R', b'G', b'B']);
    let mut decoder = Decoder::with_options(rgb_ids.as_slice(), DecodeOptions::raw());
    let info = decoder.read_info().expect("info");
    assert_eq!(info.output_color_space, info.input_color_space);
    let raw = decoder.decode().expect("raw");

    let mut plain = Decoder::new(rgb_ids.as_slice());
    plain.read_info().expect("info");
    // Input is already RGB, so the transform is the identity and raw output
    // must match.
    assert_eq!(raw, plain.decode().expect("decode"));
}

#[test]
fn tif8_sampling_factors_are_readable_for_the_tag_530_cross_check() {
    let mut decoder = Decoder::new(&sample::RGB_8X8_420[..]);
    let info = decoder.read_info().expect("info");
    // The SOF is authoritative; a TIFF reader compares these against tag 530.
    let components = info.components();
    assert_eq!(
        (
            components[0].h,
            components[0].v,
            components[1].h,
            components[1].v
        ),
        (2, 2, 1, 1)
    );
    assert_eq!(info.subsampling, (2, 2));
    let frame = decoder.frame_header().expect("frame");
    assert_eq!(frame.components[0].h, 2);
    assert_eq!(frame.components.len(), 3);
}

#[test]
fn a_tables_only_blob_alone_is_not_decodable() {
    let mut out = [0u8; 4];
    assert!(matches!(
        decode_abbreviated_into(
            None,
            &sample::RGB_8X8_420_TABLES,
            &DecodeOptions::default(),
            &mut out
        ),
        Err(JpegError::AbbreviatedWithoutFrame)
    ));
}

#[test]
fn tables_mode_zero_means_self_contained_strips() {
    // JPEGTABLESMODE 0: nothing in the tag, everything in the strip.
    let empty = TableSet::default().emit(TablesMode::NONE);
    assert_eq!(empty, vec![0xFF, 0xD8, 0xFF, 0xD9]);
    let parsed = tiff::parse_jpeg_tables(&empty).expect("parse");
    assert!(parsed.is_empty());

    // A self-contained strip decodes with no tables loaded.
    let mut out = vec![0u8; 8 * 8 * 3];
    decode_abbreviated_into(
        Some(&parsed),
        &sample::RGB_8X8_420,
        &DecodeOptions::default(),
        &mut out,
    )
    .expect("self-contained strip");
}

#[cfg(feature = "jpeg-oracle")]
mod oracle {
    use super::*;
    use std::process::Command;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// The flat `K` plane written into the CMYK fixture. Chosen far from its
    /// own complement so an accidental Adobe inversion (255 - 40 = 215) cannot
    /// hide inside a tolerance.
    const CMYK_CONSTANT_K: u8 = 40;

    /// Unique scratch path. The counter matters: these tests run in parallel
    /// threads of one binary, and a label-only name had them overwriting and
    /// deleting each other's dumps, which made a fixture failure look like a
    /// clean skip.
    fn temp_path(label: &str, extension: &str) -> std::path::PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let mut path = std::env::temp_dir();
        path.push(format!(
            "oxiarc_jpeg_tiff_{label}_{}_{n}.{extension}",
            std::process::id()
        ));
        path
    }

    fn tools_available() -> bool {
        Command::new("tiffcp")
            .arg("-h")
            .output()
            .map(|o| !o.stderr.is_empty() || o.status.success())
            .unwrap_or(false)
            && Command::new("python3")
                .args(["-c", "import tifffile, numpy"])
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
    }

    /// Build a three-channel RGB fixture. See [`libtiff_fixture_with`].
    #[allow(clippy::type_complexity)]
    fn libtiff_fixture(
        mode: &str,
        rows_per_strip: &str,
    ) -> Option<(Vec<u8>, Vec<Vec<u8>>, Vec<u8>, usize, usize)> {
        libtiff_fixture_with(mode, rows_per_strip, Photometric::Rgb)
    }

    /// Which colour layout `tiffcp` is asked to produce.
    #[derive(Clone, Copy)]
    enum Photometric {
        /// `photometric='rgb'`, three channels, YCbCr-transformed by libtiff.
        Rgb,
        /// `photometric='separated'`, four channels. libtiff writes these with
        /// `'C'`/`'M'`/`'Y'`/`'K'` component identifiers, **no** `APP14` and
        /// **no** inversion, which is the case `oxiarc-tiff` depends on.
        Separated,
    }

    impl Photometric {
        /// The `tifffile` keyword and the Pillow mode used to read it back.
        fn tags(self) -> (&'static str, &'static str) {
            match self {
                Photometric::Rgb => ("rgb", "RGB"),
                Photometric::Separated => ("separated", "CMYK"),
            }
        }
    }

    /// Build a JPEG-compressed TIFF with libtiff and dump its `JPEGTables`
    /// tag plus every strip through `tifffile`.
    ///
    /// Returns `(tables, strips, reference, width, height)`, where the
    /// reference is Pillow's own read of the same file.
    #[allow(clippy::type_complexity)]
    fn libtiff_fixture_with(
        mode: &str,
        rows_per_strip: &str,
        photometric: Photometric,
    ) -> Option<(Vec<u8>, Vec<Vec<u8>>, Vec<u8>, usize, usize)> {
        let (width, height) = (48usize, 40usize);
        let src = temp_path("src", "tif");
        let dst = temp_path("dst", "tif");
        let dump = temp_path("dump", "npz");

        let (tag, pillow_mode) = photometric.tags();
        // A constant fourth plane makes the inversion question decidable: an
        // Adobe-style inversion would turn K = 40 into K = 215.
        let fourth = match photometric {
            Photometric::Rgb => String::new(),
            Photometric::Separated => format!(
                "planes.append(np.full((h, w), {K}, dtype='uint8'))\n",
                K = CMYK_CONSTANT_K
            ),
        };
        let build = format!(
            "import numpy as np, tifffile\n\
             w, h = {width}, {height}\n\
             x = np.arange(w)[None, :]\n\
             y = np.arange(h)[:, None]\n\
             r = ((x * 5 + y * 3) % 256).astype('uint8')\n\
             g = (((x // 7 + y // 5) % 2) * 200 + 20).astype('uint8')\n\
             b = ((x * x + y * y) % 251).astype('uint8')\n\
             planes = list(np.broadcast_arrays(r, g, b))\n\
             {fourth}\
             img = np.stack(planes, axis=-1).astype('uint8')\n\
             tifffile.imwrite(r'{src}', img, photometric='{tag}')\n",
            src = src.display()
        );
        let status = Command::new("python3")
            .arg("-c")
            .arg(&build)
            .status()
            .ok()?;
        if !status.success() {
            return None;
        }

        let mut command = Command::new("tiffcp");
        command.arg("-c").arg(mode).arg("-r").arg(rows_per_strip);
        let status = command.arg(&src).arg(&dst).status().ok()?;
        if !status.success() {
            return None;
        }

        let extract = format!(
            "import numpy as np, tifffile\n\
             from PIL import Image\n\
             with tifffile.TiffFile(r'{dst}') as tf:\n\
             \x20   page = tf.pages[0]\n\
             \x20   tables = page.tags['JPEGTables'].value\n\
             \x20   fh = tf.filehandle\n\
             \x20   offsets = page.dataoffsets\n\
             \x20   counts = page.databytecounts\n\
             \x20   strips = []\n\
             \x20   for off, cnt in zip(offsets, counts):\n\
             \x20       fh.seek(off)\n\
             \x20       strips.append(np.frombuffer(fh.read(cnt), dtype='uint8'))\n\
             ref = np.asarray(Image.open(r'{dst}').convert('{pillow_mode}'), dtype='uint8')\n\
             np.savez(r'{dump}', tables=np.frombuffer(bytes(tables), dtype='uint8'),\n\
             \x20        ref=ref.reshape(-1), nstrips=np.array([len(strips)]),\n\
             \x20        **{{f'strip{{i}}': s for i, s in enumerate(strips)}})\n",
            dst = dst.display(),
            dump = dump.display()
        );
        let status = Command::new("python3")
            .arg("-c")
            .arg(&extract)
            .status()
            .ok()?;
        if !status.success() {
            return None;
        }

        let read = format!(
            "import numpy as np, sys\n\
             d = np.load(r'{dump}')\n\
             out = open(sys.argv[1], 'wb')\n\
             def put(a):\n\
             \x20   out.write(len(a).to_bytes(4, 'little')); out.write(bytes(a))\n\
             put(d['tables'])\n\
             n = int(d['nstrips'][0])\n\
             out.write(n.to_bytes(4, 'little'))\n\
             for i in range(n):\n\
             \x20   put(d[f'strip{{i}}'])\n\
             put(d['ref'])\n",
            dump = dump.display()
        );
        let blob = temp_path("blob", "bin");
        let status = Command::new("python3")
            .arg("-c")
            .arg(&read)
            .arg(&blob)
            .status()
            .ok()?;
        if !status.success() {
            return None;
        }
        let data = std::fs::read(&blob).ok()?;
        for path in [&src, &dst, &dump, &blob] {
            let _ = std::fs::remove_file(path);
        }

        let mut pos = 0usize;
        let take = |pos: &mut usize| -> Vec<u8> {
            let len =
                u32::from_le_bytes([data[*pos], data[*pos + 1], data[*pos + 2], data[*pos + 3]])
                    as usize;
            *pos += 4;
            let out = data[*pos..*pos + len].to_vec();
            *pos += len;
            out
        };
        let tables = take(&mut pos);
        let count =
            u32::from_le_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]) as usize;
        pos += 4;
        let strips: Vec<Vec<u8>> = (0..count).map(|_| take(&mut pos)).collect();
        let reference = take(&mut pos);
        Some((tables, strips, reference, width, height))
    }

    #[test]
    fn every_libtiff_strip_decodes_against_the_tables_tag() {
        if !tools_available() {
            eprintln!("skipping: tiffcp/tifffile unavailable");
            return;
        }
        for (mode, rows) in [("jpeg", "16"), ("jpeg:r", "8")] {
            let (tables_blob, strips, reference, width, height) = libtiff_fixture(mode, rows)
                .unwrap_or_else(|| panic!("{mode}: tiffcp fixture build failed"));
            assert!(!strips.is_empty());
            let tables = tiff::parse_jpeg_tables(&tables_blob).expect("JPEGTables");

            let mut assembled: Vec<u8> = Vec::with_capacity(width * height * 3);
            for strip in &strips {
                let mut probe = Decoder::new(strip.as_slice());
                probe.load_tables(&tables);
                let info = probe.read_info().expect("strip header");
                let mut out = vec![0u8; usize::from(info.width) * usize::from(info.height) * 3];
                decode_abbreviated_into(Some(&tables), strip, &DecodeOptions::default(), &mut out)
                    .expect("strip decode");
                assembled.extend_from_slice(&out);
            }
            assert_eq!(
                assembled.len(),
                reference.len(),
                "{mode}: assembled size disagrees with Pillow's read"
            );
            // libtiff's own YCbCr round-trip is lossy at the tag level, so
            // compare with a one-LSB tolerance rather than byte identity.
            let mut peak = 0i32;
            for (a, b) in assembled.iter().zip(reference.iter()) {
                peak = peak.max((i32::from(*a) - i32::from(*b)).abs());
            }
            assert!(peak <= 1, "{mode}: peak difference vs Pillow is {peak}");
        }
    }

    #[test]
    fn libtiff_tables_blob_is_reproduced_byte_for_byte() {
        if !tools_available() {
            eprintln!("skipping: tiffcp/tifffile unavailable");
            return;
        }
        let (tables_blob, _, _, _, _) =
            libtiff_fixture("jpeg", "16").expect("tiffcp fixture build failed");
        let tables = tiff::parse_jpeg_tables(&tables_blob).expect("JPEGTables");
        assert_eq!(
            tiff::build_jpeg_tables(&tables, TablesMode::BOTH),
            tables_blob,
            "our JPEGTables layout must match libtiff's byte for byte"
        );
    }
    /// Four-component CMYK end to end: `parse_sof` with `Nf = 4`, a
    /// four-component MCU walk, four upsamplers and a four-channel assemble.
    ///
    /// The discriminating assertion is the inversion rule. libtiff's
    /// `Compression = 7` CMYK strips carry no `APP14`, so this crate must
    /// **not** invert them, while Photoshop's standalone CMYK JPEGs (which do
    /// carry `APP14`) must be inverted. `oxiarc-tiff` depends on the first
    /// half; the flat `K` plane makes it a 175-count difference, not a
    /// rounding one.
    #[test]
    fn libtiff_cmyk_strips_decode_without_the_adobe_inversion() {
        if !tools_available() {
            eprintln!("skipping: tiffcp/tifffile unavailable");
            return;
        }
        let (tables_blob, strips, reference, width, height) =
            libtiff_fixture_with("jpeg", "16", Photometric::Separated)
                .expect("tiffcp CMYK fixture build failed");
        assert!(!strips.is_empty());
        let tables = tiff::parse_jpeg_tables(&tables_blob).expect("JPEGTables");

        let mut assembled: Vec<u8> = Vec::with_capacity(width * height * 4);
        for strip in &strips {
            let mut probe = Decoder::new(strip.as_slice());
            probe.load_tables(&tables);
            let info = probe.read_info().expect("strip header");
            assert_eq!(info.num_components, 4, "libtiff writes four channels");
            assert!(
                !info.has_adobe,
                "libtiff CMYK strips carry no APP14; the inversion must stay off"
            );
            assert_eq!(info.input_color_space, ColorSpace::Cmyk);
            assert_eq!(info.output_color_space, ColorSpace::Cmyk);
            assert_eq!(info.subsampling, (1, 1), "CMYK is written unsubsampled");
            let ids: Vec<u8> = info.components().iter().map(|c| c.id).collect();
            assert_eq!(ids, b"CMYK".to_vec(), "TIFF TN2 component identifiers");

            let mut out = vec![0u8; usize::from(info.width) * usize::from(info.height) * 4];
            decode_abbreviated_into(Some(&tables), strip, &DecodeOptions::default(), &mut out)
                .expect("CMYK strip decode");
            assembled.extend_from_slice(&out);
        }
        assert_eq!(
            assembled.len(),
            reference.len(),
            "assembled CMYK size disagrees with Pillow's read"
        );

        // The K plane round-trips its constant. Inverted output would read 215.
        let k: Vec<u8> = assembled.iter().skip(3).step_by(4).copied().collect();
        assert_eq!(k.len(), width * height);
        let low = k.iter().copied().min().unwrap_or(0);
        let high = k.iter().copied().max().unwrap_or(0);
        assert!(
            low >= CMYK_CONSTANT_K - 1 && high <= CMYK_CONSTANT_K + 1,
            "K plane is {low}..={high}, expected ~{CMYK_CONSTANT_K}; \
             {} suggests the Adobe inversion fired on a non-Adobe stream",
            if low > 200 { "215" } else { "this" }
        );

        let mut peak = 0i32;
        for (a, b) in assembled.iter().zip(reference.iter()) {
            peak = peak.max((i32::from(*a) - i32::from(*b)).abs());
        }
        assert!(peak <= 1, "CMYK peak difference vs Pillow is {peak}");
    }

    /// The other half of the rule: an `APP14` marker turns the inversion on.
    /// Photoshop writes CMYK JPEGs that way, and reading them without the
    /// inversion produces a photographic negative.
    #[test]
    fn an_adobe_app14_switches_the_cmyk_inversion_on() {
        if !tools_available() {
            eprintln!("skipping: tiffcp/tifffile unavailable");
            return;
        }
        let (tables_blob, strips, _, _, _) =
            libtiff_fixture_with("jpeg", "16", Photometric::Separated)
                .expect("tiffcp CMYK fixture build failed");
        let tables = tiff::parse_jpeg_tables(&tables_blob).expect("JPEGTables");
        let strip = &strips[0];

        let mut probe = Decoder::new(strip.as_slice());
        probe.load_tables(&tables);
        let strip_info = probe.read_info().expect("strip header");
        let mut plain =
            vec![0u8; usize::from(strip_info.width) * usize::from(strip_info.height) * 4];
        decode_abbreviated_into(Some(&tables), strip, &DecodeOptions::default(), &mut plain)
            .expect("plain");

        // Splice the tables in front so the stream is self-contained, then add
        // an Adobe APP14 with transform 0 (CMYK, not YCCK).
        let mut standalone = tiff::merge_jpeg_tables(&tables_blob, strip).expect("merge");
        standalone = with_adobe(&standalone, 0);
        let mut decoder = Decoder::new(standalone.as_slice());
        let info = decoder.read_info().expect("info");
        assert!(info.has_adobe);
        assert_eq!(info.adobe_transform, Some(0));
        assert_eq!(info.input_color_space, ColorSpace::Cmyk);
        let inverted = decoder.decode().expect("decode");

        assert_eq!(inverted.len(), plain.len());
        for (a, b) in inverted.iter().zip(plain.iter()) {
            assert_eq!(
                i32::from(*a),
                255 - i32::from(*b),
                "APP14 must invert every sample"
            );
        }
    }
    /// libjpeg's `build_ycc_rgb_table` / `ycc_rgb_convert`, recomputed here
    /// from the pinned constants so the assertion below is an independent
    /// derivation rather than a second call into the code under test.
    fn reference_ycc_rgb(y: i32, cb: i32, cr: i32) -> (i32, i32, i32) {
        const SCALEBITS: i32 = 16;
        const ONE_HALF: i64 = 1 << 15;
        const FIX_CR_R: i64 = 91_881; // FIX(1.40200)
        const FIX_CB_B: i64 = 116_130; // FIX(1.77200)
        const FIX_CR_G: i64 = 46_802; // FIX(0.71414)
        const FIX_CB_G: i64 = 22_554; // FIX(0.34414), the five-decimal spelling
        let (x_cb, x_cr) = (i64::from(cb) - 128, i64::from(cr) - 128);
        let cr_r = ((FIX_CR_R * x_cr + ONE_HALF) >> SCALEBITS) as i32;
        let cb_b = ((FIX_CB_B * x_cb + ONE_HALF) >> SCALEBITS) as i32;
        let g_sum = -FIX_CB_G * x_cb + ONE_HALF - FIX_CR_G * x_cr;
        let g_off = (g_sum >> SCALEBITS) as i32;
        (
            (y + cr_r).clamp(0, 255),
            (y + g_off).clamp(0, 255),
            (y + cb_b).clamp(0, 255),
        )
    }

    /// YCCK (`APP14` transform 2) end to end over a real four-component
    /// stream: the plane-to-transform wiring, not just the arithmetic.
    ///
    /// The oracle is self-contained on purpose. Pillow reads four-component
    /// Adobe JPEGs through its `CMYK;I` raw mode, so a uniform `255 - x`
    /// offset against Pillow would be its convention rather than this crate's
    /// bug. Instead the same bytes are decoded twice -- once untransformed --
    /// and the colour relationship is recomputed from the pinned constants.
    ///
    /// **Documented divergence.** libjpeg's `ycck_cmyk_convert` emits
    /// `(MAX - R, MAX - G, MAX - B, K)`; this crate applies the Adobe
    /// inversion uniformly to all four channels, so it emits the exact
    /// complement, `(R, G, B, MAX - K)`. `raw_components` bypasses the
    /// question, which is what `oxiarc-tiff` uses. See the note in TODO.md.
    #[test]
    fn ycck_transform_2_converts_all_four_planes() {
        if !tools_available() {
            eprintln!("skipping: tiffcp/tifffile unavailable");
            return;
        }
        let (tables_blob, strips, _, _, _) =
            libtiff_fixture_with("jpeg", "16", Photometric::Separated)
                .expect("tiffcp CMYK fixture build failed");
        let merged = tiff::merge_jpeg_tables(&tables_blob, &strips[0]).expect("merge");
        let ycck = with_adobe(&merged, 2);

        let mut probe = Decoder::new(ycck.as_slice());
        let info = probe.read_info().expect("info");
        assert_eq!(info.adobe_transform, Some(2));
        assert_eq!(
            info.input_color_space,
            ColorSpace::Ycck,
            "transform 2 must select YCCK"
        );
        assert_eq!(info.output_color_space, ColorSpace::Cmyk);
        let (w, h) = (usize::from(info.width), usize::from(info.height));

        // Untransformed planes: Y, Cb, Cr, K exactly as coded.
        let mut raw = Decoder::with_options(ycck.as_slice(), DecodeOptions::raw());
        raw.read_info().expect("info");
        let planes = raw.decode().expect("raw decode");

        let mut converted = Decoder::new(ycck.as_slice());
        converted.read_info().expect("info");
        let cmyk = converted.decode().expect("ycck decode");

        assert_eq!(planes.len(), w * h * 4);
        assert_eq!(cmyk.len(), planes.len());

        let mut k_differs_from_passthrough = 0usize;
        for i in 0..(w * h) {
            let (y, cb, cr, k) = (
                i32::from(planes[i * 4]),
                i32::from(planes[i * 4 + 1]),
                i32::from(planes[i * 4 + 2]),
                i32::from(planes[i * 4 + 3]),
            );
            let (r, g, b) = reference_ycc_rgb(y, cb, cr);
            // maxval - rgb, then the Adobe inversion, collapses to rgb.
            let expected = [r, g, b, 255 - k];
            let got = [
                i32::from(cmyk[i * 4]),
                i32::from(cmyk[i * 4 + 1]),
                i32::from(cmyk[i * 4 + 2]),
                i32::from(cmyk[i * 4 + 3]),
            ];
            assert_eq!(
                got, expected,
                "pixel {i}: YCCK({y},{cb},{cr},{k}) converted wrongly"
            );
            if got[3] != k {
                k_differs_from_passthrough += 1;
            }
        }
        // The K channel really is routed through the inversion, not passed
        // through: that is the divergence from libjpeg this test pins.
        assert!(
            k_differs_from_passthrough > 0,
            "K was passed through unchanged; the documented convention says \
             it is inverted, so one of the two is now stale"
        );
    }
}
