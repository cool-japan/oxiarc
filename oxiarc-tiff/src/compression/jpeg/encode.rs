//! A baseline JPEG encoder for TIFF `Compression = 7`.
//!
//! # Why this lives here
//!
//! `oxiarc-jpeg` ships a decoder; its encoder is a separate work item that has
//! not landed. TIFF cannot wait for it — a codec that decodes but cannot write
//! is a half-codec — so this module encodes the one profile TIFF needs:
//! **baseline sequential, 8-bit, Huffman, no colour transform**, which is
//! exactly what libtiff's JPEG codec writes. Everything that *can* be shared
//! with `oxiarc-jpeg` is shared: the Annex K quantisation and Huffman tables,
//! [`QuantTable::scaled_for_quality`] (libjpeg's `jpeg_quality_scaling`), and
//! [`TableSet::emit`], which produces the `JPEGTables` blob in libtiff's byte
//! layout. When the `oxiarc-jpeg` encoder lands, this module's
//! [`encode_chunk`] is the only thing that has to change.
//!
//! # What it writes
//!
//! Measured against `tiffcp -c jpeg` (libtiff 4.7.1): tag 347 holds
//! `SOI DQT… DHT… EOI`, and every strip holds `SOI SOF0 SOS <entropy> EOI`
//! with no `JFIF` or `Adobe` marker — TTN2 keeps colour out of the JPEG
//! stream and in `PhotometricInterpretation`. Component identifiers follow
//! libjpeg: 1/2/3 for YCbCr and greyscale, `R`/`G`/`B` for an RGB frame.

use oxiarc_jpeg::tables::{
    ANNEX_K_AC_CHROMA_BITS, ANNEX_K_AC_CHROMA_VALUES, ANNEX_K_AC_LUMA_BITS, ANNEX_K_AC_LUMA_VALUES,
    ANNEX_K_DC_CHROMA_BITS, ANNEX_K_DC_CHROMA_VALUES, ANNEX_K_DC_LUMA_BITS, ANNEX_K_DC_LUMA_VALUES,
    ZIGZAG_TO_NATURAL,
};
use oxiarc_jpeg::{HuffmanTable, QuantTable, TableSet, TablesMode};

/// One component of the frame being written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Component {
    /// Component identifier `Ci`.
    pub(super) id: u8,
    /// Horizontal sampling factor.
    pub(super) h: u8,
    /// Vertical sampling factor.
    pub(super) v: u8,
    /// Quantisation table selector.
    pub(super) quant: u8,
    /// DC Huffman table selector.
    pub(super) dc: u8,
    /// AC Huffman table selector.
    pub(super) ac: u8,
}

/// The frame layout for one chunk.
#[derive(Clone, Debug)]
pub(super) struct Layout {
    /// Frame width in pixels.
    pub(super) width: u16,
    /// Frame height in rows.
    pub(super) height: u16,
    /// The frame's components.
    pub(super) components: Vec<Component>,
}

impl Layout {
    /// `(Hmax, Vmax)`.
    pub(super) fn max_sampling(&self) -> (u8, u8) {
        let h = self
            .components
            .iter()
            .map(|c| c.h)
            .max()
            .unwrap_or(1)
            .max(1);
        let v = self
            .components
            .iter()
            .map(|c| c.v)
            .max()
            .unwrap_or(1)
            .max(1);
        (h, v)
    }

    /// Whether the frame needs a second quantisation and Huffman slot.
    fn has_chroma(&self) -> bool {
        self.components.iter().any(|c| c.quant != 0 || c.ac != 0)
    }
}

