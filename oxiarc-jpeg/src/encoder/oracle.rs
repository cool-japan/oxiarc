//! A coefficient-level differential test against `cjpeg`.
//!
//! Six independent surfaces stand between a source image and a JPEG's bytes:
//! forward colour conversion, MCU edge replication, chroma decimation, dummy
//! block generation, the forward DCT and the quantiser's rounding. An
//! end-to-end byte comparison reports all six as one offset in one file, and
//! bisecting that by hand is miserable.
//!
//! So this module entropy-*decodes* `cjpeg`'s own baseline output back into
//! quantised coefficients and compares them with ours block by block. When it
//! passes, every failure in `tests/encode_oracle.rs` is in the entropy coder
//! or the marker layer, and when it fails the failing block index names the
//! component, the block column and the block row.
//!
//! Only compiled for tests, and only with the `jpeg-oracle` feature.

use super::coefficients::{CoefficientPlane, build_coefficients};
use super::options::{EncodeOptions, InputColor};
use super::plan::build_plan;
use super::prepare::{Samples, build_dct_planes};
use crate::huffman::{BitReader, HuffmanTable};

/// The parts of a baseline datastream this module needs.
struct Baseline {
    width: usize,
    height: usize,
    components: Vec<(u8, u8, u8, u8)>,
    dc: Vec<Option<HuffmanTable>>,
    ac: Vec<Option<HuffmanTable>>,
    scan_dc: Vec<usize>,
    scan_ac: Vec<usize>,
    entropy: Vec<u8>,
    restart_interval: usize,
}

/// Parse just enough of a baseline JPEG to entropy-decode its single scan.
fn parse_baseline(data: &[u8]) -> Option<Baseline> {
    let mut pos = 2usize; // skip SOI
    let mut out = Baseline {
        width: 0,
        height: 0,
        components: Vec::new(),
        dc: vec![None, None, None, None],
        ac: vec![None, None, None, None],
        scan_dc: Vec::new(),
        scan_ac: Vec::new(),
        entropy: Vec::new(),
        restart_interval: 0,
    };
    while pos + 4 <= data.len() {
        if data[pos] != 0xFF {
            return None;
        }
        let marker = data[pos + 1];
        let length = usize::from(u16::from_be_bytes([data[pos + 2], data[pos + 3]]));
        let payload = data.get(pos + 4..pos + 2 + length)?;
        match marker {
            0xC0 | 0xC1 => {
                out.height = usize::from(u16::from_be_bytes([payload[1], payload[2]]));
                out.width = usize::from(u16::from_be_bytes([payload[3], payload[4]]));
                let count = usize::from(payload[5]);
                for index in 0..count {
                    let base = 6 + 3 * index;
                    out.components.push((
                        payload[base],
                        payload[base + 1] >> 4,
                        payload[base + 1] & 0x0F,
                        payload[base + 2],
                    ));
                }
            }
            0xC4 => {
                let mut cursor = 0usize;
                while cursor < payload.len() {
                    let class = payload[cursor] >> 4;
                    let slot = usize::from(payload[cursor] & 0x0F);
                    let mut bits = [0u8; 16];
                    bits.copy_from_slice(&payload[cursor + 1..cursor + 17]);
                    let total: usize = bits.iter().map(|&b| usize::from(b)).sum();
                    let values = payload[cursor + 17..cursor + 17 + total].to_vec();
                    let table = HuffmanTable::new(bits, values).ok()?;
                    if class == 0 {
                        out.dc[slot] = Some(table);
                    } else {
                        out.ac[slot] = Some(table);
                    }
                    cursor += 17 + total;
                }
            }
            0xDD => {
                out.restart_interval = usize::from(u16::from_be_bytes([payload[0], payload[1]]));
            }
            0xDA => {
                let count = usize::from(payload[0]);
                for index in 0..count {
                    let tables = payload[2 + 2 * index];
                    out.scan_dc.push(usize::from(tables >> 4));
                    out.scan_ac.push(usize::from(tables & 0x0F));
                }
                out.entropy = data[pos + 2 + length..].to_vec();
                return Some(out);
            }
            _ => {}
        }
        pos += 2 + length;
    }
    None
}

