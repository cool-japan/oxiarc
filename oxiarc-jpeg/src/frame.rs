//! Frame and scan headers: `SOF`, `SOS`, `DRI`, `DNL` and `DAC`.

use crate::error::{JpegError, Result, UnsupportedFeature};
use crate::limits::DecodeLimits;

/// The coding process a `SOF` marker selects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum CodingProcess {
    /// `SOF0` — baseline sequential DCT, 8-bit only, at most two tables of
    /// each class.
    Baseline,
    /// `SOF1`/`SOF9` — extended sequential DCT, 8- or 12-bit.
    ExtendedSequential,
    /// `SOF2`/`SOF10` — progressive DCT, 8- or 12-bit.
    Progressive,
    /// `SOF3`/`SOF11` — lossless (sequential) predictive, 2..=16 bit.
    Lossless,
}

/// Which entropy coder the frame uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntropyCoding {
    /// Huffman coding (Annex F).
    Huffman,
    /// Arithmetic coding (Annex D).
    Arithmetic,
}

/// One component of a frame (`Ci`, `Hi`, `Vi`, `Tqi` plus derived geometry).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Component {
    /// Component identifier `Ci` as written in the `SOF`.
    pub id: u8,
    /// Horizontal sampling factor `Hi`, `1..=4`.
    pub h: u8,
    /// Vertical sampling factor `Vi`, `1..=4`.
    pub v: u8,
    /// Quantisation table selector `Tqi`, `0..=3`.
    pub quant_table: u8,
    /// Samples per row this component actually carries,
    /// `ceil(X * Hi / Hmax)`.
    pub width_samples: u32,
    /// Sample rows this component actually carries, `ceil(Y * Vi / Vmax)`.
    pub height_samples: u32,
    /// Blocks per row the component actually carries,
    /// `ceil(width_samples / 8)`. This is the loop bound of a
    /// **non-interleaved** scan.
    pub blocks_per_line: u32,
    /// Block rows the component actually carries,
    /// `ceil(height_samples / 8)`.
    pub blocks_per_column: u32,
    /// Blocks per row once padded to whole MCUs, `mcus_per_line * Hi`. This
    /// is the coefficient buffer's row stride.
    pub blocks_per_line_padded: u32,
    /// Block rows once padded to whole MCUs, `mcus_per_column * Vi`.
    pub blocks_per_column_padded: u32,
}

/// A parsed `SOF` segment plus the geometry derived from it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameHeader {
    /// The `SOF` marker code (`0xC0..=0xCF`).
    pub marker: u8,
    /// Which coding process the marker selects.
    pub process: CodingProcess,
    /// Which entropy coder the marker selects.
    pub entropy: EntropyCoding,
    /// Sample precision `P` in bits.
    pub precision: u8,
    /// Frame width `X` in samples.
    pub width: u16,
    /// Frame height `Y` in lines. Zero until a `DNL` segment resolves it.
    pub height: u16,
    /// The frame's components, in `SOF` order.
    pub components: Vec<Component>,
    /// Largest `Hi` over all components.
    pub hmax: u8,
    /// Largest `Vi` over all components.
    pub vmax: u8,
    /// MCUs per row, `ceil(X / (8 * Hmax))`.
    pub mcus_per_line: u32,
    /// MCU rows, `ceil(Y / (8 * Vmax))`.
    pub mcus_per_column: u32,
}

impl FrameHeader {
    /// `true` when the frame is coded with the progressive DCT process.
    #[must_use]
    pub fn is_progressive(&self) -> bool {
        self.process == CodingProcess::Progressive
    }

    /// `true` when the frame is coded with the lossless predictive process.
    #[must_use]
    pub fn is_lossless(&self) -> bool {
        self.process == CodingProcess::Lossless
    }

    /// Index of the component with identifier `id`.
    #[must_use]
    pub fn component_index(&self, id: u8) -> Option<usize> {
        self.components.iter().position(|c| c.id == id)
    }