/// The quantisation and Huffman tables a layout uses at `quality`.
pub(super) fn table_set(quality: u8, layout: &Layout) -> TableSet {
    let mut tables = TableSet::default();
    tables.quant[0] = Some(QuantTable::annex_k_luma().scaled_for_quality(quality, true));
    tables.dc_huffman[0] =
        HuffmanTable::new(ANNEX_K_DC_LUMA_BITS, ANNEX_K_DC_LUMA_VALUES.to_vec()).ok();
    tables.ac_huffman[0] =
        HuffmanTable::new(ANNEX_K_AC_LUMA_BITS, ANNEX_K_AC_LUMA_VALUES.to_vec()).ok();
    if layout.has_chroma() {
        tables.quant[1] = Some(QuantTable::annex_k_chroma().scaled_for_quality(quality, true));
        tables.dc_huffman[1] =
            HuffmanTable::new(ANNEX_K_DC_CHROMA_BITS, ANNEX_K_DC_CHROMA_VALUES.to_vec()).ok();
        tables.ac_huffman[1] =
            HuffmanTable::new(ANNEX_K_AC_CHROMA_BITS, ANNEX_K_AC_CHROMA_VALUES.to_vec()).ok();
    }
    tables
}

/// The `JPEGTables` (347) blob for a table set, in libtiff's byte layout.
pub(super) fn tables_blob(tables: &TableSet) -> Vec<u8> {
    tables.emit(TablesMode {
        quant: true,
        huffman: true,
    })
}

/// Canonical Huffman codes for encoding, built from a `BITS`/`HUFFVAL` pair.
#[derive(Clone, Debug)]
struct Encoder {
    /// `(code, length)` per symbol value.
    codes: [(u16, u8); 256],
}

impl Encoder {
    /// Builds the canonical code assignment T.81 Annex C describes.
    fn new(table: &HuffmanTable) -> Self {
        let mut codes = [(0u16, 0u8); 256];
        let mut code = 0u16;
        let mut index = 0usize;
        let values = table.values();
        for (slot, count) in table.bits().iter().enumerate() {
            let length = (slot + 1) as u8;
            for _ in 0..*count {
                if let Some(symbol) = values.get(index) {
                    codes[usize::from(*symbol)] = (code, length);
                }
                index += 1;
                code = code.wrapping_add(1);
            }
            code <<= 1;
        }
        Self { codes }
    }

    /// The code for one symbol.
    fn code(&self, symbol: u8) -> (u16, u8) {
        self.codes[usize::from(symbol)]
    }
}

/// An MSB-first bit writer with JPEG's `0xFF` byte stuffing.
#[derive(Debug, Default)]
struct BitWriter {
    out: Vec<u8>,
    accumulator: u32,
    used: u8,
}

impl BitWriter {
    /// Writes the low `length` bits of `value`.
    fn write(&mut self, value: u16, length: u8) {
        if length == 0 {
            return;
        }
        let masked = u32::from(value) & ((1u32 << length) - 1);
        self.accumulator = (self.accumulator << length) | masked;
        self.used += length;
        while self.used >= 8 {
            self.used -= 8;
            let byte = ((self.accumulator >> self.used) & 0xFF) as u8;
            self.out.push(byte);
            if byte == 0xFF {
                // A `0xFF` in entropy data is stuffed so it cannot be read as
                // a marker.
                self.out.push(0x00);
            }
        }
    }

    /// Pads the last byte with one bits, as T.81 §F.1.2.3 requires.
    fn flush(&mut self) {
        if self.used > 0 {
            let pad = 8 - self.used;
            self.write((1u16 << pad) - 1, pad);
        }
        self.used = 0;
        self.accumulator = 0;
    }
}

/// One component's samples at its own resolution.
#[derive(Debug)]
struct Plane {
    data: Vec<u8>,
    width: usize,
    height: usize,
}

impl Plane {
    /// The sample at `(x, y)`, replicating the edges libjpeg-style.
    fn at(&self, x: usize, y: usize) -> u8 {
        let x = x.min(self.width.saturating_sub(1));
        let y = y.min(self.height.saturating_sub(1));
        self.data.get(y * self.width + x).copied().unwrap_or(0)
    }
}

