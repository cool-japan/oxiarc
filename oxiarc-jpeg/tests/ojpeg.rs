//! Legacy OJPEG (`Compression = 6`) reconstruction, without external tools.
//!
//! Every fixture here is built by taking a datastream this crate encoded and
//! *dismantling* it the way a TIFF 6.0 writer would have: the quantisation
//! values, the Huffman tables and the entropy bytes are pulled apart into
//! tags, and the reconstruction has to put them back together well enough to
//! decode to the very same pixels. Comparing against the original decode is
//! what makes these tests non-vacuous — flavours (b) and (c) have no
//! reference decoder anywhere (libtiff itself fails on them, see
//! `ojpeg_oracle.rs`).

use oxiarc_jpeg::tiff::{
    OJpegGeometry, OJpegTags, decode_ojpeg, decode_ojpeg_into, reconstruct_ojpeg,
};
use oxiarc_jpeg::{
    DecodeOptions, Decoder, EncodeOptions, EncodeProcess, InputColor, JpegError, MarkerPolicy,
    RestartInterval, Subsampling, encode_to_vec_with_options,
};

/// A source image with flat regions, hard edges and a gradient.
fn source(width: usize, height: usize, channels: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(width * height * channels);
    for y in 0..height {
        for x in 0..width {
            let values = [
                ((x * 7 + y * 13) % 256) as u8,
                if (x / 5 + y / 3) % 2 == 0 { 255 } else { 32 },
                ((x * 3 + y * 11) % 256) as u8,
            ];
            for c in 0..channels {
                out.push(values[c % 3]);
            }
        }
    }
    out
}

/// The pieces a TIFF 6.0 writer would have scattered across tags 519, 520,
/// 521 and the strip.
struct Dismantled {
    q_tables: Vec<Vec<u8>>,
    dc_tables: Vec<Vec<u8>>,
    ac_tables: Vec<Vec<u8>>,
    /// Bytes from the first entropy byte to the `EOI`, exclusive.
    entropy: Vec<u8>,
    /// The `SOS` segment, marker included.
    scan_header: Vec<u8>,
    restart_interval: u16,
}

/// Pull a datastream apart into the tag payloads OJPEG stores.
fn dismantle(jpeg: &[u8]) -> Dismantled {
    let mut out = Dismantled {
        q_tables: Vec::new(),
        dc_tables: Vec::new(),
        ac_tables: Vec::new(),
        entropy: Vec::new(),
        scan_header: Vec::new(),
        restart_interval: 0,
    };
    let mut i = 2usize;
    while i + 3 < jpeg.len() {
        assert_eq!(jpeg[i], 0xFF, "expected a marker at {i}");
        let code = jpeg[i + 1];
        if code == 0xD8 {
            i += 2;
            continue;
        }
        let length = usize::from(u16::from_be_bytes([jpeg[i + 2], jpeg[i + 3]]));
        let payload = &jpeg[i + 4..i + 2 + length];
        match code {
            // `Pq/Tq` then 64 zig-zag values; OJPEG stores only the values.
            0xDB => {
                let mut rest = payload;
                while rest.len() >= 65 {
                    out.q_tables.push(rest[1..65].to_vec());
                    rest = &rest[65..];
                }
            }
            // `Tc/Th` then BITS and HUFFVAL; OJPEG stores only the latter two.
            0xC4 => {
                let mut rest = payload;
                while rest.len() >= 17 {
                    let class = rest[0] >> 4;
                    let counted: usize = rest[1..17].iter().map(|&n| usize::from(n)).sum();
                    let table = rest[1..17 + counted].to_vec();
                    if class == 0 {
                        out.dc_tables.push(table);
                    } else {
                        out.ac_tables.push(table);
                    }
                    rest = &rest[17 + counted..];
                }
            }
            0xDD => out.restart_interval = u16::from_be_bytes([payload[0], payload[1]]),
            0xDA => {
                out.scan_header = jpeg[i..i + 2 + length].to_vec();
                let start = i + 2 + length;
                let end = jpeg.len().saturating_sub(2);
                out.entropy = jpeg[start..end].to_vec();
                return out;
            }
            _ => {}
        }
        i += 2 + length;
    }
    panic!("no SOS in the fixture");
}