/// Entropy-decode a baseline interleaved scan into one coefficient plane per
/// component, using the same padded block geometry the encoder produces.
fn decode_coefficients(frame: &Baseline) -> Option<Vec<CoefficientPlane>> {
    let hmax = frame.components.iter().map(|c| c.1).max()? as usize;
    let vmax = frame.components.iter().map(|c| c.2).max()? as usize;
    let mcus_per_line = frame.width.div_ceil(8 * hmax);
    let mcus_per_column = frame.height.div_ceil(8 * vmax);

    let mut planes: Vec<CoefficientPlane> = frame
        .components
        .iter()
        .map(|&(_, h, v, _)| {
            let blocks_wide = mcus_per_line * usize::from(h);
            let blocks_high = mcus_per_column * usize::from(v);
            CoefficientPlane {
                data: vec![0i16; blocks_wide * blocks_high * 64],
                blocks_wide,
                blocks_high,
            }
        })
        .collect();

    let mut reader = BitReader::new(&frame.entropy);
    let mut predictions = vec![0i32; frame.components.len()];
    let mut since_restart = 0usize;

    for mcu in 0..mcus_per_line * mcus_per_column {
        if frame.restart_interval > 0 && since_restart == frame.restart_interval {
            reader.seek_marker()?;
            reader.consume_marker();
            predictions.iter_mut().for_each(|p| *p = 0);
            since_restart = 0;
        }
        since_restart += 1;
        let mcu_x = mcu % mcus_per_line;
        let mcu_y = mcu / mcus_per_line;
        for (index, &(_, h, v, _)) in frame.components.iter().enumerate() {
            let dc_table = frame.dc[frame.scan_dc[index]].as_ref()?;
            let ac_table = frame.ac[frame.scan_ac[index]].as_ref()?;
            for by in 0..usize::from(v) {
                for bx in 0..usize::from(h) {
                    let block_x = mcu_x * usize::from(h) + bx;
                    let block_y = mcu_y * usize::from(v) + by;
                    let stride = planes[index].blocks_wide;
                    let start = (block_y * stride + block_x) * 64;

                    let size = u32::from(reader.decode(dc_table, mcu as u64).ok()?);
                    let diff = reader.receive_extend(size);
                    predictions[index] += diff;
                    planes[index].data[start] = predictions[index] as i16;

                    let mut k = 1usize;
                    while k < 64 {
                        let symbol = reader.decode(ac_table, mcu as u64).ok()?;
                        let run = usize::from(symbol >> 4);
                        let size = u32::from(symbol & 0x0F);
                        if size == 0 {
                            if run != 15 {
                                break;
                            }
                            k += 16;
                            continue;
                        }
                        k += run;
                        if k >= 64 {
                            return None;
                        }
                        let value = reader.receive_extend(size);
                        planes[index].data[start + k] = value as i16;
                        k += 1;
                    }
                }
            }
        }
    }
    Some(planes)
}