    /// Recompute the derived geometry after `DNL` supplied the real height.
    pub(crate) fn set_height(&mut self, height: u16) {
        self.height = height;
        derive_geometry(self);
    }
}

/// A parsed `SOS` segment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanHeader {
    /// Indices into [`FrameHeader::components`], in scan order.
    pub component_indices: Vec<usize>,
    /// `Tdj` per scan component.
    pub dc_table: Vec<u8>,
    /// `Taj` per scan component.
    pub ac_table: Vec<u8>,
    /// `Ss` — spectral selection start, or `Psv` for lossless frames.
    pub spectral_start: u8,
    /// `Se` — spectral selection end (`0` for lossless).
    pub spectral_end: u8,
    /// `Ah` — successive approximation high bit position.
    pub approx_high: u8,
    /// `Al` — successive approximation low bit position, or `Pt` for lossless.
    pub approx_low: u8,
}

impl ScanHeader {
    /// `true` when the scan interleaves more than one component.
    #[must_use]
    pub fn is_interleaved(&self) -> bool {
        self.component_indices.len() > 1
    }
}

/// Arithmetic conditioning parameters from `DAC` (T.81 Annex B.2.4.3).
///
/// Parsed and retained even when arithmetic decoding is not available, so
/// that a stream's tables survive a `TableSet` round-trip and so the
/// arithmetic decoder can be added without changing the parser.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArithmeticConditioning {
    /// DC conditioning `Cs` per table slot: the packed `(U << 4) | L` byte,
    /// where `L` is the low nibble and `U` the high one. The T.81 default is
    /// `L = 0`, `U = 1`, i.e. `0x10`.
    pub dc: [u8; 4],
    /// AC conditioning `Kx` per table slot. The T.81 default is `5`.
    pub ac: [u8; 4],
}

impl Default for ArithmeticConditioning {
    fn default() -> Self {
        Self {
            dc: [0x10; 4],
            ac: [5; 4],
        }
    }
}

impl ArithmeticConditioning {
    /// Parse a `DAC` payload (everything after the length field).
    pub(crate) fn parse(&mut self, payload: &[u8], offset: usize) -> Result<()> {
        if payload.len() % 2 != 0 {
            return Err(JpegError::malformed(
                "DAC",
                offset,
                "payload length must be even",
            ));
        }
        for pair in payload.chunks_exact(2) {
            let tc = pair[0] >> 4;
            let tb = pair[0] & 0x0F;
            if tc > 1 {
                return Err(JpegError::malformed("DAC", offset, "Tc must be 0 or 1"));
            }
            if tb > 3 {
                return Err(JpegError::malformed("DAC", offset, "Tb must be 0..=3"));
            }
            let cs = pair[1];
            if tc == 0 {
                let low = cs & 0x0F;
                let high = cs >> 4;
                if low > high {
                    return Err(JpegError::malformed(
                        "DAC",
                        offset,
                        "DC conditioning requires L <= U",
                    ));
                }
                self.dc[usize::from(tb)] = cs;
            } else {
                if cs == 0 || cs > 63 {
                    return Err(JpegError::malformed(
                        "DAC",
                        offset,
                        "AC conditioning Kx must be 1..=63",
                    ));
                }
                self.ac[usize::from(tb)] = cs;
            }
        }
        Ok(())
    }

    /// Emit `DAC` segments for every slot that differs from the T.81 default.
    pub(crate) fn emit(&self, out: &mut Vec<u8>) {
        let default = ArithmeticConditioning::default();
        let mut pairs: Vec<[u8; 2]> = Vec::new();
        for (slot, (&value, &def)) in self.dc.iter().zip(default.dc.iter()).enumerate() {
            if value != def {
                pairs.push([slot as u8, value]);
            }
        }
        for (slot, (&value, &def)) in self.ac.iter().zip(default.ac.iter()).enumerate() {
            if value != def {
                pairs.push([0x10 | slot as u8, value]);
            }
        }
        if pairs.is_empty() {
            return;
        }
        let len = 2 + 2 * pairs.len();
        out.push(0xFF);
        out.push(0xCC);
        out.push((len >> 8) as u8);
        out.push((len & 0xFF) as u8);
        for pair in pairs {
            out.extend_from_slice(&pair);
        }
    }
}