/// Encode one image the way a TIFF 6.0 writer would have: no `JFIF`, no
/// `Adobe`, sequential baseline.
fn encode(
    pixels: &[u8],
    width: u16,
    height: u16,
    color: InputColor,
    options: &EncodeOptions,
) -> Vec<u8> {
    encode_to_vec_with_options(pixels, width, height, color, options).expect("encode")
}

fn baseline_options(subsampling: Subsampling) -> EncodeOptions {
    EncodeOptions {
        quality: 80,
        subsampling,
        write_jfif: MarkerPolicy::Never,
        write_adobe: MarkerPolicy::Never,
        ..Default::default()
    }
}

fn decode_raw(jpeg: &[u8]) -> Vec<u8> {
    let mut decoder = Decoder::with_options(jpeg, DecodeOptions::raw());
    decoder.read_info().expect("read_info");
    decoder.decode().expect("decode")
}

/// Flavour (b), the TIFF 6.0 spec form: the strip is bare entropy data and
/// every table arrives through a tag.
#[test]
fn spec_form_strips_reconstruct_and_decode() {
    for &(width, height, channels, subsampling) in &[
        (64usize, 48usize, 1usize, Subsampling::S444),
        (64, 48, 3, Subsampling::S444),
        (64, 48, 3, Subsampling::S420),
        (37, 23, 3, Subsampling::S422),
    ] {
        let pixels = source(width, height, channels);
        let color = if channels == 1 {
            InputColor::Luma
        } else {
            InputColor::Rgb
        };
        let jpeg = encode(
            &pixels,
            width as u16,
            height as u16,
            color,
            &baseline_options(subsampling),
        );
        let expected = decode_raw(&jpeg);
        let parts = dismantle(&jpeg);

        let tags = OJpegTags {
            jpeg_proc: Some(1),
            q_tables: parts.q_tables.clone(),
            dc_tables: parts.dc_tables.clone(),
            ac_tables: parts.ac_tables.clone(),
            ..Default::default()
        };
        let (h, v) = match subsampling {
            Subsampling::S444 => (1u8, 1u8),
            Subsampling::S422 => (2, 1),
            _ => (2, 2),
        };
        let geometry = OJpegGeometry {
            width: width as u16,
            height: height as u16,
            bits_per_sample: 8,
            samples_per_pixel: channels as u8,
            photometric: if channels == 1 { 1 } else { 6 },
            subsampling: (h, v),
            planar_config: 1,
        };

        let (info, decoded) = decode_ojpeg(&tags, &geometry, &parts.entropy, &DecodeOptions::raw())
            .expect("decode_ojpeg");
        assert_eq!((info.width, info.height), (width as u16, height as u16));
        assert_eq!(
            decoded, expected,
            "{width}x{height} x{channels} {subsampling:?} did not reconstruct"
        );
    }
}

/// Flavour (c): the strip starts at its own `SOS`, with the tables in tags.
#[test]
fn hybrid_strips_keep_their_own_scan_header() {
    let pixels = source(48, 32, 3);
    let jpeg = encode(
        &pixels,
        48,
        32,
        InputColor::Rgb,
        &baseline_options(Subsampling::S420),
    );
    let expected = decode_raw(&jpeg);
    let parts = dismantle(&jpeg);

    let mut strip = parts.scan_header.clone();
    strip.extend_from_slice(&parts.entropy);

    let tags = OJpegTags {
        jpeg_proc: Some(1),
        q_tables: parts.q_tables,
        dc_tables: parts.dc_tables,
        ac_tables: parts.ac_tables,
        ..Default::default()
    };
    let geometry = OJpegGeometry {
        width: 48,
        height: 32,
        samples_per_pixel: 3,
        photometric: 6,
        subsampling: (2, 2),
        ..Default::default()
    };
    let (_, decoded) =
        decode_ojpeg(&tags, &geometry, &strip, &DecodeOptions::raw()).expect("decode_ojpeg");
    assert_eq!(decoded, expected);
}

