//! Temporary probe: whole-image decode wall clock against `tiffcp`.
#![cfg(all(feature = "all-codecs", feature = "mmap"))]

use oxiarc_tiff::{ColorType, Decoder, Encoder, ImageSpec, Layout};
use std::fs;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

const SIDE: u32 = 4096;

fn dir() -> PathBuf {
    let path = std::env::temp_dir().join("oxiarc_tiff_perf");
    fs::create_dir_all(&path).expect("scratch");
    path
}

/// Photograph-like: a smooth base plus enough noise that LZW cannot win.
fn noisy(len: usize, step: usize) -> Vec<u8> {
    let mut out = vec![0u8; len];
    let mut state = 0x2545_F491_4F6C_DD1Du64;
    for (index, slot) in out.iter_mut().enumerate() {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        let base = ((index / step) % 256) as u32;
        let noise = (state >> 33) as u32 % 200;
        *slot = ((base * 2 + noise) / 3) as u8;
    }
    out
}

fn bilevel(width: u32, height: u32) -> Vec<u8> {
    // One byte per pixel: the writer packs sub-byte depths itself.
    let mut out = vec![0u8; width as usize * height as usize];
    for y in 0..height as usize {
        for x in 0..width as usize {
            if (x / (3 + y % 11)) % 2 == 0 || (x + y) % 97 < 5 {
                out[y * width as usize + x] = 1;
            }
        }
    }
    out
}

fn write_source(path: &Path, spec: ImageSpec, data: &[u8]) {
    if path.exists() {
        return;
    }
    let file = fs::File::create(path).expect("create");
    let mut encoder = Encoder::new(std::io::BufWriter::new(file)).expect("encoder");
    encoder.write_image(&spec, data).expect("write");
    encoder.finish().expect("finish");
}