/// Classify a `SOF` marker code.
///
/// Returns the coding process and entropy coder, or an `Unsupported` error
/// for the hierarchical codes.
pub(crate) fn classify_sof(marker: u8) -> Result<(CodingProcess, EntropyCoding)> {
    use CodingProcess::{Baseline, ExtendedSequential, Lossless, Progressive};
    use EntropyCoding::{Arithmetic, Huffman};
    match marker {
        0xC0 => Ok((Baseline, Huffman)),
        0xC1 => Ok((ExtendedSequential, Huffman)),
        0xC2 => Ok((Progressive, Huffman)),
        0xC3 => Ok((Lossless, Huffman)),
        0xC9 => Ok((ExtendedSequential, Arithmetic)),
        0xCA => Ok((Progressive, Arithmetic)),
        0xCB => Ok((Lossless, Arithmetic)),
        0xC5 | 0xC6 | 0xC7 | 0xCD | 0xCE | 0xCF => {
            Err(JpegError::Unsupported(UnsupportedFeature::Hierarchical))
        }
        _ => Err(JpegError::malformed("SOF", 0, "not a frame marker")),
    }
}

/// Parse a `SOF` payload (everything after the length field).
pub(crate) fn parse_sof(
    marker: u8,
    payload: &[u8],
    offset: usize,
    limits: &DecodeLimits,
) -> Result<FrameHeader> {
    let (process, entropy) = classify_sof(marker)?;
    if payload.len() < 6 {
        return Err(JpegError::malformed("SOF", offset, "segment too short"));
    }
    let precision = payload[0];
    let height = u16::from_be_bytes([payload[1], payload[2]]);
    let width = u16::from_be_bytes([payload[3], payload[4]]);
    let num_components = payload[5];

    match process {
        CodingProcess::Lossless => {
            if !(2..=16).contains(&precision) {
                return Err(JpegError::Unsupported(UnsupportedFeature::SamplePrecision(
                    precision,
                )));
            }
        }
        CodingProcess::Baseline => {
            if precision != 8 {
                return Err(JpegError::Unsupported(UnsupportedFeature::SamplePrecision(
                    precision,
                )));
            }
        }
        _ => {
            if precision != 8 && precision != 12 {
                return Err(JpegError::Unsupported(UnsupportedFeature::SamplePrecision(
                    precision,
                )));
            }
        }
    }

    if width == 0 {
        return Err(JpegError::malformed("SOF", offset, "X must be non-zero"));
    }
    if num_components == 0 {
        return Err(JpegError::Unsupported(UnsupportedFeature::ComponentCount(
            0,
        )));
    }
    limits.check_components(usize::from(num_components))?;
    limits.check_dimensions(u32::from(width), u32::from(height))?;

    let need = 6 + 3 * usize::from(num_components);
    if payload.len() < need {
        return Err(JpegError::malformed(
            "SOF",
            offset,
            "component list is truncated",
        ));
    }

    let mut components = Vec::with_capacity(usize::from(num_components));
    for i in 0..usize::from(num_components) {
        let base = 6 + 3 * i;
        let id = payload[base];
        let h = payload[base + 1] >> 4;
        let v = payload[base + 1] & 0x0F;
        let quant_table = payload[base + 2];
        if h == 0 || h > 4 || v == 0 || v > 4 {
            return Err(JpegError::Unsupported(UnsupportedFeature::SamplingFactor {
                h,
                v,
            }));
        }
        if quant_table > 3 {
            return Err(JpegError::malformed("SOF", offset, "Tq must be 0..=3"));
        }
        if components.iter().any(|c: &Component| c.id == id) {
            return Err(JpegError::malformed(
                "SOF",
                offset,
                "duplicate component identifier",
            ));
        }
        components.push(Component {
            id,
            h,
            v,
            quant_table,
            width_samples: 0,
            height_samples: 0,
            blocks_per_line: 0,
            blocks_per_column: 0,
            blocks_per_line_padded: 0,
            blocks_per_column_padded: 0,
        });
    }

    let mut frame = FrameHeader {
        marker,
        process,
        entropy,
        precision,
        width,
        height,
        components,
        hmax: 1,
        vmax: 1,
        mcus_per_line: 0,
        mcus_per_column: 0,
    };
    derive_geometry(&mut frame);
    Ok(frame)
}