/// The image geometry a plane is extracted from.
#[derive(Clone, Copy, Debug)]
struct Source {
    /// Frame width in pixels.
    width: usize,
    /// Frame height in rows.
    height: usize,
    /// Interleaved channels in the source buffer.
    samples_per_pixel: usize,
    /// Frame-wide `Hmax`.
    hmax: u8,
    /// Frame-wide `Vmax`.
    vmax: u8,
}

/// Extracts one component and box-downsamples it to its sampling factors.
///
/// The averaging window and the edge replication are libjpeg's
/// `h2v1_downsample`/`h2v2_downsample`: the samples outside the image repeat
/// the last column and row, so a 4:2:0 chroma sample at the right edge is not
/// biased towards black.
fn build_plane(src: &[u8], source: Source, index: usize, component: Component) -> Plane {
    let Source {
        width,
        height,
        samples_per_pixel,
        hmax,
        vmax,
    } = source;
    let step_x = usize::from(hmax / component.h.max(1)).max(1);
    let step_y = usize::from(vmax / component.v.max(1)).max(1);
    let plane_width = width.div_ceil(step_x);
    let plane_height = height.div_ceil(step_y);
    let mut data = vec![0u8; plane_width * plane_height];
    let count = (step_x * step_y) as u32;
    for y in 0..plane_height {
        for x in 0..plane_width {
            let mut sum = 0u32;
            for dy in 0..step_y {
                for dx in 0..step_x {
                    let sx = (x * step_x + dx).min(width - 1);
                    let sy = (y * step_y + dy).min(height - 1);
                    let offset = (sy * width + sx) * samples_per_pixel + index;
                    sum += u32::from(src.get(offset).copied().unwrap_or(0));
                }
            }
            data[y * plane_width + x] = ((sum + count / 2) / count) as u8;
        }
    }
    Plane {
        data,
        width: plane_width,
        height: plane_height,
    }
}

/// The 8-point DCT-II basis, `cos((2x + 1) u pi / 16)`.
fn cosine_table() -> [[f64; 8]; 8] {
    let mut table = [[0.0f64; 8]; 8];
    for (u, row) in table.iter_mut().enumerate() {
        for (x, slot) in row.iter_mut().enumerate() {
            let angle = (2.0 * x as f64 + 1.0) * u as f64 * core::f64::consts::PI / 16.0;
            *slot = angle.cos();
        }
    }
    table
}

/// The forward DCT of one level-shifted 8x8 block.
///
/// This is the mathematical transform (libjpeg's `jpeg_fdct_float`), not the
/// scaled integer approximation: an encoder is free to be *more* accurate than
/// `islow`, and the decoder's exactness is what interoperability depends on.
fn forward_dct(block: &[f64; 64], cosines: &[[f64; 8]; 8], out: &mut [f64; 64]) {
    let mut rows = [0.0f64; 64];
    for y in 0..8 {
        for u in 0..8 {
            let mut sum = 0.0;
            for x in 0..8 {
                sum += block[y * 8 + x] * cosines[u][x];
            }
            let scale = if u == 0 {
                core::f64::consts::FRAC_1_SQRT_2
            } else {
                1.0
            };
            rows[y * 8 + u] = sum * scale;
        }
    }
    for u in 0..8 {
        for v in 0..8 {
            let mut sum = 0.0;
            for y in 0..8 {
                sum += rows[y * 8 + u] * cosines[v][y];
            }
            let scale = if v == 0 {
                core::f64::consts::FRAC_1_SQRT_2
            } else {
                1.0
            };
            out[v * 8 + u] = sum * scale * 0.25;
        }
    }
}

/// The number of magnitude bits a coefficient needs.
fn magnitude_category(value: i32) -> u8 {
    let mut magnitude = value.unsigned_abs();
    let mut category = 0u8;
    while magnitude > 0 {
        magnitude >>= 1;
        category += 1;
    }
    category
}

/// The bits T.81 stores for a coefficient of `category` bits.
fn magnitude_bits(value: i32, category: u8) -> u16 {
    if value >= 0 {
        value as u16
    } else {
        (value + (1 << category) - 1) as u16
    }
}

