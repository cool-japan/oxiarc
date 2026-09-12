//! Differential tests for legacy OJPEG (`Compression = 6`).
//!
//! Two oracles, because no single one covers the format:
//!
//! * **flavour (a)** — a hand-built TIFF whose tags 513/514 carry a whole
//!   datastream is read back through **libtiff** (via Pillow, which uses it
//!   for TIFF), and our reconstruction has to produce the same pixels.
//! * **flavours (b) and (c)** — libtiff *fails* on these (`OJPEGSetupDecode`
//!   warns and then "can't read strip 0"), so the oracle is constructive: a
//!   `cjpeg` datastream is dismantled into tag payloads the way a TIFF 6.0
//!   writer would have, and our reconstruction must decode to exactly what
//!   `djpeg -dct int` produces for the original. That is a stronger check
//!   than a libtiff round-trip would have been, because the expected pixels
//!   come from a decoder that never sees our reconstruction.
//!
//! Gated behind `jpeg-oracle`; self-skips when the tools are missing.
#![cfg(feature = "jpeg-oracle")]

#[allow(dead_code)]
mod oracle_support;

use oracle_support::{cjpeg, djpeg, first_difference, libjpeg_version, synthetic, temp_path};
use oxiarc_jpeg::DecodeOptions;
use oxiarc_jpeg::tiff::{OJpegGeometry, OJpegTags, decode_ojpeg};
use std::process::Command;

fn tools_available() -> bool {
    oracle_support::tool_available("cjpeg") && oracle_support::tool_available("djpeg")
}

fn pillow_available() -> bool {
    Command::new("python3")
        .args(["-c", "import PIL"])
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

/// The tag payloads a TIFF 6.0 writer would have stored, pulled out of a
/// complete datastream.
struct Dismantled {
    q_tables: Vec<Vec<u8>>,
    dc_tables: Vec<Vec<u8>>,
    ac_tables: Vec<Vec<u8>>,
    entropy: Vec<u8>,
    restart_interval: u16,
}

fn dismantle(jpeg: &[u8]) -> Dismantled {
    let mut out = Dismantled {
        q_tables: Vec::new(),
        dc_tables: Vec::new(),
        ac_tables: Vec::new(),
        entropy: Vec::new(),
        restart_interval: 0,
    };
    let mut i = 2usize;
    while i + 3 < jpeg.len() {
        assert_eq!(jpeg[i], 0xFF);
        let code = jpeg[i + 1];
        if code == 0xD8 {
            i += 2;
            continue;
        }
        let length = usize::from(u16::from_be_bytes([jpeg[i + 2], jpeg[i + 3]]));
        let payload = &jpeg[i + 4..i + 2 + length];
        match code {
            0xDB => {
                let mut rest = payload;
                while rest.len() >= 65 {
                    assert_eq!(rest[0] >> 4, 0, "OJPEG cannot carry 16-bit quantisers");
                    out.q_tables.push(rest[1..65].to_vec());
                    rest = &rest[65..];
                }
            }
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
                let start = i + 2 + length;
                out.entropy = jpeg[start..jpeg.len() - 2].to_vec();
                return out;
            }
            _ => {}
        }
        i += 2 + length;
    }
    panic!("no SOS in the cjpeg fixture");
}

