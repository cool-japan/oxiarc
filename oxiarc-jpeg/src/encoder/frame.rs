//! Assembling a whole datastream: headers, tables, scans.
//!
//! Table selection lives here because it is the one decision shared by every
//! process. Standard Annex K.3 tables go into the slots the components name
//! (`dc[0]`/`ac[0]` luminance, `dc[1]`/`ac[1]` chrominance, exactly as
//! `jcparam.c`'s `std_huff_tables`); generated tables replace them when the
//! caller asked for optimisation, or when the frame forces it.

use super::bitwriter::BitWriter;
use super::coefficients::CoefficientPlane;
use super::enctable::{DerivedTable, Histogram};
use super::markers;
use super::plan::EncodePlan;
use super::sequential::{ScanTables, encode_scan, gather_scan};
use crate::error::Result;
use crate::huffman::HuffmanTable;
use crate::tables::{
    ANNEX_K_AC_CHROMA_BITS, ANNEX_K_AC_CHROMA_VALUES, ANNEX_K_AC_LUMA_BITS, ANNEX_K_AC_LUMA_VALUES,
    ANNEX_K_DC_CHROMA_BITS, ANNEX_K_DC_CHROMA_VALUES, ANNEX_K_DC_LUMA_BITS, ANNEX_K_DC_LUMA_VALUES,
};

/// The Huffman tables a frame writes, plus their encode-side derivations.
pub(crate) struct TableBank {
    pub dc: [Option<HuffmanTable>; 4],
    pub ac: [Option<HuffmanTable>; 4],
    pub dc_derived: [Option<DerivedTable>; 4],
    pub ac_derived: [Option<DerivedTable>; 4],
    /// Which slots have already been written into the datastream. libjpeg
    /// resets these whenever an optimisation pass regenerates a table, which
    /// is why a progressive frame can emit the same slot twice.
    pub dc_sent: [bool; 4],
    pub ac_sent: [bool; 4],
}

impl TableBank {
    /// Empty banks.
    pub(crate) fn new() -> Self {
        Self {
            dc: [None, None, None, None],
            ac: [None, None, None, None],
            dc_derived: [None, None, None, None],
            ac_derived: [None, None, None, None],
            dc_sent: [false; 4],
            ac_sent: [false; 4],
        }
    }

    /// Install Annex K.3's tables into the slots `plan`'s components use.
    pub(crate) fn install_standard(&mut self, plan: &EncodePlan) -> Result<()> {
        for component in &plan.components {
            let dc_slot = usize::from(component.dc_slot);
            let ac_slot = usize::from(component.ac_slot);
            if self.dc[dc_slot].is_none() {
                let (bits, values) = if dc_slot == 0 {
                    (ANNEX_K_DC_LUMA_BITS, &ANNEX_K_DC_LUMA_VALUES[..])
                } else {
                    (ANNEX_K_DC_CHROMA_BITS, &ANNEX_K_DC_CHROMA_VALUES[..])
                };
                self.set_dc(dc_slot, HuffmanTable::new(bits, values.to_vec())?);
            }
            if self.ac[ac_slot].is_none() {
                let (bits, values) = if ac_slot == 0 {
                    (ANNEX_K_AC_LUMA_BITS, &ANNEX_K_AC_LUMA_VALUES[..])
                } else {
                    (ANNEX_K_AC_CHROMA_BITS, &ANNEX_K_AC_CHROMA_VALUES[..])
                };
                self.set_ac(ac_slot, HuffmanTable::new(bits, values.to_vec())?);
            }
        }
        Ok(())
    }

    /// Install one DC table, replacing whatever was there and marking the
    /// slot unsent.
    pub(crate) fn set_dc(&mut self, slot: usize, table: HuffmanTable) {
        self.dc_derived[slot] = Some(DerivedTable::new(&table));
        self.dc[slot] = Some(table);
        self.dc_sent[slot] = false;
    }

    /// Install one AC table.
    pub(crate) fn set_ac(&mut self, slot: usize, table: HuffmanTable) {
        self.ac_derived[slot] = Some(DerivedTable::new(&table));
        self.ac[slot] = Some(table);
        self.ac_sent[slot] = false;
    }

