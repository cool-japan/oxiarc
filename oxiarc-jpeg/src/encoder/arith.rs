//! Arithmetic-coded scan encoding: `SOF9`, `SOF10` and `SOF11`.
//!
//! The decision sequences here are the exact inverses of
//! [`crate::decoder::arith`]'s, which is what makes the two testable against
//! each other; both follow T.81 Annexes F, G and H, and the DCT halves are
//! held to byte identity with `cjpeg -arithmetic`.
//!
//! Arithmetic coding needs no statistics pass: the coder adapts as it goes,
//! so a scan is written in one traversal. That is why these coders plug into
//! the *coding* traversal only and never into the gathering one.

use super::coefficients::CoefficientPlane;
use super::markers;
use super::options::{EncodeProcess, ScanSpec};
use super::plan::EncodePlan;
use super::sequential::{ScanSink, ScanTables, walk_scan};
use crate::arith::encoder::ArithEncoder;
use crate::arith::qm::Bin;
use crate::arith::{
    AC_BINS, AC_X1_HIGH, AC_X1_LOW, Category, Conditioning, DC_BINS, DC_X1, DctStats,
    LOSSLESS_BINS, LosslessStats, MAGNITUDE_OFFSET, lossless_context, lossless_x1,
};
use crate::error::Result;

/// Encode one DC difference (T.81 figures F.4 to F.9).
fn encode_dc_diff(
    encoder: &mut ArithEncoder,
    area: &mut [Bin; DC_BINS],
    context: &mut usize,
    conditioning: &Conditioning,
    slot: usize,
    difference: i32,
) {
    let base = *context;
    if difference == 0 {
        encoder.encode(&mut area[base], 0);
        *context = Category::Zero.dc_context();
        return;
    }
    encoder.encode(&mut area[base], 1);

    let negative = difference < 0;
    let magnitude = difference.unsigned_abs();
    let mut st = if negative {
        encoder.encode(&mut area[base + 1], 1);
        base + 3
    } else {
        encoder.encode(&mut area[base + 1], 0);
        base + 2
    };

    let value = magnitude - 1;
    let mut m = 0u32;
    if value != 0 {
        encoder.encode(&mut area[st], 1);
        m = 1;
        st = DC_X1;
        let mut rest = value;
        loop {
            rest >>= 1;
            if rest == 0 {
                break;
            }
            encoder.encode(&mut area[st], 1);
            m <<= 1;
            st += 1;
        }
    }
    encoder.encode(&mut area[st], 0);
    *context = conditioning.classify(slot, m, negative).dc_context();

    st += MAGNITUDE_OFFSET;
    let mut bit = m >> 1;
    while bit != 0 {
        encoder.encode(&mut area[st], u8::from(value & bit != 0));
        bit >>= 1;
    }
}

/// The magnitude tail shared by the DC and AC coders once the first category
/// decision has been made: the `X` chain, then the `M` bits.
fn encode_magnitude_tail(
    encoder: &mut ArithEncoder,
    area: &mut [Bin],
    mut st: usize,
    value: u32,
    mut m: u32,
) {
    debug_assert!(
        st + MAGNITUDE_OFFSET < area.len(),
        "the magnitude chain left its statistics area"
    );
    encoder.encode(&mut area[st], 0);
    st += MAGNITUDE_OFFSET;
    m >>= 1;
    while m != 0 {
        encoder.encode(&mut area[st], u8::from(value & m != 0));
        m >>= 1;
    }
}