/// Recompute every derived geometry field of `frame`.
fn derive_geometry(frame: &mut FrameHeader) {
    let hmax = frame.components.iter().map(|c| c.h).max().unwrap_or(1);
    let vmax = frame.components.iter().map(|c| c.v).max().unwrap_or(1);
    frame.hmax = hmax;
    frame.vmax = vmax;

    let width = u32::from(frame.width);
    let height = u32::from(frame.height);
    let hmax32 = u32::from(hmax);
    let vmax32 = u32::from(vmax);

    // A lossless MCU is `Hi` x `Vi` *samples*, not 8x8 blocks (T.81 A.2.3),
    // so its MCU grid is eight times finer than a DCT frame's.
    let lossless = frame.process == CodingProcess::Lossless;
    let mcu_width = if lossless { hmax32 } else { 8 * hmax32 };
    let mcu_height = if lossless { vmax32 } else { 8 * vmax32 };
    frame.mcus_per_line = width.div_ceil(mcu_width);
    frame.mcus_per_column = height.div_ceil(mcu_height);

    for component in &mut frame.components {
        let h = u32::from(component.h);
        let v = u32::from(component.v);
        component.width_samples = (width * h).div_ceil(hmax32);
        component.height_samples = (height * v).div_ceil(vmax32);
        component.blocks_per_line = component.width_samples.div_ceil(8);
        component.blocks_per_column = component.height_samples.div_ceil(8);
        if lossless {
            component.blocks_per_line_padded = component.blocks_per_line;
            component.blocks_per_column_padded = component.blocks_per_column;
        } else {
            component.blocks_per_line_padded = frame.mcus_per_line * h;
            component.blocks_per_column_padded = frame.mcus_per_column * v;
        }
    }
}

/// Parse a `SOS` payload (everything after the length field).
pub(crate) fn parse_sos(payload: &[u8], offset: usize, frame: &FrameHeader) -> Result<ScanHeader> {
    if payload.is_empty() {
        return Err(JpegError::malformed("SOS", offset, "segment too short"));
    }
    let ns = usize::from(payload[0]);
    if ns == 0 || ns > 4 {
        return Err(JpegError::malformed("SOS", offset, "Ns must be 1..=4"));
    }
    if payload.len() < 1 + 2 * ns + 3 {
        return Err(JpegError::malformed("SOS", offset, "segment truncated"));
    }

    let mut component_indices = Vec::with_capacity(ns);
    let mut dc_table = Vec::with_capacity(ns);
    let mut ac_table = Vec::with_capacity(ns);
    for i in 0..ns {
        let cs = payload[1 + 2 * i];
        let tables = payload[2 + 2 * i];
        let index = frame.component_index(cs).ok_or(JpegError::malformed(
            "SOS",
            offset,
            "scan names a component the frame does not define",
        ))?;
        if component_indices.contains(&index) {
            return Err(JpegError::malformed(
                "SOS",
                offset,
                "component appears twice in one scan",
            ));
        }
        component_indices.push(index);
        dc_table.push(tables >> 4);
        ac_table.push(tables & 0x0F);
    }
    if dc_table.iter().any(|&t| t > 3) || ac_table.iter().any(|&t| t > 3) {
        return Err(JpegError::malformed(
            "SOS",
            offset,
            "table selector must be 0..=3",
        ));
    }

    let tail = 1 + 2 * ns;
    let spectral_start = payload[tail];
    let spectral_end = payload[tail + 1];
    let approx = payload[tail + 2];

    Ok(ScanHeader {
        component_indices,
        dc_table,
        ac_table,
        spectral_start,
        spectral_end,
        approx_high: approx >> 4,
        approx_low: approx & 0x0F,
    })
}

