//! Shared helpers for the libjpeg-turbo differential suite.
//!
//! Everything here shells out to external binaries; nothing in this file adds
//! a dependency to the crate. Each helper returns `None` when the tool is
//! missing so the tests can self-skip on a hermetic machine.

use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

/// Unique scratch path under the platform temp directory.
pub fn temp_path(label: &str, extension: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut path = std::env::temp_dir();
    path.push(format!(
        "oxiarc_jpeg_{label}_{}_{n}.{extension}",
        std::process::id()
    ));
    path
}

/// `true` when `name` runs and reports a version.
pub fn tool_available(name: &str) -> bool {
    Command::new(name)
        .arg("-version")
        .output()
        .map(|out| out.status.success() || !out.stderr.is_empty())
        .unwrap_or(false)
}

/// The `cjpeg -version` banner, quoted in failure messages so a byte-parity
/// regression after a toolchain upgrade is diagnosable in one look.
pub fn libjpeg_version() -> String {
    Command::new("cjpeg")
        .arg("-version")
        .output()
        .ok()
        .map(|out| {
            let text = if out.stderr.is_empty() {
                out.stdout
            } else {
                out.stderr
            };
            String::from_utf8_lossy(&text).trim().to_string()
        })
        .unwrap_or_else(|| "unknown".to_string())
}

/// A binary PNM image.
pub struct Pnm {
    pub width: usize,
    pub height: usize,
    pub maxval: u32,
    pub channels: usize,
    /// Samples in raster order, one entry per sample.
    pub samples: Vec<u16>,
}

impl Pnm {
    /// Serialise as a binary PGM or PPM.
    pub fn to_bytes(&self) -> Vec<u8> {
        let magic = if self.channels == 1 { "P5" } else { "P6" };
        let mut out =
            format!("{magic}\n{} {}\n{}\n", self.width, self.height, self.maxval).into_bytes();
        for &sample in &self.samples {
            if self.maxval > 255 {
                out.extend_from_slice(&sample.to_be_bytes());
            } else {
                out.push(sample as u8);
            }
        }
        out
    }

    /// Parse a binary PGM or PPM.
    pub fn parse(data: &[u8]) -> Option<Pnm> {
        let mut pos = 0usize;
        let mut fields: Vec<u32> = Vec::new();
        let magic = data.get(..2)?;
        let channels = match magic {
            b"P5" => 1,
            b"P6" => 3,
            _ => return None,
        };
        pos += 2;
        while fields.len() < 3 {
            while pos < data.len() && (data[pos] as char).is_whitespace() {
                pos += 1;
            }
            if data.get(pos) == Some(&b'#') {
                while pos < data.len() && data[pos] != b'\n' {
                    pos += 1;
                }
                continue;
            }
            let start = pos;
            while pos < data.len() && data[pos].is_ascii_digit() {
                pos += 1;
            }
            if start == pos {
                return None;
            }
            fields.push(std::str::from_utf8(&data[start..pos]).ok()?.parse().ok()?);
        }
        pos += 1; // exactly one whitespace byte after maxval
        let width = fields[0] as usize;
        let height = fields[1] as usize;
        let maxval = fields[2];
        let count = width * height * channels;
        let mut samples = Vec::with_capacity(count);
        if maxval > 255 {
            for chunk in data[pos..].chunks_exact(2).take(count) {
                samples.push(u16::from_be_bytes([chunk[0], chunk[1]]));
            }
        } else {
            for &byte in data[pos..].iter().take(count) {
                samples.push(u16::from(byte));
            }
        }
        if samples.len() != count {
            return None;
        }
        Some(Pnm {
            width,
            height,
            maxval,
            channels,
            samples,
        })
    }
}

/// A synthetic source image with flat regions, hard edges and a gradient.
pub fn synthetic(width: usize, height: usize, channels: usize, maxval: u32) -> Pnm {
    let mut samples = Vec::with_capacity(width * height * channels);
    for y in 0..height {
        for x in 0..width {
            let base = (x * 7 + y * 13) as u32 % (maxval + 1);
            let edge = if (x / 5 + y / 3) % 2 == 0 {
                maxval
            } else {
                maxval / 8
            };
            let flat = maxval / 2;
            let values = [base, edge, flat];
            for c in 0..channels {
                samples.push(values[c % 3] as u16);
            }
        }
    }
    Pnm {
        width,
        height,
        maxval,
        channels,
        samples,
    }
}

/// Run `cjpeg` with `args` over `source`, returning the JPEG bytes.
pub fn cjpeg(args: &[&str], source: &Pnm) -> Option<Vec<u8>> {
    let input = temp_path("cjpeg_in", if source.channels == 1 { "pgm" } else { "ppm" });
    let output = temp_path("cjpeg_out", "jpg");
    std::fs::write(&input, source.to_bytes()).ok()?;
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

/// Run `djpeg` with `args` over `jpeg`, returning the decoded PNM.
pub fn djpeg(args: &[&str], jpeg: &[u8]) -> Option<Pnm> {
    djpeg_env(args, jpeg, &[])
}

/// Run `djpeg` with extra environment variables (used to force the portable
/// C kernels with `JSIMD_FORCENONE=1`).
pub fn djpeg_env(args: &[&str], jpeg: &[u8], env: &[(&str, &str)]) -> Option<Pnm> {
    let input = temp_path("djpeg_in", "jpg");
    let output = temp_path("djpeg_out", "pnm");
    std::fs::write(&input, jpeg).ok()?;
    let mut command = Command::new("djpeg");
    command.args(args).arg("-outfile").arg(&output).arg(&input);
    for (key, value) in env {
        command.env(key, value);
    }
    let status = command.status().ok()?;
    let parsed = if status.success() {
        std::fs::read(&output)
            .ok()
            .and_then(|data| Pnm::parse(&data))
    } else {
        None
    };
    let _ = std::fs::remove_file(&input);
    let _ = std::fs::remove_file(&output);
    parsed
}

/// Report the first differing sample, for a readable assertion message.
pub fn first_difference(ours: &[u16], theirs: &[u16]) -> Option<(usize, u16, u16)> {
    if ours.len() != theirs.len() {
        return Some((
            ours.len().min(theirs.len()),
            ours.len() as u16,
            theirs.len() as u16,
        ));
    }
    ours.iter()
        .zip(theirs.iter())
        .enumerate()
        .find(|(_, (a, b))| a != b)
        .map(|(i, (a, b))| (i, *a, *b))
}