/// Encode the AC coefficients of one block over `ss..=se` with the point
/// transform `al` (T.81 figures F.5 to F.9 and G.1.2.2).
fn encode_ac_band(
    encoder: &mut ArithEncoder,
    area: &mut [Bin; AC_BINS],
    conditioning: &Conditioning,
    slot: usize,
    band: (usize, usize),
    al: u8,
    block: &[i16; 64],
) {
    let (ss, se) = band;
    let kx = conditioning.kx(slot) as usize;
    let transformed = |k: usize| -> u32 { (i32::from(block[k]).unsigned_abs()) >> al };

    // Last index that survives the point transform; the band ends there.
    let mut ke = se;
    while ke > 0 && transformed(ke) == 0 {
        ke -= 1;
    }

    let mut k = ss;
    while k <= ke {
        let mut st = 3 * (k - 1);
        encoder.encode(&mut area[st], 0); // not the end of the band
        let magnitude = loop {
            let magnitude = transformed(k);
            if magnitude != 0 {
                encoder.encode(&mut area[st + 1], 1);
                encoder.encode_fixed(u8::from(block[k] < 0));
                break magnitude;
            }
            encoder.encode(&mut area[st + 1], 0);
            st += 3;
            k += 1;
        };
        st += 2;

        let value = magnitude - 1;
        let mut m = 0u32;
        if value != 0 {
            encoder.encode(&mut area[st], 1);
            m = 1;
            let mut rest = value >> 1;
            if rest != 0 {
                encoder.encode(&mut area[st], 1);
                m <<= 1;
                st = if k <= kx { AC_X1_LOW } else { AC_X1_HIGH };
                loop {
                    rest >>= 1;
                    if rest == 0 {
                        break;
                    }
                    encoder.encode(&mut area[st], 1);
                    m <<= 1;
                    st += 1;
                }
            }
        }
        encode_magnitude_tail(encoder, area.as_mut_slice(), st, value, m);
        k += 1;
    }
    if k <= se {
        encoder.encode(&mut area[3 * (k - 1)], 1); // end of band
    }
}

/// Encode one block of an AC refinement scan (T.81 figure G.10).
fn encode_ac_refine(
    encoder: &mut ArithEncoder,
    area: &mut [Bin; AC_BINS],
    band: (usize, usize),
    ah: u8,
    al: u8,
    block: &[i16; 64],
) {
    let (ss, se) = band;
    let magnitude = |k: usize, shift: u8| -> u32 { (i32::from(block[k]).unsigned_abs()) >> shift };

    let mut ke = se;
    while ke > 0 && magnitude(ke, al) == 0 {
        ke -= 1;
    }
    // The band this scan's predecessor already ended at: below it, the "end
    // of band" decision is not coded, because the decoder knows the band
    // cannot end there.
    let mut kex = ke;
    while kex > 0 && magnitude(kex, ah) == 0 {
        kex -= 1;
    }

    let mut k = ss;
    while k <= ke {
        let mut st = 3 * (k - 1);
        if k > kex {
            encoder.encode(&mut area[st], 0);
        }
        loop {
            let value = magnitude(k, al);
            if value != 0 {
                if value >> 1 != 0 {
                    // Already nonzero in an earlier scan: one correction bit.
                    encoder.encode(&mut area[st + 2], u8::from(value & 1 != 0));
                } else {
                    encoder.encode(&mut area[st + 1], 1);
                    encoder.encode_fixed(u8::from(block[k] < 0));
                }
                break;
            }
            encoder.encode(&mut area[st + 1], 0);
            st += 3;
            k += 1;
        }
        k += 1;
    }
    if k <= se {
        encoder.encode(&mut area[3 * (k - 1)], 1);
    }
}

/// Which statistics areas one scan resets at a restart marker.
#[derive(Clone, Copy)]
struct ResetPolicy {
    dc: bool,
    ac: bool,
}

/// The state one arithmetic DCT scan carries.
struct DctCoder<'a> {
    encoder: ArithEncoder,
    stats: DctStats,
    conditioning: Conditioning,
    tables: &'a [ScanTables],
    contexts: [usize; 4],
    reset: ResetPolicy,
}

impl<'a> DctCoder<'a> {
    fn new(plan: &EncodePlan, tables: &'a [ScanTables], reset: ResetPolicy) -> Self {
        Self {
            encoder: ArithEncoder::new(),
            stats: DctStats::new(),
            conditioning: Conditioning::new(&plan.arithmetic),
            tables,
            contexts: [0; 4],
            reset,
        }
    }

    /// T.81 D.1.7 and E.1.4: a restart terminates the code string, writes the
    /// marker and re-initialises every statistics area the scan uses.
    fn restart_marker(&mut self, index: u8) {
        self.encoder.restart(index);
        for (k, slots) in self.tables.iter().enumerate() {
            if self.reset.dc {
                self.stats.reset_dc(slots.dc);
                if k < self.contexts.len() {
                    self.contexts[k] = 0;
                }
            }
            if self.reset.ac {
                self.stats.reset_ac(slots.ac);
            }
        }
    }
}