fn tiffcp(args: &[&str], input: &Path, output: &Path) -> bool {
    Command::new("tiffcp")
        .args(args)
        .arg(input)
        .arg(output)
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

fn time_tiffcp(input: &Path, output: &Path) -> Duration {
    let start = Instant::now();
    let ok = tiffcp(&["-c", "none"], input, output);
    let elapsed = start.elapsed();
    assert!(ok, "tiffcp -c none failed on {}", input.display());
    elapsed
}

fn time_ours(input: &Path, len: usize) -> Duration {
    let start = Instant::now();
    let file = fs::File::open(input).expect("open");
    let mut decoder = Decoder::new(BufReader::new(file)).expect("decoder");
    let mut buffer = vec![0u8; len];
    decoder.read_image_bytes(&mut buffer).expect("decode");
    let elapsed = start.elapsed();
    std::hint::black_box(&buffer);
    elapsed
}

fn median(mut values: Vec<Duration>) -> Duration {
    values.sort_unstable();
    values[values.len() / 2]
}

fn load() -> String {
    Command::new("uptime")
        .output()
        .ok()
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .map(|line| line.trim().to_string())
        .unwrap_or_default()
}

#[test]
fn probe() {
    let rounds: usize = std::env::var("PERF_ROUNDS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(9);
    let dir = dir();
    let rgb = dir.join("rgb8.tif");
    let gray = dir.join("gray16.tif");
    let bil = dir.join("bilevel.tif");
    let rgb_len = (SIDE as usize) * (SIDE as usize) * 3;
    let gray_len = (SIDE as usize) * (SIDE as usize) * 2;
    let bil_len = (SIDE as usize) * (SIDE as usize);
    write_source(
        &rgb,
        ImageSpec::new(SIDE, SIDE, ColorType::Rgb(8))
            .with_layout(Layout::Strips { rows_per_strip: 16 }),
        &noisy(rgb_len, 3 * 512),
    );
    write_source(
        &gray,
        ImageSpec::new(SIDE, SIDE, ColorType::Gray(16))
            .with_layout(Layout::Strips { rows_per_strip: 16 }),
        &noisy(gray_len, 2 * 512),
    );
    write_source(
        &bil,
        ImageSpec::new(SIDE, SIDE, ColorType::Gray(1))
            .with_photometric(oxiarc_tiff::PhotometricInterpretation::WhiteIsZero)
            .with_layout(Layout::Strips { rows_per_strip: 64 }),
        &bilevel(SIDE, SIDE),
    );
    println!("load at start: {}", load());

    let fixtures: [(&str, &Path, usize, &[&str]); 3] = [
        (
            "rgb8",
            &rgb,
            rgb_len,
            &["none", "packbits", "lzw", "zip", "zstd", "lzma", "jpeg"],
        ),
        (
            "gray16",
            &gray,
            gray_len,
            &["none", "packbits", "lzw", "zip", "zstd", "lzma"],
        ),
        ("bilevel", &bil, bil_len, &["none", "g3", "g3:2d", "g4"]),
    ];
    for (fixture, source, len, codecs) in fixtures {
        println!("\n== {fixture} ({len} bytes native) ==");
        println!("| codec | file MB | tiffcp -c none | ours | ratio |");
        println!("|---|---|---|---|---|");
        for codec in codecs {
            let coded = dir.join(format!("{fixture}_{}.tif", codec.replace(':', "_")));
            let mut args = vec!["-c", codec];
            if codec.starts_with("jpeg") {
                args.extend_from_slice(&["-r", "16"]);
            }
            if !coded.exists() && !tiffcp(&args, source, &coded) {
                println!("| {codec} | (tiffcp refused) | | | |");
                continue;
            }
            let size = fs::metadata(&coded).map(|m| m.len()).unwrap_or(0);
            let scratch = dir.join("scratch.tif");
            let mut theirs = Vec::new();
            let mut ours = Vec::new();
            for round in 0..rounds {
                // Interleaved, alternating which arm goes first.
                if round % 2 == 0 {
                    theirs.push(time_tiffcp(&coded, &scratch));
                    ours.push(time_ours(&coded, len));
                } else {
                    ours.push(time_ours(&coded, len));
                    theirs.push(time_tiffcp(&coded, &scratch));
                }
            }
            let t = median(theirs);
            let o = median(ours);
            println!(
                "| {codec} | {:.1} | {:.1} ms | {:.1} ms | {:.2}x |",
                size as f64 / 1e6,
                t.as_secs_f64() * 1e3,
                o.as_secs_f64() * 1e3,
                o.as_secs_f64() / t.as_secs_f64()
            );
        }
    }
    println!("\nload at end: {}", load());
}

/// Codec-only throughput: one strip, decoded many times, no pipeline.
#[test]
fn strip_throughput() {
    use oxiarc_tiff::compression::{CodecContext, CodecState, decode_into};
    let rounds: usize = std::env::var("PERF_STRIPS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(60);
    let dir = dir();
    println!("load at start: {}", load());
    println!("| fixture | codec | strip in | strip out | ms/strip | MB/s |");
    println!("|---|---|---|---|---|---|");
    for (fixture, bits, spp) in [("rgb8", vec![8u16; 3], 3u16), ("gray16", vec![16u16], 1u16)] {
        for codec in ["lzw", "zip", "zstd", "lzma", "packbits"] {
            let path = dir.join(format!("{fixture}_{codec}.tif"));
            if !path.exists() {
                continue;
            }
            let bytes = fs::read(&path).expect("read");
            let mut decoder = Decoder::new(std::io::Cursor::new(bytes)).expect("decoder");
            let method = decoder.info().expect("info").compression;
            let (cw, ch) = decoder.chunk_dimensions().expect("chunk");
            let raw = decoder.read_chunk_raw(0).expect("raw");
            let out_len = cw as usize * ch as usize * usize::from(spp) * (bits[0] as usize / 8);
            let state = CodecState::new();
            let mut cx = CodecContext::new(
                method,
                cw as usize,
                ch as usize,
                &bits,
                spp,
                oxiarc_tiff::Endian::Little,
            );
            cx.state = Some(&state);
            let mut out = vec![0u8; out_len];
            // Warm-up, and a check that the decode is real.
            let written = decode_into(&raw, &mut out, &cx).expect("decode");
            assert_eq!(written, out_len, "{fixture} {codec}");
            let mut samples = Vec::new();
            for _ in 0..rounds {
                let start = Instant::now();
                decode_into(&raw, &mut out, &cx).expect("decode");
                samples.push(start.elapsed());
            }
            let m = median(samples);
            println!(
                "| {fixture} | {codec} | {} | {out_len} | {:.3} | {:.0} |",
                raw.len(),
                m.as_secs_f64() * 1e3,
                out_len as f64 / m.as_secs_f64() / 1e6
            );
        }
    }
    println!("load at end: {}", load());
}

/// Whole-image decode, every sample printed, to separate noise from a real gap.
#[test]
fn whole_image_samples() {
    let dir = dir();
    println!("load: {}", load());
    for (fixture, len) in [
        ("rgb8", (SIDE as usize) * (SIDE as usize) * 3),
        ("gray16", (SIDE as usize) * (SIDE as usize) * 2),
    ] {
        for codec in ["none", "lzw", "zip", "zstd"] {
            let path = dir.join(format!("{fixture}_{codec}.tif"));
            if !path.exists() {
                continue;
            }
            let mut samples = Vec::new();
            for _ in 0..7 {
                samples.push(time_ours(&path, len));
            }
            let all: Vec<String> = samples
                .iter()
                .map(|d| format!("{:.0}", d.as_secs_f64() * 1e3))
                .collect();
            println!(
                "{fixture} {codec}: median {:.1} ms; all {} ms",
                median(samples).as_secs_f64() * 1e3,
                all.join(" ")
            );
        }
    }
    println!("load: {}", load());
}