/// Encodes one chunk as an abbreviated JPEG image datastream.
///
/// `inline_tables` carries `DQT`/`DHT` segments to place before the frame
/// header; pass `None` when the tables live in tag 347.
pub(super) fn encode_chunk(
    src: &[u8],
    layout: &Layout,
    tables: &TableSet,
    inline_tables: Option<&[u8]>,
) -> Vec<u8> {
    let width = usize::from(layout.width);
    let height = usize::from(layout.height);
    let samples_per_pixel = layout.components.len().max(1);
    let (hmax, vmax) = layout.max_sampling();

    let planes: Vec<Plane> = layout
        .components
        .iter()
        .enumerate()
        .map(|(index, component)| {
            build_plane(
                src,
                Source {
                    width: width.max(1),
                    height: height.max(1),
                    samples_per_pixel,
                    hmax,
                    vmax,
                },
                index,
                *component,
            )
        })
        .collect();

    let mut out = Vec::with_capacity(src.len() / 4 + 256);
    out.extend_from_slice(&[0xFF, 0xD8]);
    if let Some(bytes) = inline_tables {
        out.extend_from_slice(bytes);
    }
    write_frame_header(&mut out, layout);
    write_scan_header(&mut out, layout);

    let cosines = cosine_table();
    let dc_encoders: Vec<Option<Encoder>> = tables
        .dc_huffman
        .iter()
        .map(|slot| slot.as_ref().map(Encoder::new))
        .collect();
    let ac_encoders: Vec<Option<Encoder>> = tables
        .ac_huffman
        .iter()
        .map(|slot| slot.as_ref().map(Encoder::new))
        .collect();

    let mcus_across = width.div_ceil(8 * usize::from(hmax)).max(1);
    let mcus_down = height.div_ceil(8 * usize::from(vmax)).max(1);
    let mut writer = BitWriter::default();
    let mut previous_dc = vec![0i32; layout.components.len()];
    let mut block = [0.0f64; 64];
    let mut coefficients = [0.0f64; 64];

    for mcu_y in 0..mcus_down {
        for mcu_x in 0..mcus_across {
            for (index, component) in layout.components.iter().enumerate() {
                let Some(plane) = planes.get(index) else {
                    continue;
                };
                let quant = tables
                    .quant
                    .get(usize::from(component.quant))
                    .and_then(|slot| slot.as_ref());
                for by in 0..usize::from(component.v) {
                    for bx in 0..usize::from(component.h) {
                        let origin_x = (mcu_x * usize::from(component.h) + bx) * 8;
                        let origin_y = (mcu_y * usize::from(component.v) + by) * 8;
                        for y in 0..8 {
                            for x in 0..8 {
                                let sample = plane.at(origin_x + x, origin_y + y);
                                block[y * 8 + x] = f64::from(sample) - 128.0;
                            }
                        }
                        forward_dct(&block, &cosines, &mut coefficients);
                        let dc_encoder = dc_encoders
                            .get(usize::from(component.dc))
                            .and_then(|slot| slot.as_ref());
                        let ac_encoder = ac_encoders
                            .get(usize::from(component.ac))
                            .and_then(|slot| slot.as_ref());
                        let previous = previous_dc.get(index).copied().unwrap_or(0);
                        let dc = encode_block(
                            &mut writer,
                            &coefficients,
                            quant,
                            dc_encoder,
                            ac_encoder,
                            previous,
                        );
                        if let Some(slot) = previous_dc.get_mut(index) {
                            *slot = dc;
                        }
                    }
                }
            }
        }
    }
    writer.flush();
    out.extend_from_slice(&writer.out);
    out.extend_from_slice(&[0xFF, 0xD9]);
    out
}