/// The sequential scan sink: one DC difference and one AC band per block.
impl ScanSink for DctCoder<'_> {
    fn restart(&mut self, index: u8) -> Result<()> {
        self.restart_marker(index);
        Ok(())
    }

    fn block(&mut self, scan_index: usize, block: &[i16; 64], last_dc: i32) -> Result<()> {
        let slots = self.tables[scan_index];
        let difference = i32::from(block[0]) - last_dc;
        encode_dc_diff(
            &mut self.encoder,
            self.stats.dc(slots.dc),
            &mut self.contexts[scan_index],
            &self.conditioning,
            slots.dc,
            difference,
        );
        encode_ac_band(
            &mut self.encoder,
            self.stats.ac(slots.ac),
            &self.conditioning,
            slots.ac,
            (1, 63),
            0,
            block,
        );
        Ok(())
    }
}

/// Entropy-code one range of a sequential arithmetic scan into a fresh
/// buffer, for the parallel encoder.
///
/// `range.start` is a restart boundary, so the coder starts from the same
/// state the serial encoder would have been in: every statistics area at its
/// initial state, the registers re-initialised and the predictions zero.
#[cfg(feature = "rayon")]
pub(crate) fn encode_sequential_range(
    plan: &EncodePlan,
    coefficients: &[CoefficientPlane],
    components: &[usize],
    tables: &[ScanTables],
    restart_interval: usize,
    range: std::ops::Range<usize>,
) -> Result<Vec<u8>> {
    let mut coder = DctCoder::new(plan, tables, ResetPolicy { dc: true, ac: true });
    super::sequential::walk_scan_range(
        plan,
        coefficients,
        components,
        restart_interval,
        range,
        &mut coder,
    )?;
    Ok(coder.encoder.finish())
}

/// Write the single scan of a sequential arithmetic frame (`SOF9`).
pub(crate) fn write_sequential_scan(
    plan: &EncodePlan,
    coefficients: &[CoefficientPlane],
    out: &mut Vec<u8>,
) -> Result<()> {
    let components: Vec<usize> = (0..plan.components.len()).collect();
    let tables = super::frame::scan_tables(plan, &components);
    let restart = usize::from(plan.restart_for(&components));

    markers::dac(out, plan, &components, true, true);
    if restart > 0 {
        markers::dri(out, restart as u16);
    }
    markers::sos(out, plan, &components, 0, 63, 0, 0)?;

    #[cfg(feature = "rayon")]
    if let Some(bytes) = super::parallel::encode_sequential_scan_parallel(
        plan,
        coefficients,
        &components,
        &tables,
        restart,
        &[None, None, None, None],
        &[None, None, None, None],
    ) {
        out.extend_from_slice(&bytes?);
        return Ok(());
    }

    let mut coder = DctCoder::new(plan, &tables, ResetPolicy { dc: true, ac: true });
    walk_scan(plan, coefficients, &components, restart, &mut coder)?;
    let mut bytes = coder.encoder.finish();
    out.append(&mut bytes);
    Ok(())
}

/// Write every scan of a progressive arithmetic frame (`SOF10`).
pub(crate) fn write_progressive_scans(
    plan: &EncodePlan,
    coefficients: &[CoefficientPlane],
    out: &mut Vec<u8>,
) -> Result<()> {
    let script = match &plan.progressive_script {
        Some(script) => script.clone(),
        None => super::progressive::default_script(plan),
    };
    super::progressive::validate_script(plan, &script)?;

    let mut last_restart = 0u16;
    for scan in &script {
        let restart = plan.restart_for(&scan.components);
        let tables = super::frame::scan_tables(plan, &scan.components);
        let is_dc = scan.is_dc();
        let code_dc = is_dc && scan.approx_high == 0;
        let code_ac = !is_dc;

        markers::dac(out, plan, &scan.components, code_dc, code_ac);
        if restart != last_restart {
            markers::dri(out, restart);
            last_restart = restart;
        }
        markers::sos(
            out,
            plan,
            &scan.components,
            scan.spectral_start,
            scan.spectral_end,
            scan.approx_high,
            scan.approx_low,
        )?;

        let mut coder = DctCoder::new(
            plan,
            &tables,
            ResetPolicy {
                dc: code_dc,
                ac: code_ac,
            },
        );
        run_progressive_scan(&mut coder, plan, coefficients, scan, usize::from(restart));
        let mut bytes = coder.encoder.finish();
        out.append(&mut bytes);
    }
    Ok(())
}

