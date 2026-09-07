//! Parallel entropy decoding across restart intervals (`rayon` feature).
//!
//! A restart marker is a hard synchronisation point: T.81 E.2.4 resets the
//! predictions, the bit (or arithmetic) accumulator and — for arithmetic —
//! every statistics area, so the data between two markers can be decoded
//! knowing nothing about what came before it. That makes restart intervals
//! the one place a JPEG scan is embarrassingly parallel.
//!
//! # What is parallelised, and what is not
//!
//! Only a **sequential** scan (`SOF0`, `SOF1`, `SOF9`) of a frame that
//! carries restart markers, and only when the interval tiles whole MCU rows
//! — otherwise two bands would write into the same sample row and the merge
//! would need locking for no gain. Progressive and lossless scans stay
//! serial: a progressive frame revisits every block in later scans, and
//! lossless prediction crosses restart intervals row by row.
//!
//! The output is **identical** either way. `tests/parallel.rs` asserts that
//! two ways: by running both paths over the same input, and — because two
//! runs in one process share a configuration — against digests of the encoded
//! bytes *and* of the decoded samples that were measured in a build without
//! this feature and are asserted in builds with and without it.

use super::planes::Planes;
use super::scan::{ScanOutcome, ScanTables};
use crate::error::Result;
use crate::frame::{EntropyCoding, FrameHeader, ScanHeader};
use crate::limits::DecodeLimits;
use rayon::prelude::*;

/// Smallest scan worth splitting: below this, thread hand-off costs more than
/// the decode.
const MINIMUM_UNITS: u64 = 256;

/// One band of whole MCU rows, and the entropy bytes that code it.
struct Band<'a> {
    /// Pixel row the band starts at, per component.
    first_rows: Vec<usize>,
    /// Frame header describing this band as if it were a whole image.
    frame: FrameHeader,
    /// Entropy bytes from the band's first byte to the end of the scan.
    entropy: &'a [u8],
    /// Offset of `entropy` inside the scan's entropy data.
    offset: usize,
}

/// Greatest common divisor, for the band size.
fn gcd(a: u64, b: u64) -> u64 {
    if b == 0 { a } else { gcd(b, a % b) }
}

/// Offsets of the restart markers in `entropy`, in order.
///
/// Stops at the first non-restart marker, which ends the scan. Returns the
/// offset **after** each marker, i.e. where the next interval's data starts.
fn restart_offsets(entropy: &[u8]) -> Vec<usize> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    while pos + 1 < entropy.len() {
        if entropy[pos] != 0xFF {
            pos += 1;
            continue;
        }
        let mut probe = pos + 1;
        while entropy.get(probe) == Some(&0xFF) {
            probe += 1;
        }
        match entropy.get(probe) {
            None => break,
            Some(0) => pos = probe + 1,
            Some(&code) if (0xD0..=0xD7).contains(&code) => {
                out.push(probe + 1);
                pos = probe + 1;
            }
            Some(_) => break,
        }
    }
    out
}

/// Offset just past the `RSTn` that starts at `at`, or `None` when the bytes
/// there are not a restart marker.
///
/// Fill bytes (`0xFF` runs) are part of the marker, which is why this is not
/// simply `at + 2`.
fn restart_marker_end(entropy: &[u8], at: usize) -> Option<usize> {
    if entropy.get(at) != Some(&0xFF) {
        return None;
    }
    let mut probe = at + 1;
    while entropy.get(probe) == Some(&0xFF) {
        probe += 1;
    }
    match entropy.get(probe) {
        Some(&code) if (0xD0..=0xD7).contains(&code) => Some(probe + 1),
        _ => None,
    }
}