/// Compare our coefficients against `cjpeg`'s for one image and option set.
///
/// Returns the number of blocks compared, or a description of the first
/// mismatch.
pub(crate) fn compare_with_cjpeg(
    jpeg: &[u8],
    pixels: &[u8],
    width: u16,
    height: u16,
    input: InputColor,
    options: &EncodeOptions,
) -> Result<usize, String> {
    let frame = parse_baseline(jpeg).ok_or("could not parse the reference stream")?;
    let theirs =
        decode_coefficients(&frame).ok_or("could not entropy-decode the reference scan")?;

    let plan = build_plan(options, width, height, input).map_err(|e| e.to_string())?;
    let planes = build_dct_planes(&plan, &Samples::Eight(pixels)).map_err(|e| e.to_string())?;
    let ours = build_coefficients(&plan, &planes);

    if ours.len() != theirs.len() {
        return Err(format!(
            "component count: ours {} theirs {}",
            ours.len(),
            theirs.len()
        ));
    }
    let mut compared = 0usize;
    for (index, (mine, reference)) in ours.iter().zip(theirs.iter()).enumerate() {
        if (mine.blocks_wide, mine.blocks_high) != (reference.blocks_wide, reference.blocks_high) {
            return Err(format!(
                "component {index} geometry: ours {}x{} theirs {}x{}",
                mine.blocks_wide, mine.blocks_high, reference.blocks_wide, reference.blocks_high
            ));
        }
        for by in 0..mine.blocks_high {
            for bx in 0..mine.blocks_wide {
                let a = mine.block(bx, by);
                let b = reference.block(bx, by);
                if a != b {
                    let k = a
                        .iter()
                        .zip(b.iter())
                        .position(|(x, y)| x != y)
                        .unwrap_or(0);
                    return Err(format!(
                        "component {index} block ({bx},{by}) coefficient {k}: \
                         ours {} theirs {}",
                        a[k], b[k]
                    ));
                }
                compared += 1;
            }
        }
    }
    Ok(compared)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ColorSpace;
    use crate::encoder::options::Subsampling;
    use std::process::Command;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn temp_path(label: &str, extension: &str) -> std::path::PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let mut path = std::env::temp_dir();
        path.push(format!(
            "oxiarc_jpeg_enc_{label}_{}_{n}.{extension}",
            std::process::id()
        ));
        path
    }

    fn cjpeg_available() -> bool {
        Command::new("cjpeg")
            .arg("-version")
            .output()
            .map(|out| out.status.success() || !out.stderr.is_empty())
            .unwrap_or(false)
    }

    /// A source image with flat areas, hard edges and a gradient — the three
    /// things that exercise edge replication, decimation and the DCT.
    fn source(width: usize, height: usize, channels: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(width * height * channels);
        for y in 0..height {
            for x in 0..width {
                let values = [
                    ((x * 7 + y * 13) % 256) as u8,
                    if (x / 5 + y / 3) % 2 == 0 { 255 } else { 32 },
                    128,
                ];
                for c in 0..channels {
                    out.push(values[c % 3]);
                }
            }
        }
        out
    }

    fn to_pnm(pixels: &[u8], width: usize, height: usize, channels: usize) -> Vec<u8> {
        let magic = if channels == 1 { "P5" } else { "P6" };
        let mut out = format!("{magic}\n{width} {height}\n255\n").into_bytes();
        out.extend_from_slice(pixels);
        out
    }

    fn run_cjpeg(args: &[&str], pnm: &[u8], channels: usize) -> Option<Vec<u8>> {
        let input = temp_path("in", if channels == 1 { "pgm" } else { "ppm" });
        let output = temp_path("out", "jpg");
        std::fs::write(&input, pnm).ok()?;
        let status = Command::new("cjpeg")
            .args(args)
            .arg("-outfile")
            .arg(&output)
            .arg(&input)
            .status()
            .ok()?;
        let bytes = if status.success() {
            std::fs::read(&output).ok()
        } else {
            None
        };
        let _ = std::fs::remove_file(&input);
        let _ = std::fs::remove_file(&output);
        bytes
    }

    struct Case {
        label: &'static str,
        args: Vec<String>,
        options: EncodeOptions,
        input: InputColor,
        channels: usize,
    }

    fn cases() -> Vec<Case> {
        let mut cases = Vec::new();
        for &(name, sample, subsampling) in &[
            ("444", "1x1", Subsampling::S444),
            ("422", "2x1", Subsampling::S422),
            ("440", "1x2", Subsampling::S440),
            ("420", "2x2", Subsampling::S420),
            ("411", "4x1", Subsampling::S411),
            (
                "2x4",
                "2x4",
                Subsampling::Custom([(2, 4), (1, 1), (1, 1), (1, 1)]),
            ),
            (
                "4x2",
                "4x2",
                Subsampling::Custom([(4, 2), (1, 1), (1, 1), (1, 1)]),
            ),
            (
                "1x4",
                "1x4",
                Subsampling::Custom([(1, 4), (1, 1), (1, 1), (1, 1)]),
            ),
            (
                "3x1",
                "3x1",
                Subsampling::Custom([(3, 1), (1, 1), (1, 1), (1, 1)]),
            ),
        ] {
            for &quality in &[5u8, 10, 50, 75, 90, 100] {
                cases.push(Case {
                    label: name,
                    args: vec![
                        "-quality".into(),
                        quality.to_string(),
                        "-sample".into(),
                        format!("{sample},1x1,1x1"),
                        "-dct".into(),
                        "int".into(),
                    ],
                    options: EncodeOptions {
                        quality,
                        subsampling,
                        ..Default::default()
                    },
                    input: InputColor::Rgb,
                    channels: 3,
                });
            }
        }
        for &factor in &[10u8, 50, 100] {
            for &(name, sample, subsampling) in &[
                ("smooth-444", "1x1", Subsampling::S444),
                ("smooth-420", "2x2", Subsampling::S420),
                ("smooth-422", "2x1", Subsampling::S422),
            ] {
                cases.push(Case {
                    label: name,
                    args: vec![
                        "-quality".into(),
                        "75".into(),
                        "-smooth".into(),
                        factor.to_string(),
                        "-sample".into(),
                        format!("{sample},1x1,1x1"),
                    ],
                    options: EncodeOptions {
                        quality: 75,
                        subsampling,
                        downsampling: crate::Downsampling::Smooth(factor),
                        ..Default::default()
                    },
                    input: InputColor::Rgb,
                    channels: 3,
                });
            }
        }
        cases.push(Case {
            label: "grayscale",
            args: vec!["-quality".into(), "75".into()],
            options: EncodeOptions::default(),
            input: InputColor::Luma,
            channels: 1,
        });
        cases.push(Case {
            label: "rgb-no-transform",
            args: vec!["-quality".into(), "75".into(), "-rgb".into()],
            options: EncodeOptions {
                jpeg_color_space: Some(ColorSpace::Rgb),
                ..Default::default()
            },
            input: InputColor::Rgb,
            channels: 3,
        });
        cases.push(Case {
            label: "rgb-to-grey",
            args: vec!["-quality".into(), "75".into(), "-grayscale".into()],
            options: EncodeOptions {
                jpeg_color_space: Some(ColorSpace::Luma),
                ..Default::default()
            },
            input: InputColor::Rgb,
            channels: 3,
        });
        cases
    }

    /// The heart of milestone one: our quantised coefficients must equal the
    /// ones `cjpeg` wrote, for every subsampling ratio, several qualities and
    /// image shapes that are not multiples of an MCU.
    #[test]
    fn quantised_coefficients_match_cjpeg() {
        if !cjpeg_available() {
            eprintln!("cjpeg not on PATH; skipping the coefficient oracle");
            return;
        }
        let sizes = [
            (17usize, 19usize),
            (1, 1),
            (1, 33),
            (33, 1),
            (64, 64),
            (131, 97),
        ];
        let mut comparisons = 0usize;
        for case in cases() {
            for &(width, height) in &sizes {
                let pixels = source(width, height, case.channels);
                let pnm = to_pnm(&pixels, width, height, case.channels);
                let args: Vec<&str> = case.args.iter().map(String::as_str).collect();
                let Some(jpeg) = run_cjpeg(&args, &pnm, case.channels) else {
                    panic!("cjpeg failed for {} at {width}x{height}", case.label);
                };
                match compare_with_cjpeg(
                    &jpeg,
                    &pixels,
                    width as u16,
                    height as u16,
                    case.input,
                    &case.options,
                ) {
                    Ok(blocks) => comparisons += blocks,
                    Err(reason) => panic!(
                        "{} at {width}x{height} (args {:?}): {reason}",
                        case.label, case.args
                    ),
                }
            }
        }
        assert!(
            comparisons > 5_000,
            "only {comparisons} blocks compared; the oracle is not doing its job"
        );
    }
}