/// Flavour (a): tags 513/514 carry a whole datastream and the strip is empty.
#[test]
fn interchange_streams_decode_whole() {
    let pixels = source(40, 24, 3);
    let jpeg = encode(
        &pixels,
        40,
        24,
        InputColor::Rgb,
        &baseline_options(Subsampling::S420),
    );
    let expected = decode_raw(&jpeg);

    let tags = OJpegTags {
        jpeg_proc: Some(1),
        interchange: Some(&jpeg),
        ..Default::default()
    };
    let geometry = OJpegGeometry {
        width: 40,
        height: 24,
        samples_per_pixel: 3,
        photometric: 6,
        subsampling: (2, 2),
        ..Default::default()
    };
    let stream = reconstruct_ojpeg(&tags, &geometry, &[]).expect("reconstruct");
    assert_eq!(
        stream, jpeg,
        "an empty strip must return the stream as it is"
    );
    let (_, decoded) =
        decode_ojpeg(&tags, &geometry, &[], &DecodeOptions::raw()).expect("decode_ojpeg");
    assert_eq!(decoded, expected);
}

/// A flavour (a) file whose strips are bare entropy data: the tables come
/// from the interchange stream's own header, not from tags at all.
#[test]
fn interchange_headers_serve_bare_strips() {
    let pixels = source(32, 16, 1);
    let jpeg = encode(
        &pixels,
        32,
        16,
        InputColor::Luma,
        &baseline_options(Subsampling::S444),
    );
    let expected = decode_raw(&jpeg);
    let parts = dismantle(&jpeg);

    let tags = OJpegTags {
        jpeg_proc: Some(1),
        interchange: Some(&jpeg),
        ..Default::default()
    };
    let geometry = OJpegGeometry {
        width: 32,
        height: 16,
        samples_per_pixel: 1,
        photometric: 1,
        ..Default::default()
    };
    let (_, decoded) = decode_ojpeg(&tags, &geometry, &parts.entropy, &DecodeOptions::raw())
        .expect("decode_ojpeg");
    assert_eq!(decoded, expected);

    // And with no tables anywhere, reconstruction produces a stream that
    // cannot decode rather than a wrong image.
    let bare = OJpegTags {
        jpeg_proc: Some(1),
        ..Default::default()
    };
    let stream = reconstruct_ojpeg(&bare, &geometry, &parts.entropy).expect("reconstruct");
    assert!(Decoder::new(stream.as_slice()).decode().is_err());
}

/// Tag 515 supplies the restart interval a marker-free strip cannot carry.
#[test]
fn the_restart_interval_tag_reaches_the_synthesised_stream() {
    let pixels = source(64, 32, 1);
    let mut options = baseline_options(Subsampling::S444);
    options.restart_interval = RestartInterval::McuRows(1);
    let jpeg = encode(&pixels, 64, 32, InputColor::Luma, &options);
    let expected = decode_raw(&jpeg);
    let parts = dismantle(&jpeg);
    assert_eq!(
        parts.restart_interval, 8,
        "8 MCUs per row at 64 pixels wide"
    );

    let tags = OJpegTags {
        jpeg_proc: Some(1),
        restart_interval: Some(parts.restart_interval),
        q_tables: parts.q_tables.clone(),
        dc_tables: parts.dc_tables.clone(),
        ac_tables: parts.ac_tables.clone(),
        ..Default::default()
    };
    let geometry = OJpegGeometry {
        width: 64,
        height: 32,
        samples_per_pixel: 1,
        ..Default::default()
    };
    let (_, decoded) = decode_ojpeg(&tags, &geometry, &parts.entropy, &DecodeOptions::raw())
        .expect("decode_ojpeg");
    assert_eq!(decoded, expected);

    // Without the tag the same strip decodes to something else entirely,
    // which is what makes the assertion above load-bearing.
    let without = OJpegTags {
        restart_interval: None,
        ..tags.clone()
    };
    let other = decode_ojpeg(&without, &geometry, &parts.entropy, &DecodeOptions::raw());
    if let Ok((_, pixels)) = other {
        assert_ne!(pixels, expected);
    }
}