/// Split a sequential scan into independently decodable bands.
///
/// Returns `None` when the shape rules parallelism out, in which case the
/// caller decodes serially.
fn plan_bands<'a>(
    frame: &FrameHeader,
    scan: &ScanHeader,
    restart_interval: u16,
    entropy: &'a [u8],
) -> Option<Vec<Band<'a>>> {
    if restart_interval == 0 {
        return None;
    }
    // Every component must be in the scan, or the band frames would not
    // describe the same image.
    if scan.component_indices.len() != frame.components.len() {
        return None;
    }
    let interval = u64::from(restart_interval);
    let units_per_row = u64::from(frame.mcus_per_line);
    let total_rows = u64::from(frame.mcus_per_column);
    if units_per_row == 0 || total_rows < 2 {
        return None;
    }
    if units_per_row.saturating_mul(total_rows) < MINIMUM_UNITS {
        return None;
    }

    // The band has to be a whole number of MCU rows *and* of restart
    // intervals, i.e. lcm(interval, units_per_row) units.
    let lcm = interval / gcd(interval, units_per_row) * units_per_row;
    let smallest_band = lcm / units_per_row;
    if smallest_band == 0 || smallest_band >= total_rows {
        return None;
    }
    // The smallest legal band is often one MCU row, which would mean one
    // plane allocation and one merge per row. Grow it until there are about
    // as many bands as threads: each band then carries real work, and the
    // bands stay equal-sized so nothing has to be stolen.
    let target = rayon::current_num_threads().max(2) as u64;
    let smallest_count = total_rows.div_ceil(smallest_band);
    let group = (smallest_count / target).max(1);
    let rows_per_band = smallest_band.saturating_mul(group);
    let intervals_per_band = lcm / interval * group;
    let band_count = total_rows.div_ceil(rows_per_band);
    if band_count < 2 {
        return None;
    }

    let markers = restart_offsets(entropy);
    // A conforming scan carries exactly one marker per interval boundary, and
    // nothing else: the count has to match to the marker. Tolerating one
    // extra — a trailing marker some encoders write — is not safe here,
    // because a *stray* marker in the middle has the same count while
    // shifting every band after it into the middle of an interval, which
    // decoded to different samples and reported a different scan length than
    // the same code built without `rayon`. A stream that does carry a
    // trailing marker still decodes, serially. The serial path handles and
    // reports every one of these cases; the parallel path simply declines.
    let total_units = units_per_row.saturating_mul(total_rows);
    let expected = total_units.div_ceil(interval).saturating_sub(1);
    if (markers.len() as u64) != expected {
        return None;
    }
    let needed = (band_count - 1) * intervals_per_band;
    if (markers.len() as u64) < needed {
        return None;
    }

    let vmax = u64::from(frame.vmax);
    let mut bands = Vec::with_capacity(band_count as usize);
    for index in 0..band_count {
        let first_row = index * rows_per_band;
        let pixel_start = first_row * vmax * 8;
        if pixel_start >= u64::from(frame.height) {
            break;
        }
        let remaining = u64::from(frame.height) - pixel_start;
        let height = remaining.min(rows_per_band * vmax * 8);
        let mut band_frame = frame.clone();
        band_frame.set_height(u16::try_from(height).ok()?);

        let offset = if index == 0 {
            0
        } else {
            markers[((index * intervals_per_band) - 1) as usize]
        };
        let first_rows = frame
            .components
            .iter()
            .map(|component| (first_row * u64::from(component.v) * 8) as usize)
            .collect();
        bands.push(Band {
            first_rows,
            frame: band_frame,
            entropy: &entropy[offset.min(entropy.len())..],
            offset: offset.min(entropy.len()),
        });
    }
    if bands.len() < 2 {
        return None;
    }
    Some(bands)
}

