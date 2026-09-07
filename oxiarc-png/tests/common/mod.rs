//! Shared helpers for the integration tests: a hand-rolled PNG writer that can
//! emit combinations Pillow cannot, and small pseudo-random generators.
//!
//! Everything here builds files byte by byte through `oxiarc_png::chunk`, so
//! the tests never depend on the crate's own encoder.

#![allow(dead_code)]

use oxiarc_png::chunk::{self, ChunkType, SIGNATURE, write_chunk};
use oxiarc_png::{BitDepth, ColorType};

/// A deterministic 64-bit LCG, so fixtures are reproducible without a
/// dependency.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Rng {
        Rng(seed | 1)
    }

    pub fn next_u8(&mut self) -> u8 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (self.0 >> 33) as u8
    }

    pub fn bytes(&mut self, n: usize) -> Vec<u8> {
        (0..n).map(|_| self.next_u8()).collect()
    }
}

/// A PNG being assembled chunk by chunk.
pub struct PngBuilder {
    pub width: u32,
    pub height: u32,
    pub color_type: ColorType,
    pub bit_depth: BitDepth,
    pub interlace: bool,
    chunks: Vec<(ChunkType, Vec<u8>)>,
    idat_split: usize,
    idat_level: u8,
    raw_deflate: bool,
    omit_iend: bool,
    trailing: Vec<u8>,
}

impl PngBuilder {
    pub fn new(width: u32, height: u32, color_type: ColorType, bit_depth: BitDepth) -> PngBuilder {
        PngBuilder {
            width,
            height,
            color_type,
            bit_depth,
            interlace: false,
            chunks: Vec::new(),
            idat_split: usize::MAX,
            idat_level: 6,
            raw_deflate: false,
            omit_iend: false,
            trailing: Vec::new(),
        }
    }

    pub fn interlaced(mut self, yes: bool) -> Self {
        self.interlace = yes;
        self
    }

    /// Split the image data into `n`-byte `IDAT` chunks.
    pub fn idat_split(mut self, n: usize) -> Self {
        self.idat_split = n;
        self
    }

    pub fn deflate_level(mut self, level: u8) -> Self {
        self.idat_level = level;
        self
    }

    /// Emit raw DEFLATE image data, as Apple `CgBI` files do.
    pub fn raw_deflate(mut self, yes: bool) -> Self {
        self.raw_deflate = yes;
        self
    }

    pub fn omit_iend(mut self, yes: bool) -> Self {
        self.omit_iend = yes;
        self
    }

    pub fn trailing(mut self, bytes: &[u8]) -> Self {
        self.trailing = bytes.to_vec();
        self
    }

    /// Add a chunk before `IDAT`.
    pub fn chunk(mut self, kind: ChunkType, data: &[u8]) -> Self {
        self.chunks.push((kind, data.to_vec()));
        self
    }

    pub fn samples(&self) -> usize {
        self.color_type.samples()
    }

    pub fn row_stride(&self, width: u32) -> usize {
        let bits = width as usize * self.samples() * usize::from(self.bit_depth as u8);
        bits.div_ceil(8)
    }

    /// The exact number of raw bytes the image data must carry.
    pub fn expected_raw(&self) -> usize {
        if self.interlace {
            (0..7)
                .map(|pass| {
                    let (w, h) =
                        oxiarc_png::interlace::pass_dimensions(self.width, self.height, pass);
                    if w == 0 || h == 0 {
                        0
                    } else {
                        (1 + self.row_stride(w)) * h as usize
                    }
                })
                .sum()
        } else {
            (1 + self.row_stride(self.width)) * self.height as usize
        }
    }

    fn ihdr(&self) -> Vec<u8> {
        let mut d = vec![0u8; 13];
        d[0..4].copy_from_slice(&self.width.to_be_bytes());
        d[4..8].copy_from_slice(&self.height.to_be_bytes());
        d[8] = self.bit_depth as u8;
        d[9] = self.color_type as u8;
        d[12] = u8::from(self.interlace);
        d
    }

    /// Assemble the file with `raw` as the already-filtered image data.
    pub fn build_with_raw(&self, raw: &[u8]) -> Vec<u8> {
        let compressed = if self.raw_deflate {
            oxiarc_deflate::deflate(raw, self.idat_level).expect("deflate")
        } else {
            oxiarc_deflate::zlib_compress(raw, self.idat_level).expect("zlib")
        };
        let mut out = SIGNATURE.to_vec();
        write_chunk(&mut out, chunk::IHDR, &self.ihdr()).expect("ihdr");
        for (kind, data) in &self.chunks {
            write_chunk(&mut out, *kind, data).expect("chunk");
        }
        if self.idat_split == usize::MAX || compressed.is_empty() {
            write_chunk(&mut out, chunk::IDAT, &compressed).expect("idat");
        } else {
            for part in compressed.chunks(self.idat_split.max(1)) {
                write_chunk(&mut out, chunk::IDAT, part).expect("idat");
            }
        }
        if !self.omit_iend {
            write_chunk(&mut out, chunk::IEND, &[]).expect("iend");
        }
        out.extend_from_slice(&self.trailing);
        out
    }