    /// Emit a `DHT` for one slot unless it is already in the datastream.
    pub(crate) fn emit_dc(&mut self, slot: usize, out: &mut Vec<u8>) -> Result<()> {
        if self.dc_sent[slot] {
            return Ok(());
        }
        if let Some(table) = &self.dc[slot] {
            markers::dht(out, 0, slot as u8, table)?;
            self.dc_sent[slot] = true;
        }
        Ok(())
    }

    /// Emit an AC `DHT` for one slot unless it is already in the datastream.
    pub(crate) fn emit_ac(&mut self, slot: usize, out: &mut Vec<u8>) -> Result<()> {
        if self.ac_sent[slot] {
            return Ok(());
        }
        if let Some(table) = &self.ac[slot] {
            markers::dht(out, 1, slot as u8, table)?;
            self.ac_sent[slot] = true;
        }
        Ok(())
    }
}

/// The per-scan-component table slots for a scan over `components`.
pub(crate) fn scan_tables(plan: &EncodePlan, components: &[usize]) -> Vec<ScanTables> {
    components
        .iter()
        .map(|&index| ScanTables {
            dc: usize::from(plan.components[index].dc_slot),
            ac: usize::from(plan.components[index].ac_slot),
        })
        .collect()
}

/// Write a complete sequential frame's single scan.
///
/// Sequential JPEG puts every component in one interleaved scan when there
/// are four or fewer of them, which T.81 caps at, so there is exactly one
/// scan. A one-component frame's scan is non-interleaved, and its MCU is one
/// block.
pub(crate) fn write_sequential_scan(
    plan: &EncodePlan,
    coefficients: &[CoefficientPlane],
    bank: &mut TableBank,
    out: &mut Vec<u8>,
) -> Result<()> {
    let components: Vec<usize> = (0..plan.components.len()).collect();
    let tables = scan_tables(plan, &components);
    let restart = usize::from(plan.restart_for(&components));

    if plan.optimize_huffman {
        let mut dc_histograms = std::array::from_fn(|_| Histogram::default());
        let mut ac_histograms = std::array::from_fn(|_| Histogram::default());
        gather_scan(
            plan,
            coefficients,
            &components,
            &tables,
            restart,
            &mut dc_histograms,
            &mut ac_histograms,
        )?;
        for (slot, histogram) in dc_histograms.iter().enumerate() {
            if !histogram.is_empty() {
                bank.set_dc(slot, histogram.optimal_table()?);
            }
        }
        for (slot, histogram) in ac_histograms.iter().enumerate() {
            if !histogram.is_empty() {
                bank.set_ac(slot, histogram.optimal_table()?);
            }
        }
    }

    for &index in &components {
        let component = &plan.components[index];
        bank.emit_dc(usize::from(component.dc_slot), out)?;
        bank.emit_ac(usize::from(component.ac_slot), out)?;
    }
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
        &bank.dc_derived,
        &bank.ac_derived,
    ) {
        out.extend_from_slice(&bytes?);
        return Ok(());
    }

    let mut writer = BitWriter::with_capacity(1 << 16);
    encode_scan(
        plan,
        coefficients,
        &components,
        &tables,
        restart,
        &bank.dc_derived,
        &bank.ac_derived,
        &mut writer,
    )?;
    out.extend_from_slice(&writer.finish());
    Ok(())
}