/// Decode a sequential scan across restart intervals in parallel.
///
/// Returns `None` when the scan cannot be split, leaving the caller to decode
/// it serially.
#[allow(clippy::too_many_arguments)]
pub(crate) fn decode_sequential_parallel(
    frame: &FrameHeader,
    scan: &ScanHeader,
    tables: &ScanTables<'_>,
    dac: &crate::frame::ArithmeticConditioning,
    restart_interval: u16,
    entropy: &[u8],
    planes: &mut Planes,
    limits: &DecodeLimits,
    tolerate_truncated: bool,
) -> Option<Result<ScanOutcome>> {
    let bands = plan_bands(frame, scan, restart_interval, entropy)?;

    // Each band decodes into planes of its own, which is what keeps this free
    // of shared mutable state — and so of `unsafe`.
    let results: Vec<Result<(Planes, ScanOutcome)>> = bands
        .par_iter()
        .map(|band| {
            let mut band_planes = Planes::allocate(&band.frame, limits)?;
            let outcome = decode_band(
                &band.frame,
                scan,
                tables,
                dac,
                restart_interval,
                band.entropy,
                &mut band_planes,
                tolerate_truncated,
            )?;
            Ok((band_planes, outcome))
        })
        .collect();

    let mut truncated = false;
    let mut consumed = 0usize;
    let mut decoded = Vec::with_capacity(results.len());
    for (band, result) in bands.iter().zip(results) {
        match result {
            Ok((band_planes, outcome)) => {
                truncated |= outcome.truncated;
                consumed = consumed.max(band.offset + outcome.consumed);
                decoded.push((band, band_planes, outcome));
            }
            // A band that fails may simply have started in the wrong place —
            // the alignment check below is what proves otherwise, and it has
            // not run yet. Hand the scan back to the serial path so that the
            // error a caller sees is the one the same library reports without
            // this feature, raised at the same point in the stream.
            Err(_) => return None,
        }
    }

    // Every band but the last must end exactly on the restart marker that
    // starts the next one. Counting the markers is not enough: a stream that
    // carries one the `DRI` does not account for has the right *number* of
    // them (a trailing marker is legal, so the count check has to tolerate
    // one extra) while every band after the stray one starts in the middle of
    // an interval. Decoding it here would then produce different samples, a
    // different scan length and a different error from the same library built
    // without this feature, which is exactly what the feature promises not to
    // do. Anything that does not line up goes back to the serial path, which
    // owns the diagnosis.
    for (index, (band, _, outcome)) in decoded.iter().enumerate() {
        let Some(next) = decoded.get(index + 1) else {
            break;
        };
        let end = band.offset + outcome.consumed;
        if restart_marker_end(entropy, end) != Some(next.0.offset) {
            return None;
        }
    }

    for (band, band_planes, _) in decoded {
        planes.copy_band_from(&band_planes, &band.first_rows);
    }
    Some(Ok(ScanOutcome {
        consumed,
        truncated,
    }))
}