    /// Assemble the file from unfiltered sample rows, using filter 0.
    ///
    /// `samples` is the packed, non-interlaced image; for interlaced files it
    /// is split into the seven passes here.
    pub fn build_from_samples(&self, samples: &[u8]) -> Vec<u8> {
        self.build_with_raw(&self.filter_samples(samples))
    }

    /// Turn a packed image into the raw image-data stream, filter byte 0 per
    /// row, splitting into Adam7 passes when interlaced.
    pub fn filter_samples(&self, samples: &[u8]) -> Vec<u8> {
        let bits = usize::from(self.bit_depth as u8) * self.samples();
        let stride = self.row_stride(self.width);
        let mut raw = Vec::new();
        if !self.interlace {
            for y in 0..self.height as usize {
                raw.push(0);
                raw.extend_from_slice(&samples[y * stride..(y + 1) * stride]);
            }
            return raw;
        }
        for pass in 0..7 {
            let (pw, ph) = oxiarc_png::interlace::pass_dimensions(self.width, self.height, pass);
            if pw == 0 || ph == 0 {
                continue;
            }
            let p = oxiarc_png::interlace::PASSES[pass];
            let pass_stride = self.row_stride(pw);
            for line in 0..ph {
                raw.push(0);
                let mut row = vec![0u8; pass_stride];
                for i in 0..pw as usize {
                    let x = p.x_offset as usize + i * p.x_step as usize;
                    let y = p.y_offset as usize + line as usize * p.y_step as usize;
                    copy_pixel(samples, stride, x, y, &mut row, i, bits);
                }
                raw.extend_from_slice(&row);
            }
        }
        raw
    }
}

/// Copy one pixel of `bits` bits from a packed image into a packed row.
fn copy_pixel(
    src: &[u8],
    src_stride: usize,
    x: usize,
    y: usize,
    dst: &mut [u8],
    dst_index: usize,
    bits: usize,
) {
    if bits >= 8 {
        let bytes = bits / 8;
        let s = y * src_stride + x * bytes;
        dst[dst_index * bytes..dst_index * bytes + bytes].copy_from_slice(&src[s..s + bytes]);
        return;
    }
    let src_bit = y * src_stride * 8 + x * bits;
    let value = (src[src_bit / 8] >> (8 - bits - src_bit % 8)) & ((1u16 << bits) as u8 - 1);
    let dst_bit = dst_index * bits;
    let shift = 8 - bits - dst_bit % 8;
    let mask = ((1u16 << bits) as u8 - 1) << shift;
    dst[dst_bit / 8] = (dst[dst_bit / 8] & !mask) | ((value << shift) & mask);
}

/// A 2x2 8-bit grayscale PNG with pixel values 1, 2, 3, 4.
pub fn tiny_gray_png() -> Vec<u8> {
    PngBuilder::new(2, 2, ColorType::Grayscale, BitDepth::Eight).build_from_samples(&[1, 2, 3, 4])
}

/// A reader that hands out at most `chunk` bytes per `read`, and optionally
/// injects `Interrupted` errors, so tests can prove the decoder is resumable
/// at any byte boundary.
pub struct ChunkedReader<'a> {
    data: &'a [u8],
    pos: usize,
    chunk: usize,
    interrupt_every: usize,
    calls: usize,
}