/// Encode a complete DCT frame (`SOI` through `EOI`) into `out`.
///
/// `abbreviated` suppresses every table segment, which is what a TIFF strip
/// needs: the tables live in the container's `JPEGTables` tag instead.
pub(crate) fn encode_dct_frame(
    plan: &EncodePlan,
    pixels: &super::prepare::Samples<'_>,
    extra_markers: &[u8],
    abbreviated: bool,
    out: &mut Vec<u8>,
) -> Result<()> {
    let planes = super::prepare::build_dct_planes(plan, pixels)?;
    let coefficients = super::coefficients::build_coefficients(plan, &planes);

    let mut bank = TableBank::new();
    bank.install_standard(plan)?;

    markers::soi(out);
    if plan.write_jfif {
        markers::jfif(out, plan.density);
    }
    if let Some(transform) = plan.write_adobe {
        markers::adobe(out, transform);
    }
    out.extend_from_slice(extra_markers);
    if abbreviated {
        // The tables are already in the datastream as far as the decoder is
        // concerned, so mark every slot sent and skip the DQT segments.
        bank.dc_sent = [true; 4];
        bank.ac_sent = [true; 4];
    } else {
        markers::dqt(out, plan);
    }
    markers::sof(out, plan)?;

    match plan.process {
        super::options::EncodeProcess::Progressive => {
            super::progressive::write_scans(plan, &coefficients, &mut bank, out)?;
        }
        _ => write_sequential_scan(plan, &coefficients, &mut bank, out)?,
    }
    markers::eoi(out);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encoder::options::{EncodeOptions, InputColor};
    use crate::encoder::plan::build_plan;

    #[test]
    fn standard_tables_land_in_the_slots_components_name() {
        let plan = build_plan(&EncodeOptions::default(), 8, 8, InputColor::Rgb).expect("plan");
        let mut bank = TableBank::new();
        bank.install_standard(&plan).expect("tables");
        assert_eq!(
            bank.dc[0].as_ref().expect("dc0").bits(),
            &ANNEX_K_DC_LUMA_BITS
        );
        assert_eq!(
            bank.dc[1].as_ref().expect("dc1").bits(),
            &ANNEX_K_DC_CHROMA_BITS
        );
        assert!(bank.dc[2].is_none());
    }

    #[test]
    fn a_grey_frame_installs_only_slot_zero() {
        let plan = build_plan(&EncodeOptions::default(), 8, 8, InputColor::Luma).expect("plan");
        let mut bank = TableBank::new();
        bank.install_standard(&plan).expect("tables");
        assert!(bank.dc[0].is_some());
        assert!(bank.dc[1].is_none());
        assert!(bank.ac[1].is_none());
    }

    #[test]
    fn duplicate_dht_emission_is_suppressed_until_a_table_changes() {
        let plan = build_plan(&EncodeOptions::default(), 8, 8, InputColor::Rgb).expect("plan");
        let mut bank = TableBank::new();
        bank.install_standard(&plan).expect("tables");
        let mut out = Vec::new();
        bank.emit_dc(0, &mut out).expect("emit");
        let after_first = out.len();
        bank.emit_dc(0, &mut out).expect("emit");
        assert_eq!(out.len(), after_first, "second emission suppressed");

        let table = bank.dc[0].clone().expect("table");
        bank.set_dc(0, table);
        bank.emit_dc(0, &mut out).expect("emit");
        assert!(out.len() > after_first, "a regenerated table is re-sent");
    }

    /// The marker order libjpeg writes, measured from `cjpeg`: `SOI`,
    /// `APP0`, both `DQT` segments, then the `SOF`.
    #[test]
    fn headers_come_out_in_the_order_cjpeg_writes() {
        let plan = build_plan(&EncodeOptions::default(), 8, 8, InputColor::Rgb).expect("plan");
        let mut out = Vec::new();
        let pixels = vec![64u8; 8 * 8 * 3];
        encode_dct_frame(
            &plan,
            &crate::encoder::prepare::Samples::Eight(&pixels),
            &[],
            false,
            &mut out,
        )
        .expect("frame");
        assert_eq!(&out[..2], &[0xFF, 0xD8], "SOI");
        assert_eq!(&out[2..4], &[0xFF, 0xE0], "APP0");
        assert_eq!(&out[20..22], &[0xFF, 0xDB], "DQT");
        let second_dqt = 20 + 2 + 67;
        assert_eq!(&out[second_dqt..second_dqt + 2], &[0xFF, 0xDB]);
        let sof = second_dqt + 2 + 67;
        assert_eq!(&out[sof..sof + 2], &[0xFF, 0xC0], "SOF0");
        assert_eq!(&out[out.len() - 2..], &[0xFF, 0xD9], "EOI");
    }
}