/// Decode one band with whichever entropy coder the frame declares.
///
/// `dac` is only read by the arithmetic arm, which the `arithmetic` feature
/// compiles out; without it an arithmetic frame is refused before any scan is
/// reached, so the parameter is simply unused.
#[allow(clippy::too_many_arguments)]
#[cfg_attr(not(feature = "arithmetic"), allow(unused_variables))]
fn decode_band(
    frame: &FrameHeader,
    scan: &ScanHeader,
    tables: &ScanTables<'_>,
    dac: &crate::frame::ArithmeticConditioning,
    restart_interval: u16,
    entropy: &[u8],
    planes: &mut Planes,
    tolerate_truncated: bool,
) -> Result<ScanOutcome> {
    match frame.entropy {
        #[cfg(feature = "arithmetic")]
        EntropyCoding::Arithmetic => super::arith::decode_sequential_arith(
            frame,
            scan,
            tables,
            dac,
            restart_interval,
            entropy,
            planes,
            tolerate_truncated,
        ),
        #[cfg(not(feature = "arithmetic"))]
        EntropyCoding::Arithmetic => Err(crate::error::JpegError::Unsupported(
            crate::error::UnsupportedFeature::ArithmeticCoding,
        )),
        EntropyCoding::Huffman => super::scan::decode_sequential(
            frame,
            scan,
            tables,
            restart_interval,
            entropy,
            planes,
            tolerate_truncated,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restart_offsets_skip_stuffing_and_stop_at_the_terminator() {
        let data = [
            0x01u8, 0xFF, 0x00, 0x02, 0xFF, 0xD0, 0x03, 0xFF, 0xD1, 0x04, 0xFF, 0xD9, 0xFF, 0xD2,
        ];
        assert_eq!(restart_offsets(&data), vec![6, 9]);
    }

    #[test]
    fn the_band_size_is_the_least_common_multiple_of_row_and_interval() {
        assert_eq!(gcd(12, 8), 4);
        assert_eq!(gcd(5, 16), 1);
        assert_eq!(gcd(9, 0), 9);
    }
}

/// A same-process differential between the parallel and serial entropy
/// decoders.
///
/// Every other gate on this feature compares the *whole* decode against a
/// reference measured in another configuration, which can only fail when a
/// band boundary changes the samples. These tests call both decoders on the
/// same scan inside one process, so a divergence is attributed to the split
/// itself, and they also assert that the split actually engaged — a planner
/// that quietly declines would otherwise pass every comparison in the file.
#[cfg(test)]
mod differential {
    use super::*;
    use crate::decoder::scan::decode_sequential;
    use crate::encoder::{
        EncodeOptions, InputColor, RestartInterval, Subsampling, encode_to_vec_with_options,
    };
    use crate::frame::{parse_sof, parse_sos};
    use crate::huffman::parse_dht;
    use crate::limits::DecodeLimits;
    use crate::parser::Scanner;
    use crate::quant::parse_dqt;
    use crate::tableset::TableSet;

    /// A deterministic source image with detail at every scale.
    fn pixels(width: usize, height: usize, channels: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(width * height * channels);
        for y in 0..height {
            for x in 0..width {
                let values = [
                    ((x * 7 + y * 13) % 256) as u8,
                    if (x / 5 + y / 3) % 2 == 0 { 240 } else { 24 },
                    ((x * x + y * 3) % 256) as u8,
                ];
                for c in 0..channels {
                    out.push(values[c % 3]);
                }
            }
        }
        out
    }

    /// The frame, scan, tables and entropy offset of a single-scan stream.
    fn dissect(jpeg: &[u8]) -> (FrameHeader, ScanHeader, TableSet, usize) {
        let mut tables = TableSet::default();
        let mut frame: Option<FrameHeader> = None;
        let mut scanner = Scanner::new(jpeg);
        while let Some(segment) = scanner.next_segment().expect("segment") {
            match segment.code {
                0xDB => parse_dqt(segment.payload, segment.offset, &mut tables.quant).expect("DQT"),
                0xC4 => parse_dht(
                    segment.payload,
                    segment.offset,
                    &mut tables.dc_huffman,
                    &mut tables.ac_huffman,
                )
                .expect("DHT"),
                0xCC => tables
                    .arithmetic
                    .parse(segment.payload, segment.offset)
                    .expect("DAC"),
                0xDD => {
                    tables.restart_interval =
                        Some(u16::from_be_bytes([segment.payload[0], segment.payload[1]]));
                }
                0xDA => {
                    let frame = frame.expect("a SOF before the SOS");
                    let scan = parse_sos(segment.payload, segment.offset, &frame).expect("SOS");
                    return (frame, scan, tables, scanner.position());
                }
                code if (0xC0..=0xCF).contains(&code) => {
                    frame = Some(
                        parse_sof(
                            code,
                            segment.payload,
                            segment.offset,
                            &DecodeLimits::default(),
                        )
                        .expect("SOF"),
                    );
                }
                _ => {}
            }
        }
        panic!("the stream has no scan");
    }

    /// Every component plane, unpadded rows included, as one comparable value.
    fn snapshot(planes: &Planes, components: usize) -> Vec<Vec<u16>> {
        (0..components).map(|i| planes.plane(i).to_vec()).collect()
    }

    /// Decode one scan serially, whichever entropy coder it uses.
    #[allow(clippy::too_many_arguments)]
    fn decode_serial(
        frame: &FrameHeader,
        scan: &ScanHeader,
        tables: &ScanTables<'_>,
        dac: &crate::frame::ArithmeticConditioning,
        restart_interval: u16,
        entropy: &[u8],
        planes: &mut Planes,
        tolerate: bool,
    ) -> Result<ScanOutcome> {
        match frame.entropy {
            #[cfg(feature = "arithmetic")]
            EntropyCoding::Arithmetic => crate::decoder::arith::decode_sequential_arith(
                frame,
                scan,
                tables,
                dac,
                restart_interval,
                entropy,
                planes,
                tolerate,
            ),
            #[cfg(not(feature = "arithmetic"))]
            EntropyCoding::Arithmetic => {
                let _ = dac;
                unreachable!("no arithmetic fixture is built without the feature")
            }
            EntropyCoding::Huffman => decode_sequential(
                frame,
                scan,
                tables,
                restart_interval,
                entropy,
                planes,
                tolerate,
            ),
        }
    }

    /// Encode one image, then decode its scan both ways and compare.
    ///
    /// Returns `true` when the parallel planner accepted the scan, so the
    /// caller can assert that a case meant to split really did.
    fn agrees(
        width: u16,
        height: u16,
        channels: usize,
        options: &EncodeOptions,
        color: InputColor,
    ) -> bool {
        let source = pixels(usize::from(width), usize::from(height), channels);
        let jpeg = encode_to_vec_with_options(&source, width, height, color, options)
            .expect("encode the fixture");
        let (frame, scan, tables, start) = dissect(&jpeg);
        let entropy = &jpeg[start..];
        let restart_interval = tables.restart_interval.unwrap_or(0);
        let scan_tables = ScanTables {
            dc: &tables.dc_huffman,
            ac: &tables.ac_huffman,
            quant: &tables.quant,
        };
        let limits = DecodeLimits::default();

        let mut serial_planes = Planes::allocate(&frame, &limits).expect("planes");
        let serial = decode_serial(
            &frame,
            &scan,
            &scan_tables,
            &tables.arithmetic,
            restart_interval,
            entropy,
            &mut serial_planes,
            false,
        )
        .expect("serial decode");

        let mut parallel_planes = Planes::allocate(&frame, &limits).expect("planes");
        let outcome = decode_sequential_parallel(
            &frame,
            &scan,
            &scan_tables,
            &tables.arithmetic,
            restart_interval,
            entropy,
            &mut parallel_planes,
            &limits,
            false,
        );
        let Some(parallel) = outcome else {
            // The planner declined; the serial path is what runs, and there is
            // nothing to compare. Report it so a caller can insist otherwise.
            return false;
        };
        let parallel = parallel.expect("parallel decode");

        let components = frame.components.len();
        assert_eq!(
            snapshot(&parallel_planes, components),
            snapshot(&serial_planes, components),
            "{width}x{height} {:?} {:?} interval {restart_interval} decoded differently in \
             parallel",
            options.subsampling,
            frame.entropy,
        );
        assert_eq!(
            parallel.consumed, serial.consumed,
            "{width}x{height} {:?} interval {restart_interval}: the parallel path reported a \
             different scan length",
            options.subsampling,
        );
        assert_eq!(parallel.truncated, serial.truncated);
        true
    }

    /// The matrix that matters: geometries whose last MCU row is partial, and
    /// restart intervals that do and do not tile a row.
    #[test]
    fn the_parallel_decoder_agrees_with_the_serial_one() {
        let entropies: &[EntropyCoding] = &[
            EntropyCoding::Huffman,
            #[cfg(feature = "arithmetic")]
            EntropyCoding::Arithmetic,
        ];
        let sizes: &[(u16, u16)] = &[
            (320, 256), // whole MCU rows either way
            (320, 250), // partial last MCU row at 4:2:0 and 4:4:4
            (314, 250), // partial last MCU column as well
            (129, 257), // odd both ways, one MCU column of padding
        ];
        for &entropy in entropies {
            for &subsampling in &[Subsampling::S444, Subsampling::S422, Subsampling::S420] {
                // Counted per coder *and* per sampling ratio, never in
                // aggregate: an aggregate count is satisfied by the Huffman
                // cases alone, and would not notice the arithmetic path
                // quietly running serially everywhere — which is exactly the
                // failure this file exists to catch.
                let mut split_count = 0usize;
                for &(width, height) in sizes {
                    for &interval in &[
                        RestartInterval::McuRows(1),
                        RestartInterval::McuRows(2),
                        RestartInterval::Mcus(5),
                        RestartInterval::Mcus(16),
                    ] {
                        let options = EncodeOptions {
                            quality: 80,
                            subsampling,
                            entropy,
                            restart_interval: interval,
                            ..Default::default()
                        };
                        if agrees(width, height, 3, &options, InputColor::Rgb) {
                            split_count += 1;
                        }
                    }
                }
                assert!(
                    split_count >= 8,
                    "{entropy:?} {subsampling:?} split only {split_count} of 16 cases: the \
                     differential would be comparing nothing"
                );
            }
        }
    }

    /// A one-component frame takes the non-interleaved geometry, whose units
    /// are blocks rather than MCUs.
    #[test]
    fn grayscale_scans_agree_as_well() {
        let entropies: &[EntropyCoding] = &[
            EntropyCoding::Huffman,
            #[cfg(feature = "arithmetic")]
            EntropyCoding::Arithmetic,
        ];
        for &entropy in entropies {
            let mut split = false;
            for &(width, height) in &[(320u16, 256u16), (313, 250)] {
                let options = EncodeOptions {
                    quality: 80,
                    entropy,
                    restart_interval: RestartInterval::McuRows(1),
                    ..Default::default()
                };
                split |= agrees(width, height, 1, &options, InputColor::Luma);
            }
            assert!(split, "no grayscale case split for {entropy:?}");
        }
    }

    /// The planner must decline anything it cannot prove independent, and the
    /// caller then decodes serially.
    #[test]
    fn the_planner_declines_what_it_cannot_split() {
        let source = pixels(320, 256, 3);
        // No restart markers at all.
        let jpeg = encode_to_vec_with_options(
            &source,
            320,
            256,
            InputColor::Rgb,
            &EncodeOptions {
                quality: 80,
                restart_interval: RestartInterval::None,
                ..Default::default()
            },
        )
        .expect("encode");
        let (frame, scan, _tables, start) = dissect(&jpeg);
        assert!(plan_bands(&frame, &scan, 0, &jpeg[start..]).is_none());

        // A tiny image, below the threshold.
        let small = pixels(32, 32, 3);
        let jpeg = encode_to_vec_with_options(
            &small,
            32,
            32,
            InputColor::Rgb,
            &EncodeOptions {
                quality: 80,
                restart_interval: RestartInterval::Mcus(1),
                ..Default::default()
            },
        )
        .expect("encode");
        let (frame, scan, tables, start) = dissect(&jpeg);
        let interval = tables.restart_interval.unwrap_or(0);
        assert!(plan_bands(&frame, &scan, interval, &jpeg[start..]).is_none());
    }

    /// Whatever the markers do, the two paths must agree.
    ///
    /// A stray marker is not caught by counting: one *trailing* marker is
    /// legal, so the count check tolerates an extra one, and a stray marker in
    /// the middle then passes it while shifting every band after it into the
    /// middle of an interval. Before the alignment check added alongside this
    /// test, a stream with one extra marker decoded to different samples and
    /// reported a different scan length than the same code built without
    /// `rayon` (measured: 52428 bytes consumed against 13988, planes
    /// differing). The invariant is output equality, not a particular
    /// decision: declining is one way to keep it, agreeing is another.
    #[test]
    fn a_stray_restart_marker_cannot_change_what_the_scan_decodes_to() {
        let source = pixels(320, 256, 3);
        let jpeg = encode_to_vec_with_options(
            &source,
            320,
            256,
            InputColor::Rgb,
            &EncodeOptions {
                quality: 80,
                restart_interval: RestartInterval::McuRows(1),
                ..Default::default()
            },
        )
        .expect("encode");
        let (frame, scan, tables, start) = dissect(&jpeg);
        let interval = tables.restart_interval.unwrap_or(0);
        let entropy = &jpeg[start..];
        let markers = restart_offsets(entropy);
        assert!(markers.len() > 6, "the fixture needs several intervals");

        assert!(
            compare_paths(&frame, &scan, &tables, interval, entropy, false),
            "the clean stream must still split"
        );

        for &at in &[markers[0], markers[1] + 1, markers[2] + 9, markers[4] + 3] {
            let mut spoiled = entropy.to_vec();
            spoiled.splice(at..at, [0xFFu8, 0xD2]);
            for tolerate in [false, true] {
                compare_paths(&frame, &scan, &tables, interval, &spoiled, tolerate);
            }
        }
    }

    /// The same for a scan that simply stops: whichever band notices first,
    /// the caller must see what the serial decoder would have said.
    #[test]
    fn a_truncated_scan_reports_the_same_thing_either_way() {
        let source = pixels(320, 256, 3);
        let jpeg = encode_to_vec_with_options(
            &source,
            320,
            256,
            InputColor::Rgb,
            &EncodeOptions {
                quality: 80,
                restart_interval: RestartInterval::McuRows(1),
                ..Default::default()
            },
        )
        .expect("encode");
        let (frame, scan, tables, start) = dissect(&jpeg);
        let interval = tables.restart_interval.unwrap_or(0);
        let entropy = &jpeg[start..];
        for numerator in [1usize, 2, 3, 5, 7] {
            let cut = entropy.len() * numerator / 8;
            for tolerate in [false, true] {
                compare_paths(&frame, &scan, &tables, interval, &entropy[..cut], tolerate);
            }
        }
    }

    /// Run both paths over the same bytes and insist they agree.
    ///
    /// Returns `true` when the parallel path actually ran, so a caller can
    /// assert that a case it means to cover really is covered.
    fn compare_paths(
        frame: &FrameHeader,
        scan: &ScanHeader,
        tables: &TableSet,
        interval: u16,
        entropy: &[u8],
        tolerate: bool,
    ) -> bool {
        let scan_tables = ScanTables {
            dc: &tables.dc_huffman,
            ac: &tables.ac_huffman,
            quant: &tables.quant,
        };
        let limits = DecodeLimits::default();

        let mut serial_planes = Planes::allocate(frame, &limits).expect("planes");
        let serial = decode_serial(
            frame,
            scan,
            &scan_tables,
            &tables.arithmetic,
            interval,
            entropy,
            &mut serial_planes,
            tolerate,
        );

        let mut parallel_planes = Planes::allocate(frame, &limits).expect("planes");
        let parallel = decode_sequential_parallel(
            frame,
            scan,
            &scan_tables,
            &tables.arithmetic,
            interval,
            entropy,
            &mut parallel_planes,
            &limits,
            tolerate,
        );

        let Some(parallel) = parallel else {
            // Declined: the serial path is what runs, so there is nothing that
            // could differ.
            return false;
        };
        let components = frame.components.len();
        match (parallel, serial) {
            (Ok(parallel), Ok(serial)) => {
                assert_eq!(
                    parallel.consumed, serial.consumed,
                    "scan length differs (tolerate = {tolerate})"
                );
                assert_eq!(
                    parallel.truncated, serial.truncated,
                    "truncation differs (tolerate = {tolerate})"
                );
                assert_eq!(
                    snapshot(&parallel_planes, components),
                    snapshot(&serial_planes, components),
                    "samples differ (tolerate = {tolerate})"
                );
            }
            (Err(parallel), Err(serial)) => {
                assert_eq!(
                    parallel.to_string(),
                    serial.to_string(),
                    "different errors (tolerate = {tolerate})"
                );
            }
            (parallel, serial) => panic!(
                "one path failed and the other did not (tolerate = {tolerate}): {:?} against {:?}",
                parallel.map(|o| o.consumed).map_err(|e| e.to_string()),
                serial.map(|o| o.consumed).map_err(|e| e.to_string()),
            ),
        }
        true
    }

    /// A stream this crate did not write must split too.
    ///
    /// The admission rule counts restart markers against what the `DRI`
    /// implies, so it is only as good as the assumption that a conforming
    /// encoder writes exactly that many. Measured here rather than assumed:
    /// `cjpeg -restart 1` on a 320x256 4:2:0 image writes 15 markers for 16
    /// intervals, `-restart 8B` writes 39 for 40, and `-arithmetic` the same
    /// as the Huffman spelling. A tightening of the rule that stopped foreign
    /// streams splitting would leave the `rayon` feature doing nothing on
    /// most files in the wild, which no output comparison could see.
    #[cfg(feature = "jpeg-oracle")]
    mod foreign {
        use super::*;
        use std::process::Command;

        /// Run `cjpeg` over a PPM, or `None` when it is not installed.
        fn cjpeg(args: &[&str], source: &[u8], width: usize, height: usize) -> Option<Vec<u8>> {
            let mut input = std::env::temp_dir();
            input.push(format!("oxiarc_jpeg_parallel_{}.ppm", std::process::id()));
            let mut output = std::env::temp_dir();
            output.push(format!("oxiarc_jpeg_parallel_{}.jpg", std::process::id()));
            let mut ppm = format!("P6\n{width} {height}\n255\n").into_bytes();
            ppm.extend_from_slice(source);
            std::fs::write(&input, &ppm).ok()?;
            let status = Command::new("cjpeg")
                .args(args)
                .arg("-outfile")
                .arg(&output)
                .arg(&input)
                .status()
                .ok()?;
            let jpeg = if status.success() {
                std::fs::read(&output).ok()
            } else {
                None
            };
            let _ = std::fs::remove_file(&input);
            let _ = std::fs::remove_file(&output);
            jpeg
        }

        #[test]
        fn libjpeg_streams_with_restart_markers_still_split() {
            let source = pixels(320, 256, 3);
            let cases: &[&[&str]] = &[
                &["-quality", "80", "-restart", "1"],
                &["-quality", "80", "-restart", "8B"],
                &["-quality", "90", "-sample", "1x1", "-restart", "2"],
                &["-quality", "80", "-arithmetic", "-restart", "1"],
            ];
            let mut checked = 0usize;
            for args in cases {
                let Some(jpeg) = cjpeg(args, &source, 320, 256) else {
                    eprintln!("skipping: cjpeg is not installed or refused {args:?}");
                    continue;
                };
                let (frame, scan, tables, start) = dissect(&jpeg);
                if !cfg!(feature = "arithmetic") && frame.entropy == EntropyCoding::Arithmetic {
                    continue;
                }
                let interval = tables.restart_interval.unwrap_or(0);
                assert!(interval > 0, "{args:?} wrote no DRI");
                assert!(
                    plan_bands(&frame, &scan, interval, &jpeg[start..]).is_some(),
                    "{args:?}: a conforming libjpeg stream must still split"
                );
                assert!(
                    compare_paths(&frame, &scan, &tables, interval, &jpeg[start..], false),
                    "{args:?}: the parallel path declined after all"
                );
                checked += 1;
            }
            if checked == 0 {
                eprintln!("cjpeg unavailable: no foreign stream was checked");
            }
        }
    }
}