/// Validate a DCT scan's spectral selection and successive approximation.
pub(crate) fn validate_dct_scan(
    scan: &ScanHeader,
    frame: &FrameHeader,
    offset: usize,
) -> Result<()> {
    if frame.is_progressive() {
        if scan.spectral_end > 63 || scan.spectral_start > scan.spectral_end {
            return Err(JpegError::malformed("SOS", offset, "invalid Ss/Se"));
        }
        if scan.spectral_start == 0 && scan.spectral_end != 0 {
            return Err(JpegError::malformed(
                "SOS",
                offset,
                "a DC scan must have Se == 0",
            ));
        }
        if scan.spectral_start != 0 && scan.is_interleaved() {
            return Err(JpegError::malformed(
                "SOS",
                offset,
                "an AC scan must name exactly one component",
            ));
        }
        if scan.approx_low > 13 || scan.approx_high > 13 {
            return Err(JpegError::malformed("SOS", offset, "Ah/Al must be 0..=13"));
        }
        if scan.approx_high != 0 && scan.approx_high != scan.approx_low + 1 {
            return Err(JpegError::malformed(
                "SOS",
                offset,
                "a refinement scan needs Ah == Al + 1",
            ));
        }
    } else {
        if scan.spectral_start != 0 || scan.spectral_end != 63 {
            return Err(JpegError::malformed(
                "SOS",
                offset,
                "a sequential scan needs Ss == 0 and Se == 63",
            ));
        }
        if scan.approx_high != 0 || scan.approx_low != 0 {
            return Err(JpegError::malformed(
                "SOS",
                offset,
                "a sequential scan needs Ah == Al == 0",
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sof_payload(precision: u8, y: u16, x: u16, comps: &[(u8, u8, u8, u8)]) -> Vec<u8> {
        let mut payload = vec![precision];
        payload.extend_from_slice(&y.to_be_bytes());
        payload.extend_from_slice(&x.to_be_bytes());
        payload.push(comps.len() as u8);
        for &(id, h, v, tq) in comps {
            payload.push(id);
            payload.push((h << 4) | v);
            payload.push(tq);
        }
        payload
    }

    #[test]
    fn classifies_every_frame_marker() {
        assert_eq!(
            classify_sof(0xC0).expect("SOF0"),
            (CodingProcess::Baseline, EntropyCoding::Huffman)
        );
        assert_eq!(
            classify_sof(0xC2).expect("SOF2"),
            (CodingProcess::Progressive, EntropyCoding::Huffman)
        );
        assert_eq!(
            classify_sof(0xC3).expect("SOF3"),
            (CodingProcess::Lossless, EntropyCoding::Huffman)
        );
        assert_eq!(
            classify_sof(0xCA).expect("SOF10"),
            (CodingProcess::Progressive, EntropyCoding::Arithmetic)
        );
        for hierarchical in [0xC5u8, 0xC6, 0xC7, 0xCD, 0xCE, 0xCF] {
            assert!(matches!(
                classify_sof(hierarchical),
                Err(JpegError::Unsupported(UnsupportedFeature::Hierarchical))
            ));
        }
    }

    #[test]
    fn derives_4_2_0_geometry() {
        let payload = sof_payload(8, 97, 131, &[(1, 2, 2, 0), (2, 1, 1, 1), (3, 1, 1, 1)]);
        let frame = parse_sof(0xC0, &payload, 0, &DecodeLimits::default()).expect("SOF");
        assert_eq!((frame.hmax, frame.vmax), (2, 2));
        assert_eq!(frame.mcus_per_line, 131_u32.div_ceil(16));
        assert_eq!(frame.mcus_per_column, 97_u32.div_ceil(16));

        let luma = frame.components[0];
        assert_eq!(luma.width_samples, 131);
        assert_eq!(luma.height_samples, 97);
        assert_eq!(luma.blocks_per_line, 17);
        assert_eq!(luma.blocks_per_column, 13);
        assert_eq!(luma.blocks_per_line_padded, 9 * 2);
        assert_eq!(luma.blocks_per_column_padded, 7 * 2);

        let chroma = frame.components[1];
        assert_eq!(chroma.width_samples, 66);
        assert_eq!(chroma.height_samples, 49);
        assert_eq!(chroma.blocks_per_line, 9);
        assert_eq!(chroma.blocks_per_column, 7);
        assert_eq!(chroma.blocks_per_line_padded, 9);
    }

    /// A single-component frame that declares `H = V = 2` is legal and common
    /// in real grayscale files; `Hmax = Vmax = 2`, so the component still
    /// carries one block per eight pixels.
    #[test]
    fn single_component_with_2x2_sampling_is_not_scaled() {
        let payload = sof_payload(8, 40, 40, &[(1, 2, 2, 0)]);
        let frame = parse_sof(0xC0, &payload, 0, &DecodeLimits::default()).expect("SOF");
        let comp = frame.components[0];
        assert_eq!(comp.width_samples, 40);
        assert_eq!(comp.height_samples, 40);
        assert_eq!(comp.blocks_per_line, 5);
        assert_eq!(frame.mcus_per_line, 3);
        assert_eq!(comp.blocks_per_line_padded, 6);
    }

    #[test]
    fn odd_sampling_factors_are_accepted() {
        // 4:1:1 (H = 4) and a 1x2 luma.
        let payload = sof_payload(8, 16, 16, &[(1, 4, 1, 0), (2, 1, 1, 1), (3, 1, 1, 1)]);
        let frame = parse_sof(0xC0, &payload, 0, &DecodeLimits::default()).expect("4:1:1");
        assert_eq!(frame.components[1].width_samples, 4);

        let payload = sof_payload(8, 16, 16, &[(1, 1, 2, 0), (2, 1, 1, 1)]);
        let frame = parse_sof(0xC0, &payload, 0, &DecodeLimits::default()).expect("1x2");
        assert_eq!(frame.components[1].height_samples, 8);
    }

    #[test]
    fn rejects_bad_frames() {
        let limits = DecodeLimits::default();
        // Zero width.
        let payload = sof_payload(8, 8, 0, &[(1, 1, 1, 0)]);
        assert!(parse_sof(0xC0, &payload, 0, &limits).is_err());
        // Zero components.
        let payload = sof_payload(8, 8, 8, &[]);
        assert!(parse_sof(0xC0, &payload, 0, &limits).is_err());
        // Sampling factor 0 and 5.
        let payload = sof_payload(8, 8, 8, &[(1, 0, 1, 0)]);
        assert!(parse_sof(0xC0, &payload, 0, &limits).is_err());
        let payload = sof_payload(8, 8, 8, &[(1, 5, 1, 0)]);
        assert!(parse_sof(0xC0, &payload, 0, &limits).is_err());
        // Tq out of range.
        let payload = sof_payload(8, 8, 8, &[(1, 1, 1, 4)]);
        assert!(parse_sof(0xC0, &payload, 0, &limits).is_err());
        // Duplicate identifiers.
        let payload = sof_payload(8, 8, 8, &[(1, 1, 1, 0), (1, 1, 1, 0)]);
        assert!(parse_sof(0xC0, &payload, 0, &limits).is_err());
        // Truncated component list.
        let mut payload = sof_payload(8, 8, 8, &[(1, 1, 1, 0)]);
        payload.truncate(7);
        assert!(parse_sof(0xC0, &payload, 0, &limits).is_err());
    }

    #[test]
    fn precision_rules_differ_per_process() {
        let limits = DecodeLimits::default();
        let payload = sof_payload(12, 8, 8, &[(1, 1, 1, 0)]);
        assert!(
            parse_sof(0xC0, &payload, 0, &limits).is_err(),
            "baseline 12"
        );
        assert!(parse_sof(0xC1, &payload, 0, &limits).is_ok(), "extended 12");
        let payload = sof_payload(16, 8, 8, &[(1, 1, 1, 0)]);
        assert!(
            parse_sof(0xC1, &payload, 0, &limits).is_err(),
            "extended 16"
        );
        assert!(parse_sof(0xC3, &payload, 0, &limits).is_ok(), "lossless 16");
        let payload = sof_payload(1, 8, 8, &[(1, 1, 1, 0)]);
        assert!(parse_sof(0xC3, &payload, 0, &limits).is_err(), "lossless 1");
    }

    #[test]
    fn height_zero_is_accepted_until_dnl() {
        let payload = sof_payload(8, 0, 16, &[(1, 1, 1, 0)]);
        let mut frame = parse_sof(0xC0, &payload, 0, &DecodeLimits::default()).expect("SOF");
        assert_eq!(frame.mcus_per_column, 0);
        frame.set_height(20);
        assert_eq!(frame.mcus_per_column, 3);
        assert_eq!(frame.components[0].height_samples, 20);
    }

    fn simple_frame() -> FrameHeader {
        let payload = sof_payload(8, 16, 16, &[(1, 2, 2, 0), (2, 1, 1, 1), (3, 1, 1, 1)]);
        parse_sof(0xC0, &payload, 0, &DecodeLimits::default()).expect("SOF")
    }

    #[test]
    fn parses_an_interleaved_scan() {
        let frame = simple_frame();
        let payload = [3u8, 1, 0x00, 2, 0x11, 3, 0x11, 0, 63, 0x00];
        let scan = parse_sos(&payload, 0, &frame).expect("SOS");
        assert_eq!(scan.component_indices, vec![0, 1, 2]);
        assert_eq!(scan.dc_table, vec![0, 1, 1]);
        assert_eq!(scan.ac_table, vec![0, 1, 1]);
        assert_eq!((scan.spectral_start, scan.spectral_end), (0, 63));
        assert!(scan.is_interleaved());
        assert!(validate_dct_scan(&scan, &frame, 0).is_ok());
    }

    #[test]
    fn rejects_bad_scans() {
        let frame = simple_frame();
        assert!(parse_sos(&[0u8], 0, &frame).is_err(), "Ns = 0");
        assert!(parse_sos(&[5u8], 0, &frame).is_err(), "Ns = 5");
        assert!(
            parse_sos(&[1u8, 9, 0x00, 0, 63, 0], 0, &frame).is_err(),
            "unknown component"
        );
        assert!(
            parse_sos(&[2u8, 1, 0x00, 1, 0x00, 0, 63, 0], 0, &frame).is_err(),
            "duplicate component"
        );
        assert!(
            parse_sos(&[1u8, 1, 0x40, 0, 63, 0], 0, &frame).is_err(),
            "Td = 4"
        );
        assert!(parse_sos(&[1u8, 1, 0x00, 0], 0, &frame).is_err(), "short");
    }

    #[test]
    fn sequential_scans_must_cover_the_full_spectrum() {
        let frame = simple_frame();
        let scan = parse_sos(&[1u8, 1, 0x00, 1, 63, 0x00], 0, &frame).expect("SOS");
        assert!(validate_dct_scan(&scan, &frame, 0).is_err());
        let scan = parse_sos(&[1u8, 1, 0x00, 0, 63, 0x11], 0, &frame).expect("SOS");
        assert!(validate_dct_scan(&scan, &frame, 0).is_err());
    }

    #[test]
    fn progressive_scan_rules() {
        let payload = sof_payload(8, 16, 16, &[(1, 1, 1, 0), (2, 1, 1, 1)]);
        let frame = parse_sof(0xC2, &payload, 0, &DecodeLimits::default()).expect("SOF2");

        // DC first scan, interleaved: legal.
        let scan = parse_sos(&[2u8, 1, 0x00, 2, 0x00, 0, 0, 0x01], 0, &frame).expect("SOS");
        assert!(validate_dct_scan(&scan, &frame, 0).is_ok());

        // AC scan naming two components: illegal.
        let scan = parse_sos(&[2u8, 1, 0x00, 2, 0x00, 1, 63, 0x00], 0, &frame).expect("SOS");
        assert!(validate_dct_scan(&scan, &frame, 0).is_err());

        // Se = 64 is out of range (encoded as Se > 63).
        let scan = parse_sos(&[1u8, 1, 0x00, 1, 64, 0x00], 0, &frame).expect("SOS");
        assert!(validate_dct_scan(&scan, &frame, 0).is_err());

        // Ss > Se.
        let scan = parse_sos(&[1u8, 1, 0x00, 9, 5, 0x00], 0, &frame).expect("SOS");
        assert!(validate_dct_scan(&scan, &frame, 0).is_err());

        // Ah must be Al + 1.
        let scan = parse_sos(&[1u8, 1, 0x00, 1, 63, 0x30], 0, &frame).expect("SOS");
        assert!(validate_dct_scan(&scan, &frame, 0).is_err());
        let scan = parse_sos(&[1u8, 1, 0x00, 1, 63, 0x21], 0, &frame).expect("SOS");
        assert!(validate_dct_scan(&scan, &frame, 0).is_ok());
    }

    #[test]
    fn dac_parses_and_round_trips() {
        let mut dac = ArithmeticConditioning::default();
        assert_eq!(dac.dc, [0x10; 4], "T.81 default is L = 0, U = 1");
        assert_eq!(dac.ac, [5; 4]);

        dac.parse(&[0x00, 0x51, 0x11, 0x08], 0).expect("DAC");
        assert_eq!(dac.dc[0], 0x51);
        assert_eq!(dac.ac[1], 8);

        let mut emitted = Vec::new();
        dac.emit(&mut emitted);
        assert_eq!(&emitted[..4], &[0xFF, 0xCC, 0x00, 0x06]);

        let mut rebuilt = ArithmeticConditioning::default();
        rebuilt.parse(&emitted[4..], 0).expect("re-parse");
        assert_eq!(rebuilt, dac);
    }

    #[test]
    fn dac_rejects_malformed_payloads() {
        let mut dac = ArithmeticConditioning::default();
        assert!(dac.parse(&[0x00], 0).is_err(), "odd length");
        assert!(dac.parse(&[0x20, 0x00], 0).is_err(), "Tc = 2");
        assert!(dac.parse(&[0x04, 0x00], 0).is_err(), "Tb = 4");
        assert!(dac.parse(&[0x00, 0x01], 0).is_err(), "L > U");
        assert!(dac.parse(&[0x00, 0x10], 0).is_ok(), "L = 0, U = 1 is legal");
        assert!(dac.parse(&[0x10, 0x00], 0).is_err(), "Kx = 0");
        assert!(dac.parse(&[0x10, 64], 0).is_err(), "Kx = 64");
    }

    #[test]
    fn default_dac_emits_nothing() {
        let mut out = Vec::new();
        ArithmeticConditioning::default().emit(&mut out);
        assert!(out.is_empty());
    }
}