/// Quantises and entropy-codes one block, returning its DC coefficient.
fn encode_block(
    writer: &mut BitWriter,
    coefficients: &[f64; 64],
    quant: Option<&QuantTable>,
    dc_encoder: Option<&Encoder>,
    ac_encoder: Option<&Encoder>,
    previous_dc: i32,
) -> i32 {
    let mut zigzag = [0i32; 64];
    for (index, slot) in zigzag.iter_mut().enumerate() {
        let natural = ZIGZAG_TO_NATURAL[index];
        let divisor = quant.map_or(1u16, |table| table.value(natural)).max(1);
        let value = coefficients[natural] / f64::from(divisor);
        *slot = value.round() as i32;
    }
    let dc = zigzag[0];
    let diff = dc - previous_dc;
    let category = magnitude_category(diff);
    if let Some(encoder) = dc_encoder {
        let (code, length) = encoder.code(category);
        writer.write(code, length);
    }
    if category > 0 {
        writer.write(magnitude_bits(diff, category), category);
    }

    let mut run = 0u8;
    for value in zigzag.iter().skip(1).copied() {
        if value == 0 {
            run += 1;
            continue;
        }
        while run >= 16 {
            if let Some(encoder) = ac_encoder {
                let (code, length) = encoder.code(0xF0);
                writer.write(code, length);
            }
            run -= 16;
        }
        let category = magnitude_category(value);
        let symbol = (run << 4) | (category & 0x0F);
        if let Some(encoder) = ac_encoder {
            let (code, length) = encoder.code(symbol);
            writer.write(code, length);
        }
        writer.write(magnitude_bits(value, category), category);
        run = 0;
    }
    if run > 0 {
        if let Some(encoder) = ac_encoder {
            let (code, length) = encoder.code(0x00);
            writer.write(code, length);
        }
    }
    dc
}

/// Writes the `SOF0` segment.
fn write_frame_header(out: &mut Vec<u8>, layout: &Layout) {
    let count = layout.components.len().min(4) as u8;
    let length = 8 + 3 * u16::from(count);
    out.extend_from_slice(&[0xFF, 0xC0]);
    out.extend_from_slice(&length.to_be_bytes());
    out.push(8);
    out.extend_from_slice(&layout.height.to_be_bytes());
    out.extend_from_slice(&layout.width.to_be_bytes());
    out.push(count);
    for component in layout.components.iter().take(4) {
        out.push(component.id);
        out.push((component.h << 4) | (component.v & 0x0F));
        out.push(component.quant);
    }
}