/// `JPEGProc = 14` selects the lossless process, and tags 517/518 supply the
/// predictor and point transform the synthesised `SOS` needs.
#[test]
fn lossless_ojpeg_strips_reconstruct() {
    for predictor in [1u16, 4, 7] {
        let pixels = source(24, 16, 1);
        let options = EncodeOptions {
            process: EncodeProcess::Lossless {
                predictor: predictor as u8,
                point_transform: 0,
            },
            write_jfif: MarkerPolicy::Never,
            write_adobe: MarkerPolicy::Never,
            ..Default::default()
        };
        let jpeg = encode(&pixels, 24, 16, InputColor::Luma, &options);
        let parts = dismantle(&jpeg);

        let tags = OJpegTags {
            jpeg_proc: Some(14),
            lossless_predictors: vec![predictor],
            point_transforms: vec![0],
            dc_tables: parts.dc_tables.clone(),
            ..Default::default()
        };
        let geometry = OJpegGeometry {
            width: 24,
            height: 16,
            samples_per_pixel: 1,
            ..Default::default()
        };
        let stream = reconstruct_ojpeg(&tags, &geometry, &parts.entropy).expect("reconstruct");
        assert!(
            stream.windows(2).any(|w| w[0] == 0xFF && w[1] == 0xC3),
            "JPEGProc 14 must synthesise a SOF3"
        );
        let (_, decoded) = decode_ojpeg(&tags, &geometry, &parts.entropy, &DecodeOptions::raw())
            .expect("decode_ojpeg");
        assert_eq!(decoded, pixels, "predictor {predictor}");
    }
}

/// The reconstruction refuses what it cannot make sense of, rather than
/// producing a stream that decodes to garbage.
#[test]
fn malformed_tags_are_named_errors() {
    let geometry = OJpegGeometry {
        width: 8,
        height: 8,
        samples_per_pixel: 1,
        ..Default::default()
    };
    let strip = [0u8; 16];

    // A quantisation table that is not 64 bytes.
    let tags = OJpegTags {
        q_tables: vec![vec![0u8; 63]],
        ..Default::default()
    };
    assert!(reconstruct_ojpeg(&tags, &geometry, &strip).is_err());

    // A Huffman table shorter than its own BITS counts claim.
    let mut bits = vec![0u8; 16];
    bits[0] = 5;
    let tags = OJpegTags {
        dc_tables: vec![bits],
        ..Default::default()
    };
    assert!(reconstruct_ojpeg(&tags, &geometry, &strip).is_err());

    // An undefined JPEGProc.
    let tags = OJpegTags {
        jpeg_proc: Some(2),
        ..Default::default()
    };
    assert!(reconstruct_ojpeg(&tags, &geometry, &strip).is_err());

    // An empty strip with no interchange stream to fall back on.
    let tags = OJpegTags::default();
    assert!(reconstruct_ojpeg(&tags, &geometry, &[]).is_err());

    // A zero-sized frame.
    let tags = OJpegTags {
        q_tables: vec![vec![1u8; 64]],
        dc_tables: vec![vec![0u8; 16]],
        ac_tables: vec![vec![0u8; 16]],
        ..Default::default()
    };
    let empty_geometry = OJpegGeometry {
        width: 0,
        ..geometry
    };
    assert!(reconstruct_ojpeg(&tags, &empty_geometry, &strip).is_err());
}

/// A short tag array repeats its last entry, which is how libtiff tolerates
/// the files that store one table for three components.
#[test]
fn short_table_arrays_repeat_their_last_entry() {
    let pixels = source(16, 16, 3);
    // 4:4:4 with a single quantiser and one Huffman pair, so every component
    // legitimately shares slot 0.
    let options = EncodeOptions {
        quality: 80,
        subsampling: Subsampling::S444,
        jpeg_color_space: Some(oxiarc_jpeg::ColorSpace::Rgb),
        write_jfif: MarkerPolicy::Never,
        write_adobe: MarkerPolicy::Never,
        ..Default::default()
    };
    let jpeg = encode(&pixels, 16, 16, InputColor::Rgb, &options);
    let expected = decode_raw(&jpeg);
    let parts = dismantle(&jpeg);
    assert_eq!(parts.q_tables.len(), 1, "an RGB frame shares one quantiser");

    let tags = OJpegTags {
        jpeg_proc: Some(1),
        q_tables: parts.q_tables,
        dc_tables: parts.dc_tables,
        ac_tables: parts.ac_tables,
        ..Default::default()
    };
    let geometry = OJpegGeometry {
        width: 16,
        height: 16,
        samples_per_pixel: 3,
        photometric: 2,
        ..Default::default()
    };
    let (_, decoded) = decode_ojpeg(&tags, &geometry, &parts.entropy, &DecodeOptions::raw())
        .expect("decode_ojpeg");
    assert_eq!(decoded, expected);
}