/// Walk one progressive scan, coding every block it covers.
///
/// This is deliberately a separate traversal from the Huffman one: the
/// arithmetic coder has no end-of-band run to buffer and no correction bits
/// to queue, so sharing a walker would mean a trait whose interesting half is
/// unused on this side.
fn run_progressive_scan(
    coder: &mut DctCoder<'_>,
    plan: &EncodePlan,
    coefficients: &[CoefficientPlane],
    scan: &ScanSpec,
    restart_interval: usize,
) {
    let components = &scan.components;
    let mcus_per_row = plan.mcus_per_row_for(components);
    let mcu_rows = plan.mcu_rows_for(components);
    let interleaved = components.len() > 1;
    let is_dc = scan.is_dc();
    let band = (
        usize::from(scan.spectral_start),
        usize::from(scan.spectral_end),
    );
    let mut last_dc = [0i32; 4];
    let mut restart_index = 0u8;

    for mcu in 0..mcus_per_row * mcu_rows {
        if restart_interval > 0 && mcu > 0 && mcu % restart_interval == 0 {
            coder.restart_marker(restart_index);
            restart_index = (restart_index + 1) & 7;
            last_dc = [0i32; 4];
        }
        let mcu_x = mcu % mcus_per_row;
        let mcu_y = mcu / mcus_per_row;
        for (scan_index, &component_index) in components.iter().enumerate() {
            let component = &plan.components[component_index];
            let plane = &coefficients[component_index];
            let (h, v) = if interleaved {
                (usize::from(component.h), usize::from(component.v))
            } else {
                (1, 1)
            };
            let slots = coder.tables[scan_index];
            for by in 0..v {
                for bx in 0..h {
                    let block_x = if interleaved { mcu_x * h + bx } else { mcu_x };
                    let block_y = if interleaved { mcu_y * v + by } else { mcu_y };
                    let block = plane.block(block_x, block_y);
                    if is_dc {
                        if scan.approx_high == 0 {
                            // The DC point transform is an arithmetic shift.
                            let value = i32::from(block[0]) >> scan.approx_low;
                            encode_dc_diff(
                                &mut coder.encoder,
                                coder.stats.dc(slots.dc),
                                &mut coder.contexts[scan_index],
                                &coder.conditioning,
                                slots.dc,
                                value - last_dc[scan_index],
                            );
                            last_dc[scan_index] = value;
                        } else {
                            let bit = (i32::from(block[0]) >> scan.approx_low) & 1;
                            coder.encoder.encode_fixed(bit as u8);
                        }
                    } else if scan.approx_high == 0 {
                        encode_ac_band(
                            &mut coder.encoder,
                            coder.stats.ac(slots.ac),
                            &coder.conditioning,
                            slots.ac,
                            band,
                            scan.approx_low,
                            block,
                        );
                    } else {
                        encode_ac_refine(
                            &mut coder.encoder,
                            coder.stats.ac(slots.ac),
                            band,
                            scan.approx_high,
                            scan.approx_low,
                            block,
                        );
                    }
                }
            }
        }
    }
}

/// Encode a complete arithmetic DCT frame (`SOI` through `EOI`).
pub(crate) fn encode_dct_frame(
    plan: &EncodePlan,
    pixels: &super::prepare::Samples<'_>,
    extra_markers: &[u8],
    abbreviated: bool,
    out: &mut Vec<u8>,
) -> Result<()> {
    let planes = super::prepare::build_dct_planes(plan, pixels)?;
    let coefficients = super::coefficients::build_coefficients(plan, &planes);

    markers::soi(out);
    if plan.write_jfif {
        markers::jfif(out, plan.density);
    }
    if let Some(transform) = plan.write_adobe {
        markers::adobe(out, transform);
    }
    out.extend_from_slice(extra_markers);
    if !abbreviated {
        markers::dqt(out, plan);
    }
    markers::sof(out, plan)?;

    match plan.process {
        EncodeProcess::Progressive => write_progressive_scans(plan, &coefficients, out)?,
        _ => write_sequential_scan(plan, &coefficients, out)?,
    }
    markers::eoi(out);
    Ok(())
}