/// Writes the `SOS` segment for a baseline sequential scan.
fn write_scan_header(out: &mut Vec<u8>, layout: &Layout) {
    let count = layout.components.len().min(4) as u8;
    let length = 6 + 2 * u16::from(count);
    out.extend_from_slice(&[0xFF, 0xDA]);
    out.extend_from_slice(&length.to_be_bytes());
    out.push(count);
    for component in layout.components.iter().take(4) {
        out.push(component.id);
        out.push((component.dc << 4) | (component.ac & 0x0F));
    }
    out.extend_from_slice(&[0x00, 0x3F, 0x00]);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gray_layout(width: u16, height: u16) -> Layout {
        Layout {
            width,
            height,
            components: vec![Component {
                id: 1,
                h: 1,
                v: 1,
                quant: 0,
                dc: 0,
                ac: 0,
            }],
        }
    }

    #[test]
    fn the_canonical_codes_match_annex_c() {
        let table = HuffmanTable::new(ANNEX_K_DC_LUMA_BITS, ANNEX_K_DC_LUMA_VALUES.to_vec())
            .expect("table");
        let encoder = Encoder::new(&table);
        // Annex K's DC luma table: symbol 0 is 00 (2 bits), 1..5 are 3 bits.
        assert_eq!(encoder.code(0), (0b00, 2));
        assert_eq!(encoder.code(1), (0b010, 3));
        assert_eq!(encoder.code(5), (0b110, 3));
        assert_eq!(encoder.code(6), (0b1110, 4));
    }

    #[test]
    fn magnitude_categories_follow_the_spec() {
        assert_eq!(magnitude_category(0), 0);
        assert_eq!(magnitude_category(1), 1);
        assert_eq!(magnitude_category(-1), 1);
        assert_eq!(magnitude_category(255), 8);
        assert_eq!(magnitude_bits(5, 3), 0b101);
        // Negative values store the one's complement of the magnitude.
        assert_eq!(magnitude_bits(-5, 3), 0b010);
        assert_eq!(magnitude_bits(-1, 1), 0b0);
    }

    #[test]
    fn the_dct_of_a_flat_block_is_a_dc_term_only() {
        let cosines = cosine_table();
        let block = [7.0f64; 64];
        let mut out = [0.0f64; 64];
        forward_dct(&block, &cosines, &mut out);
        assert!((out[0] - 7.0 * 8.0).abs() < 1e-9, "{}", out[0]);
        for value in out.iter().skip(1) {
            assert!(value.abs() < 1e-9);
        }
    }

    #[test]
    fn ff_bytes_in_entropy_data_are_stuffed() {
        let mut writer = BitWriter::default();
        writer.write(0xFF, 8);
        writer.flush();
        assert_eq!(writer.out, vec![0xFF, 0x00]);
    }

    #[test]
    fn a_frame_header_matches_the_libtiff_shape() {
        let layout = gray_layout(64, 16);
        let mut out = Vec::new();
        write_frame_header(&mut out, &layout);
        assert_eq!(
            out,
            vec![
                0xFF, 0xC0, 0x00, 0x0B, 8, 0x00, 0x10, 0x00, 0x40, 1, 1, 0x11, 0
            ]
        );
        let mut scan = Vec::new();
        write_scan_header(&mut scan, &layout);
        assert_eq!(
            scan,
            vec![0xFF, 0xDA, 0x00, 0x08, 1, 1, 0x00, 0x00, 0x3F, 0x00]
        );
    }

    #[test]
    fn a_chunk_is_a_complete_datastream() {
        let layout = gray_layout(16, 8);
        let tables = table_set(75, &layout);
        let pixels: Vec<u8> = (0..16 * 8u32).map(|i| (i * 3 % 256) as u8).collect();
        let blob = tables_blob(&tables);
        assert_eq!(blob.get(..2), Some(&[0xFF, 0xD8][..]));
        assert_eq!(blob.get(blob.len() - 2..), Some(&[0xFF, 0xD9][..]));
        let stream = encode_chunk(&pixels, &layout, &tables, None);
        assert_eq!(stream.get(..2), Some(&[0xFF, 0xD8][..]));
        assert_eq!(stream.get(2..4), Some(&[0xFF, 0xC0][..]));
        assert_eq!(stream.get(stream.len() - 2..), Some(&[0xFF, 0xD9][..]));
        // The tables round trip through the `oxiarc-jpeg` parser.
        let parsed = TableSet::parse(&blob).expect("parse");
        assert!(parsed.quant[0].is_some());
        assert!(parsed.dc_huffman[0].is_some());
        assert!(parsed.ac_huffman[0].is_some());
    }

    #[test]
    fn planes_are_box_downsampled_with_edge_replication() {
        // 3x3 image, one component, 2x2 subsampling: the plane is 2x2 and the
        // right/bottom edges repeat rather than reading zero.
        let src: Vec<u8> = vec![10, 20, 30, 40, 50, 60, 70, 80, 90];
        let component = Component {
            id: 1,
            h: 1,
            v: 1,
            quant: 1,
            dc: 1,
            ac: 1,
        };
        let plane = build_plane(
            &src,
            Source {
                width: 3,
                height: 3,
                samples_per_pixel: 1,
                hmax: 2,
                vmax: 2,
            },
            0,
            component,
        );
        assert_eq!((plane.width, plane.height), (2, 2));
        assert_eq!(plane.data[0], ((10 + 20 + 40 + 50 + 2) / 4) as u8);
        assert_eq!(plane.data[1], ((30 + 30 + 60 + 60 + 2) / 4) as u8);
        assert_eq!(plane.at(9, 9), plane.data[3], "edges replicate");
    }
}