impl<'a> ChunkedReader<'a> {
    pub fn new(data: &'a [u8], chunk: usize) -> ChunkedReader<'a> {
        ChunkedReader {
            data,
            pos: 0,
            chunk: chunk.max(1),
            interrupt_every: 0,
            calls: 0,
        }
    }

    pub fn interrupting(mut self, every: usize) -> Self {
        self.interrupt_every = every;
        self
    }
}

impl std::io::Read for ChunkedReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.calls += 1;
        if self.interrupt_every > 0 && self.calls % self.interrupt_every == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "synthetic interrupt",
            ));
        }
        let n = self.chunk.min(buf.len()).min(self.data.len() - self.pos);
        buf[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

/// Build a small APNG by hand.
///
/// Frame 0 is the default image in `IDAT`; every later frame is an `fdAT`
/// chunk whose payload is its own independent zlib stream.
pub struct ApngBuilder {
    pub width: u32,
    pub height: u32,
    pub color_type: ColorType,
    pub bit_depth: BitDepth,
    frames: Vec<(FrameControl, Vec<u8>)>,
    num_plays: u32,
    default_is_frame_zero: bool,
    sequence_override: Option<Vec<u32>>,
}

use oxiarc_png::{BlendOp, DisposeOp, FrameControl};

impl ApngBuilder {
    pub fn new(width: u32, height: u32) -> ApngBuilder {
        ApngBuilder {
            width,
            height,
            color_type: ColorType::Rgba,
            bit_depth: BitDepth::Eight,
            frames: Vec::new(),
            num_plays: 0,
            default_is_frame_zero: true,
            sequence_override: None,
        }
    }

    pub fn default_is_frame_zero(mut self, yes: bool) -> Self {
        self.default_is_frame_zero = yes;
        self
    }

    /// Force the sequence numbers written to the file, to build broken files.
    pub fn sequence_override(mut self, sequence: Vec<u32>) -> Self {
        self.sequence_override = Some(sequence);
        self
    }

    /// Append a sub-frame of raw RGBA8 samples.
    pub fn frame(mut self, x: u32, y: u32, w: u32, h: u32, samples: &[u8]) -> Self {
        let control = FrameControl {
            sequence_number: 0,
            width: w,
            height: h,
            x_offset: x,
            y_offset: y,
            delay_num: 1,
            delay_den: 10,
            dispose_op: DisposeOp::None,
            blend_op: BlendOp::Source,
        };
        self.frames.push((control, samples.to_vec()));
        self
    }

    fn filter(&self, w: u32, h: u32, samples: &[u8]) -> Vec<u8> {
        let stride = w as usize * self.color_type.samples();
        let mut raw = Vec::new();
        for y in 0..h as usize {
            raw.push(0u8);
            raw.extend_from_slice(&samples[y * stride..(y + 1) * stride]);
        }
        raw
    }

    pub fn build(&self) -> Vec<u8> {
        let mut out = SIGNATURE.to_vec();
        let mut ihdr = vec![0u8; 13];
        ihdr[0..4].copy_from_slice(&self.width.to_be_bytes());
        ihdr[4..8].copy_from_slice(&self.height.to_be_bytes());
        ihdr[8] = self.bit_depth as u8;
        ihdr[9] = self.color_type as u8;
        write_chunk(&mut out, chunk::IHDR, &ihdr).expect("ihdr");

        let mut actl = Vec::new();
        actl.extend_from_slice(&(self.frames.len() as u32).to_be_bytes());
        actl.extend_from_slice(&self.num_plays.to_be_bytes());
        write_chunk(&mut out, chunk::acTL, &actl).expect("actl");

        let mut seq = 0u32;
        let mut next_seq = |index: usize| -> u32 {
            let value = match &self.sequence_override {
                Some(list) => list.get(index).copied().unwrap_or(seq),
                None => seq,
            };
            seq += 1;
            value
        };
        let mut written = 0usize;

        for (index, (control, samples)) in self.frames.iter().enumerate() {
            let raw = self.filter(control.width, control.height, samples);
            let compressed = oxiarc_deflate::zlib_compress(&raw, 6).expect("zlib");
            if index == 0 {
                if self.default_is_frame_zero {
                    let mut fctl = *control;
                    fctl.sequence_number = next_seq(written);
                    written += 1;
                    write_chunk(&mut out, chunk::fcTL, &encode_fctl(&fctl)).expect("fctl");
                }
                write_chunk(&mut out, chunk::IDAT, &compressed).expect("idat");
            } else {
                let mut fctl = *control;
                fctl.sequence_number = next_seq(written);
                written += 1;
                write_chunk(&mut out, chunk::fcTL, &encode_fctl(&fctl)).expect("fctl");
                let mut payload = next_seq(written).to_be_bytes().to_vec();
                written += 1;
                payload.extend_from_slice(&compressed);
                write_chunk(&mut out, chunk::fdAT, &payload).expect("fdat");
            }
        }
        write_chunk(&mut out, chunk::IEND, &[]).expect("iend");
        out
    }
}

/// Serialise an `fcTL` payload.
pub fn encode_fctl(f: &FrameControl) -> Vec<u8> {
    let mut d = Vec::with_capacity(26);
    d.extend_from_slice(&f.sequence_number.to_be_bytes());
    d.extend_from_slice(&f.width.to_be_bytes());
    d.extend_from_slice(&f.height.to_be_bytes());
    d.extend_from_slice(&f.x_offset.to_be_bytes());
    d.extend_from_slice(&f.y_offset.to_be_bytes());
    d.extend_from_slice(&f.delay_num.to_be_bytes());
    d.extend_from_slice(&f.delay_den.to_be_bytes());
    d.push(f.dispose_op as u8);
    d.push(f.blend_op as u8);
    d
}