/// Encode one lossless difference (T.81 H.1.2.3 and Table H.3).
///
/// Returns the conditioning category of the difference, which becomes the
/// `Da` of the next sample and the `Db` of the sample below.
#[allow(clippy::too_many_arguments)]
pub(crate) fn encode_lossless_diff(
    encoder: &mut ArithEncoder,
    area: &mut [Bin; LOSSLESS_BINS],
    conditioning: &Conditioning,
    slot: usize,
    left: Category,
    above: Category,
    difference: i32,
) -> Category {
    // T.81 H.1.2.1 codes the difference **modulo 2^16**, which bounds the
    // magnitude at 32768 and so the magnitude-category chain at `X15`.
    // Without this reduction a sixteen-bit sample pair can produce a
    // difference of 65535, whose chain would run off the end of the
    // statistics area — and would be undecodable in any case.
    let difference = (((difference + 32_768) & 0xFFFF) - 32_768).max(-32_768);
    let base = lossless_context(left, above);
    if difference == 0 {
        encoder.encode(&mut area[base], 0);
        return Category::Zero;
    }
    encoder.encode(&mut area[base], 1);

    let negative = difference < 0;
    let magnitude = difference.unsigned_abs();
    let mut st = if negative {
        encoder.encode(&mut area[base + 1], 1);
        base + 3
    } else {
        encoder.encode(&mut area[base + 1], 0);
        base + 2
    };

    let value = magnitude - 1;
    let mut m = 0u32;
    if value != 0 {
        encoder.encode(&mut area[st], 1);
        m = 1;
        st = lossless_x1(above);
        let mut rest = value;
        loop {
            rest >>= 1;
            if rest == 0 {
                break;
            }
            encoder.encode(&mut area[st], 1);
            m <<= 1;
            st += 1;
        }
    }
    let category = conditioning.classify(slot, m, negative);
    encode_magnitude_tail(encoder, area.as_mut_slice(), st, value, m);
    category
}

/// The state a lossless arithmetic scan carries: one statistics area per
/// table slot, plus the neighbouring categories of H.1.2.3.1.
pub(crate) struct LosslessCoder {
    pub(crate) encoder: ArithEncoder,
    stats: LosslessStats,
    conditioning: Conditioning,
    slots: Vec<usize>,
    above: Vec<Vec<Category>>,
    left: Vec<Vec<Category>>,
}

impl LosslessCoder {
    /// Room for every component of the scan.
    ///
    /// `widths` is the number of sample columns each component's conditioning
    /// has to cover, which is the MCU-padded width when the scan interleaves
    /// (padding samples are coded, so they condition their neighbours too).
    pub(crate) fn new(
        plan: &EncodePlan,
        components: &[usize],
        widths: &[usize],
        interleaved: bool,
    ) -> Self {
        let mut slots = Vec::with_capacity(components.len());
        let mut above = Vec::with_capacity(components.len());
        let mut left = Vec::with_capacity(components.len());
        for (k, &index) in components.iter().enumerate() {
            let component = &plan.components[index];
            slots.push(usize::from(component.dc_slot) & 3);
            let width = widths.get(k).copied().unwrap_or(1).max(1);
            above.push(vec![Category::Zero; width + 1]);
            left.push(vec![
                Category::Zero;
                if interleaved {
                    usize::from(component.v)
                } else {
                    1
                }
            ]);
        }
        Self {
            encoder: ArithEncoder::new(),
            stats: LosslessStats::new(),
            conditioning: Conditioning::new(&plan.arithmetic),
            slots,
            above,
            left,
        }
    }

    /// Code one sample's difference at `(x, y)`, `row_in_band` rows into the
    /// current MCU band of component `scan_index`.
    pub(crate) fn sample(&mut self, scan_index: usize, difference: i32, x: usize, row: usize) {
        let width = self.above[scan_index].len();
        let column = x.min(width - 1);
        if x == 0 {
            self.left[scan_index][row] = Category::Zero;
        }
        let left = self.left[scan_index][row];
        let above = self.above[scan_index][column];
        let slot = self.slots[scan_index];
        let category = encode_lossless_diff(
            &mut self.encoder,
            self.stats.area(slot),
            &self.conditioning,
            slot,
            left,
            above,
            difference,
        );
        self.left[scan_index][row] = category;
        self.above[scan_index][column] = category;
    }

    /// Terminate the interval, write `RSTn` and reset every model.
    pub(crate) fn restart_marker(&mut self, index: u8) {
        self.encoder.restart(index);
        for &slot in &self.slots {
            self.stats.reset(slot);
        }
        for row in &mut self.above {
            row.fill(Category::Zero);
        }
        for row in &mut self.left {
            row.fill(Category::Zero);
        }
    }
}