/// Flavour (c) at its most literal: the strip begins at the restart marker
/// that separated it from the strip before it.
///
/// libtiff's `tif_ojpeg.c` scans for whatever the strip has and resynchronises
/// on that marker; the module documents the same strategy ("often `SOS`,
/// sometimes an `RSTn`"). A strip is decoded on its own, with the predictions
/// and the restart counter reset by the synthesised `SOS`, so a leading marker
/// is not a boundary the decoder is waiting for — passing it through stops the
/// entropy decoder before the first MCU and the whole strip decodes to
/// nothing.
#[test]
fn a_strip_that_begins_at_a_restart_marker_still_decodes() {
    let pixels = source(64, 48, 3);
    let jpeg = encode(
        &pixels,
        64,
        48,
        InputColor::Rgb,
        &baseline_options(Subsampling::S420),
    );
    let expected = decode_raw(&jpeg);
    let pieces = dismantle(&jpeg);
    let geometry = OJpegGeometry {
        width: 64,
        height: 48,
        bits_per_sample: 8,
        samples_per_pixel: 3,
        photometric: 6,
        subsampling: (2, 2),
        planar_config: 1,
    };
    let tags = OJpegTags {
        jpeg_proc: Some(1),
        q_tables: pieces.q_tables.clone(),
        dc_tables: pieces.dc_tables.clone(),
        ac_tables: pieces.ac_tables.clone(),
        ..Default::default()
    };

    for lead in [
        vec![0xFFu8, 0xD0],
        vec![0xFF, 0xD7],
        vec![0xFF, 0xD0, 0xFF, 0xD1],
        vec![0xFF, 0xFF, 0xD3],
    ] {
        let mut strip = lead.clone();
        strip.extend_from_slice(&pieces.entropy);
        let (info, decoded) =
            decode_ojpeg(&tags, &geometry, &strip, &DecodeOptions::raw()).expect("decode");
        assert_eq!((info.width, info.height), (64, 48));
        assert_eq!(
            decoded, expected,
            "a strip led by {lead:02X?} decoded differently"
        );
    }

    // The same marker in front of a strip that carries its own scan header.
    let mut strip = vec![0xFFu8, 0xD2];
    strip.extend_from_slice(&pieces.scan_header);
    strip.extend_from_slice(&pieces.entropy);
    let (_, decoded) =
        decode_ojpeg(&tags, &geometry, &strip, &DecodeOptions::raw()).expect("decode");
    assert_eq!(
        decoded, expected,
        "a marker before the SOS changed the decode"
    );
}

/// A malformed tag 521 must name tag 521.
///
/// Both Huffman arrays go through one emitter, and it used to report
/// `JPEGDCTables` for either class, so a broken AC array sent the reader
/// looking at the wrong tag.
#[test]
fn a_malformed_ac_table_names_its_own_tag() {
    let geometry = OJpegGeometry {
        width: 8,
        height: 8,
        samples_per_pixel: 1,
        ..Default::default()
    };
    let strip = [0u8; 16];
    let mut bits = vec![0u8; 16];
    bits[3] = 7; // seven codes of length four, and none of them present
    let tags = OJpegTags {
        q_tables: vec![vec![1u8; 64]],
        dc_tables: vec![vec![0u8; 16]],
        ac_tables: vec![bits],
        ..Default::default()
    };
    let error = reconstruct_ojpeg(&tags, &geometry, &strip).expect_err("must be rejected");
    assert!(
        matches!(
            error,
            JpegError::MalformedSegment {
                segment: "JPEGACTables",
                ..
            }
        ),
        "the AC table error named {error:?}"
    );

    let tags = OJpegTags {
        q_tables: vec![vec![1u8; 64]],
        dc_tables: vec![vec![0u8; 8]],
        ..Default::default()
    };
    let error = reconstruct_ojpeg(&tags, &geometry, &strip).expect_err("must be rejected");
    assert!(
        matches!(
            error,
            JpegError::MalformedSegment {
                segment: "JPEGDCTables",
                ..
            }
        ),
        "the DC table error named {error:?}"
    );
}