/// Invariant OJPEG-2: a dismantled `cjpeg` stream, put back together from
/// tags, decodes to exactly what `djpeg` produces for the original.
#[test]
fn spec_form_reconstruction_matches_djpeg() {
    if !tools_available() {
        eprintln!("skipping: cjpeg/djpeg unavailable");
        return;
    }
    let mut compared = 0usize;
    for &(width, height, channels) in &[(64usize, 48usize, 1usize), (64, 48, 3), (37, 23, 3)] {
        for &sample in &["1x1", "2x2", "2x1"] {
            for &restart in &[false, true] {
                let source = synthetic(width, height, channels, 255);
                let mut args = vec!["-quality", "80", "-dct", "int"];
                if channels == 1 {
                    args.push("-grayscale");
                } else {
                    args.push("-sample");
                    args.push(sample);
                }
                if restart {
                    args.push("-restart");
                    args.push("2B");
                }
                let Some(jpeg) = cjpeg(&args, &source) else {
                    continue;
                };
                let reference = djpeg(&["-dct", "int", "-pnm"], &jpeg).expect("djpeg");
                let parts = dismantle(&jpeg);
                let (h, v) = match (channels, sample) {
                    (1, _) => (1u8, 1u8),
                    (_, "2x2") => (2, 2),
                    (_, "2x1") => (2, 1),
                    _ => (1, 1),
                };
                let tags = OJpegTags {
                    jpeg_proc: Some(1),
                    restart_interval: Some(parts.restart_interval),
                    q_tables: parts.q_tables,
                    dc_tables: parts.dc_tables,
                    ac_tables: parts.ac_tables,
                    ..Default::default()
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
                // `raw_components` is off here on purpose: the reference is
                // `djpeg`, which applies the YCbCr transform, and a TIFF
                // reader would apply its own.
                let (_, decoded) =
                    decode_ojpeg(&tags, &geometry, &parts.entropy, &DecodeOptions::default())
                        .expect("decode_ojpeg");
                let ours: Vec<u16> = decoded.into_iter().map(u16::from).collect();
                if let Some((index, a, b)) = first_difference(&ours, &reference.samples) {
                    panic!(
                        "OJPEG spec form {width}x{height} x{channels} {sample} restart={restart}: \
                         sample {index} differs (ours {a}, djpeg {b}); reference: {}",
                        libjpeg_version()
                    );
                }
                compared += 1;
            }
        }
    }
    assert!(
        compared > 0,
        "no OJPEG fixture was built, so the test proved nothing ({})",
        libjpeg_version()
    );
}

/// Invariant OJPEG-1: a flavour (a) file that **libtiff itself** can read
/// decodes to the same pixels through our reconstruction.
#[test]
fn interchange_flavour_matches_libtiff() {
    if !tools_available() {
        eprintln!("skipping: cjpeg/djpeg unavailable");
        return;
    }
    if !pillow_available() {
        eprintln!("skipping: Pillow (and with it libtiff) unavailable");
        return;
    }

    let builder = r#"
import struct, sys
jpeg = open(sys.argv[1], 'rb').read()
width, height, spp, photometric = (int(v) for v in sys.argv[2:6])
out = bytearray(b'II*\x00')
out += struct.pack('<I', 8)
entries = []
def entry(tag, typ, count, value):
    entries.append(struct.pack('<HHI', tag, typ, count) + value)
jpeg_off = 8 + 2 + 12 * 14 + 4 + 16
entry(256, 3, 1, struct.pack('<HH', width, 0))
entry(257, 3, 1, struct.pack('<HH', height, 0))
entry(258, 3, 1, struct.pack('<HH', 8, 0))
entry(259, 3, 1, struct.pack('<HH', 6, 0))
entry(262, 3, 1, struct.pack('<HH', photometric, 0))
entry(273, 4, 1, struct.pack('<I', jpeg_off))
entry(277, 3, 1, struct.pack('<HH', spp, 0))
entry(278, 3, 1, struct.pack('<HH', height, 0))
entry(279, 4, 1, struct.pack('<I', len(jpeg)))
entry(284, 3, 1, struct.pack('<HH', 1, 0))
entry(512, 3, 1, struct.pack('<HH', 1, 0))
entry(513, 4, 1, struct.pack('<I', jpeg_off))
entry(514, 4, 1, struct.pack('<I', len(jpeg)))
entry(530, 3, 2, struct.pack('<HH', 2, 2))
body = bytearray(struct.pack('<H', len(entries)))
for e in sorted(entries, key=lambda e: struct.unpack('<H', e[:2])[0]):
    body += e
body += struct.pack('<I', 0)
body += b'\x00' * (jpeg_off - (8 + len(body)))
out += body
out += jpeg
open(sys.argv[6], 'wb').write(bytes(out))

from PIL import Image
im = Image.open(sys.argv[6])
assert im.tag_v2.get(259) == 6, 'the fixture is not Compression 6'
mode = 'RGB' if spp == 3 else 'L'
open(sys.argv[7], 'wb').write(im.convert(mode).tobytes())
"#;

    let mut compared = 0usize;
    for &(width, height, channels) in &[(64usize, 48usize, 1usize), (64, 48, 3)] {
        let source = synthetic(width, height, channels, 255);
        let mut args = vec!["-quality", "80", "-dct", "int"];
        if channels == 1 {
            args.push("-grayscale");
        } else {
            args.push("-sample");
            args.push("2x2");
        }
        let Some(jpeg) = cjpeg(&args, &source) else {
            continue;
        };
        let jpeg_path = temp_path("ojpeg_src", "jpg");
        let tiff_path = temp_path("ojpeg_fixture", "tif");
        let raw_path = temp_path("ojpeg_libtiff", "bin");
        std::fs::write(&jpeg_path, &jpeg).expect("write jpeg");
        let status = Command::new("python3")
            .arg("-c")
            .arg(builder)
            .arg(&jpeg_path)
            .arg(width.to_string())
            .arg(height.to_string())
            .arg(channels.to_string())
            .arg(if channels == 1 { "1" } else { "6" })
            .arg(&tiff_path)
            .arg(&raw_path)
            .status()
            .expect("run python3");
        let _ = std::fs::remove_file(&jpeg_path);
        if !status.success() {
            eprintln!("skipping: libtiff refused the OJPEG fixture");
            let _ = std::fs::remove_file(&tiff_path);
            continue;
        }
        let theirs = std::fs::read(&raw_path).expect("read libtiff output");
        let _ = std::fs::remove_file(&tiff_path);
        let _ = std::fs::remove_file(&raw_path);

        let tags = OJpegTags {
            jpeg_proc: Some(1),
            interchange: Some(&jpeg),
            ..Default::default()
        };
        let geometry = OJpegGeometry {
            width: width as u16,
            height: height as u16,
            bits_per_sample: 8,
            samples_per_pixel: channels as u8,
            photometric: if channels == 1 { 1 } else { 6 },
            subsampling: (2, 2),
            planar_config: 1,
        };
        let (info, ours) =
            decode_ojpeg(&tags, &geometry, &[], &DecodeOptions::default()).expect("decode_ojpeg");
        assert_eq!((info.width, info.height), (width as u16, height as u16));
        assert_eq!(
            ours.len(),
            theirs.len(),
            "libtiff read a different shape at {width}x{height} x{channels}"
        );

        // Two separate claims, because libtiff's colour pipeline is its own.
        //
        // 1. The pixels: `djpeg` decodes the very same interchange stream, and
        //    our reconstruction has to match it **exactly**.
        let reference = djpeg(&["-dct", "int", "-pnm"], &jpeg).expect("djpeg");
        let wide: Vec<u16> = ours.iter().map(|&v| u16::from(v)).collect();
        if let Some((index, a, b)) = first_difference(&wide, &reference.samples) {
            panic!(
                "flavour (a) {width}x{height} x{channels}: sample {index} differs                  (ours {a}, djpeg {b}); reference: {}",
                libjpeg_version()
            );
        }

        // 2. libtiff accepted the file and produced an image of the right
        //    shape. Its samples are only compared tightly for grayscale: for
        //    a YCbCr photometric libtiff applies its **own** conversion and
        //    its own chroma upsampling, which differ from libjpeg's by up to
        //    ~100 LSB on high-contrast synthetic material (measured here).
        //    That divergence belongs to the container, which is why this
        //    module never applies a colour transform at all.
        if channels == 1 {
            let mut peak = 0i32;
            for (a, b) in ours.iter().zip(theirs.iter()) {
                peak = peak.max((i32::from(*a) - i32::from(*b)).abs());
            }
            assert!(
                peak <= 2,
                "flavour (a) grayscale: peak difference vs libtiff is {peak}"
            );
        }
        compared += 1;
    }
    assert!(
        compared > 0,
        "libtiff read no OJPEG fixture, so the test proved nothing"
    );
}