/// Nothing a TIFF file can put in these tags may panic.
///
/// Every field here is attacker-controlled: the tag values come from the IFD
/// and the strip from the file. The reconstruction runs over a truncation of a
/// real strip at every offset, over bit-flipped strips, over marker soup, and
/// over tag arrays of every length from empty to longer than the standard
/// allows, in every combination with a geometry that disagrees with them. The
/// only requirement is that each call returns.
#[test]
fn no_tag_or_strip_can_panic_the_reconstruction() {
    let pixels = source(32, 24, 3);
    let jpeg = encode(
        &pixels,
        32,
        24,
        InputColor::Rgb,
        &baseline_options(Subsampling::S420),
    );
    let pieces = dismantle(&jpeg);

    let geometries = [
        OJpegGeometry::default(),
        OJpegGeometry {
            width: 32,
            height: 24,
            samples_per_pixel: 3,
            photometric: 6,
            subsampling: (2, 2),
            ..Default::default()
        },
        OJpegGeometry {
            width: u16::MAX,
            height: u16::MAX,
            bits_per_sample: 16,
            samples_per_pixel: 4,
            photometric: 6,
            subsampling: (4, 4),
            planar_config: 2,
        },
        OJpegGeometry {
            width: 1,
            height: 1,
            bits_per_sample: 0,
            samples_per_pixel: 255,
            photometric: 65535,
            subsampling: (0, 0),
            planar_config: 65535,
        },
    ];

    let mut strips: Vec<Vec<u8>> = Vec::new();
    // Truncations of a real strip, at every offset up to 300 and then sparsely.
    let full = {
        let mut s = pieces.scan_header.clone();
        s.extend_from_slice(&pieces.entropy);
        s
    };
    for cut in (0..full.len().min(300)).chain((300..full.len()).step_by(37)) {
        strips.push(full[..cut].to_vec());
    }
    // Marker soup and pathological headers.
    strips.push(vec![0xFF]);
    strips.push(vec![0xFF, 0xFF]);
    strips.push(vec![0xFF, 0xD8]);
    strips.push(vec![0xFF, 0xD0, 0xFF, 0xD0, 0xFF, 0xD0]);
    strips.push(vec![0xFF, 0xC4, 0xFF, 0xFF]);
    strips.push(vec![0xFF, 0xDA, 0x00, 0x00]);
    strips.push(vec![0xFF, 0xC0, 0xFF, 0xFF, 0x08]);
    strips.push(vec![0xFF, 0xDB, 0x00, 0x02]);
    strips.push(vec![0x00; 64]);
    let mut flipped = full.clone();
    for (i, byte) in flipped.iter_mut().enumerate() {
        *byte ^= ((i * 31) % 251) as u8;
    }
    strips.push(flipped);

    let tag_sets: Vec<OJpegTags<'_>> = vec![
        OJpegTags::default(),
        OJpegTags {
            jpeg_proc: Some(1),
            q_tables: pieces.q_tables.clone(),
            dc_tables: pieces.dc_tables.clone(),
            ac_tables: pieces.ac_tables.clone(),
            restart_interval: Some(u16::MAX),
            ..Default::default()
        },
        OJpegTags {
            jpeg_proc: Some(14),
            lossless_predictors: vec![u16::MAX; 8],
            point_transforms: vec![u16::MAX; 8],
            q_tables: vec![Vec::new(); 8],
            dc_tables: vec![vec![0xFFu8; 17]; 8],
            ac_tables: vec![vec![0xFFu8; 300]; 8],
            ..Default::default()
        },
        OJpegTags {
            jpeg_proc: Some(65535),
            q_tables: vec![vec![0u8; 64], vec![0u8; 65]],
            dc_tables: vec![vec![0u8; 15]],
            ac_tables: vec![vec![0u8; 16]],
            ..Default::default()
        },
        OJpegTags {
            jpeg_proc: Some(1),
            interchange: Some(&jpeg[..jpeg.len() / 2]),
            ..Default::default()
        },
    ];

    let mut calls = 0usize;
    for tags in &tag_sets {
        for geometry in &geometries {
            for strip in &strips {
                calls += 1;
                if let Ok(stream) = reconstruct_ojpeg(tags, geometry, strip) {
                    // Reconstruction succeeding says nothing about the stream
                    // being decodable; the decode must survive it too.
                    let mut out = vec![0u8; 4096];
                    let _ =
                        decode_ojpeg_into(tags, geometry, strip, &DecodeOptions::raw(), &mut out);
                    let _ = decode_ojpeg(tags, geometry, strip, &DecodeOptions::raw());
                    assert!(
                        stream.len() >= 4,
                        "a reconstructed stream is at least SOI+EOI"
                    );
                }
            }
        }
    }
    assert!(calls > 5_000, "the sweep only made {calls} calls");
}
