//! The multi-MCU fixtures under `tests/data`, decoded intact and then used to
//! pin restart resynchronisation.
//!
//! `tests/corrupt_no_panic.rs` mutates these same five files at every offset.
//! That sweep is only meaningful if the originals really are the frames
//! `tests/data/README.md` claims, so every property it relies on — process,
//! precision, component count, sampling, restart interval — is asserted here.
//! A fixture that silently stopped being a valid progressive or lossless JPEG
//! would turn the sweep vacuous without failing anything; these tests are what
//! stops that.

use oxiarc_jpeg::{CodingProcess, DecodeOptions, Decoder, Upsampling};

const PROGRESSIVE_2X2: &[u8] = include_bytes!("data/progressive_2x2.jpg");
const RESTART_2X2: &[u8] = include_bytes!("data/restart_2x2.jpg");
const PROGRESSIVE_RESTART: &[u8] = include_bytes!("data/progressive_restart.jpg");
const TWELVE_BIT_GRAY: &[u8] = include_bytes!("data/twelve_bit_gray.jpg");
const LOSSLESS_RGB: &[u8] = include_bytes!("data/lossless_rgb.jpg");

/// Decode with a chosen upsampler at 8-bit precision.
fn decode(data: &[u8], upsampling: Upsampling) -> (usize, usize, Vec<u8>) {
    let options = DecodeOptions {
        upsampling,
        ..DecodeOptions::default()
    };
    let mut decoder = Decoder::with_options(data, options);
    let info = decoder.read_info().expect("read_info");
    let pixels = decoder.decode().expect("decode");
    (usize::from(info.width), usize::from(info.height), pixels)
}

#[test]
fn every_fixture_is_the_frame_the_readme_claims() {
    for (name, data, process, precision, components, subsampling) in [
        (
            "progressive_2x2",
            PROGRESSIVE_2X2,
            CodingProcess::Progressive,
            8u8,
            3u8,
            (2u8, 2u8),
        ),
        (
            "restart_2x2",
            RESTART_2X2,
            CodingProcess::Baseline,
            8,
            3,
            (2, 2),
        ),
        (
            "progressive_restart",
            PROGRESSIVE_RESTART,
            CodingProcess::Progressive,
            8,
            3,
            (2, 2),
        ),
        (
            "twelve_bit_gray",
            TWELVE_BIT_GRAY,
            CodingProcess::ExtendedSequential,
            12,
            1,
            (1, 1),
        ),
        (
            "lossless_rgb",
            LOSSLESS_RGB,
            CodingProcess::Lossless,
            8,
            3,
            (1, 1),
        ),
    ] {
        let mut decoder = Decoder::new(data);
        let info = decoder
            .read_info()
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!((info.width, info.height), (24, 20), "{name} dimensions");
        assert_eq!(info.process, process, "{name} process");
        assert_eq!(info.precision, precision, "{name} precision");
        assert_eq!(info.num_components, components, "{name} components");
        assert_eq!(info.subsampling, subsampling, "{name} subsampling");

        let samples = decoder
            .decode_u16()
            .unwrap_or_else(|e| panic!("{name} decode: {e}"));
        assert_eq!(
            samples.len(),
            24 * 20 * info.output_components(),
            "{name} output size"
        );
        assert!(
            samples.iter().any(|&s| s != samples[0]),
            "{name} decoded to a flat image, which the source is not"
        );
    }
}

#[test]
fn the_restart_fixtures_really_carry_restart_markers() {
    for (name, data, expected) in [
        ("restart_2x2", RESTART_2X2, 3),
        ("progressive_restart", PROGRESSIVE_RESTART, 3),
    ] {
        let mut decoder = Decoder::new(data);
        let at_header = decoder.read_info().expect("read_info");
        assert_eq!(
            at_header.restart_interval, 0,
            "{name}: libjpeg writes DRI after SOF, so read_info cannot know it yet"
        );
        decoder.decode().expect("decode");
        let after = decoder.info().expect("info");
        assert_eq!(after.restart_interval, 1, "{name} DRI after decoding");
        let markers = data
            .windows(2)
            .filter(|w| w[0] == 0xFF && (0xD0..=0xD7).contains(&w[1]))
            .count();
        assert!(
            markers >= expected,
            "{name}: expected at least {expected} RST markers, found {markers}"
        );
    }
}

/// The 4:2:0 MCU is 16x16, so a 24x20 frame is a 2x2 grid of MCUs and
/// `DRI = 1` puts a restart marker after each one. Damage confined to the
/// first interval must not reach the last MCU: the restart marker resets the
/// DC predictors *and* re-aligns the bit reader, which is the whole point of
/// restart intervals.
///
/// Box upsampling is used deliberately. Fancy chroma upsampling interpolates
/// across MCU boundaries, so a corrupted chroma block genuinely does bleed one
/// pixel column into its neighbour and the test would be asserting something
/// false.
#[test]
fn damage_inside_one_restart_interval_does_not_reach_the_last_mcu() {
    let (width, height, clean) = decode(RESTART_2X2, Upsampling::Box);
    assert_eq!((width, height), (24, 20));

    // The entropy-coded data of interval 0: everything between the end of the
    // SOS header and the first RST marker.
    let sos = RESTART_2X2
        .windows(2)
        .position(|w| w == [0xFF, 0xDA])
        .expect("SOS");
    let header_length = usize::from(u16::from_be_bytes([
        RESTART_2X2[sos + 2],
        RESTART_2X2[sos + 3],
    ]));
    let entropy_start = sos + 2 + header_length;
    let first_rst = entropy_start
        + RESTART_2X2[entropy_start..]
            .windows(2)
            .position(|w| w[0] == 0xFF && (0xD0..=0xD7).contains(&w[1]))
            .expect("RST0");
    assert!(
        first_rst > entropy_start + 4,
        "interval 0 is too short to corrupt meaningfully"
    );

    // The last MCU covers x in 16..24, y in 16..20.
    let last_mcu = |pixels: &[u8]| -> Vec<u8> {
        let mut out = Vec::new();
        for y in 16..20 {
            for x in 16..24 {
                let base = (y * width + x) * 3;
                out.extend_from_slice(&pixels[base..base + 3]);
            }
        }
        out
    };
    let clean_tail = last_mcu(&clean);

    let mut recovered = 0usize;
    let mut differed = 0usize;
    for offset in entropy_start..first_rst {
        for mask in [0x01u8, 0x10, 0x55] {
            let mut corrupt = RESTART_2X2.to_vec();
            corrupt[offset] ^= mask;
            // Never fabricate a marker: that changes the stream's structure
            // rather than its entropy data.
            if corrupt[offset] == 0xFF || RESTART_2X2[offset] == 0xFF {
                continue;
            }
            let options = DecodeOptions {
                upsampling: Upsampling::Box,
                ..DecodeOptions::default()
            };
            let mut decoder = Decoder::with_options(&corrupt[..], options);
            if decoder.read_info().is_err() {
                continue;
            }
            let Ok(pixels) = decoder.decode() else {
                continue;
            };
            recovered += 1;
            assert_eq!(
                last_mcu(&pixels),
                clean_tail,
                "corrupting byte {offset} (mask {mask:#04x}) of restart interval 0 \
                 changed the last MCU, so the decoder did not resynchronise"
            );
            if pixels[..3] != clean[..3] {
                differed += 1;
            }
        }
    }

    assert!(
        recovered >= 8,
        "only {recovered} corruptions decoded at all; the test proves nothing"
    );
    assert!(
        differed > 0,
        "no corruption changed the first MCU, so the damage never landed"
    );
}

/// The same property one level harder: progressive scans carry an EOB run that
/// spans blocks, and it must be cleared at every restart marker too.
#[test]
fn progressive_restart_intervals_also_resynchronise() {
    let (width, _, clean) = decode(PROGRESSIVE_RESTART, Upsampling::Box);
    let sos = PROGRESSIVE_RESTART
        .windows(2)
        .position(|w| w == [0xFF, 0xDA])
        .expect("SOS");
    let header_length = usize::from(u16::from_be_bytes([
        PROGRESSIVE_RESTART[sos + 2],
        PROGRESSIVE_RESTART[sos + 3],
    ]));
    let entropy_start = sos + 2 + header_length;
    let first_rst = entropy_start
        + PROGRESSIVE_RESTART[entropy_start..]
            .windows(2)
            .position(|w| w[0] == 0xFF && (0xD0..=0xD7).contains(&w[1]))
            .expect("RST0");

    let last_row = |pixels: &[u8]| pixels[(19 * width + 20) * 3..(19 * width + 24) * 3].to_vec();
    let clean_tail = last_row(&clean);

    let mut recovered = 0usize;
    for offset in entropy_start..first_rst {
        for mask in [0x01u8, 0x40] {
            let mut corrupt = PROGRESSIVE_RESTART.to_vec();
            corrupt[offset] ^= mask;
            if corrupt[offset] == 0xFF || PROGRESSIVE_RESTART[offset] == 0xFF {
                continue;
            }
            let options = DecodeOptions {
                upsampling: Upsampling::Box,
                ..DecodeOptions::default()
            };
            let mut decoder = Decoder::with_options(&corrupt[..], options);
            if decoder.read_info().is_err() {
                continue;
            }
            let Ok(pixels) = decoder.decode() else {
                continue;
            };
            recovered += 1;
            assert_eq!(
                last_row(&pixels),
                clean_tail,
                "corrupting byte {offset} (mask {mask:#04x}) of the first progressive \
                 restart interval changed the bottom-right pixels"
            );
        }
    }
    assert!(
        recovered >= 4,
        "only {recovered} corruptions decoded at all; the test proves nothing"
    );
}

/// T.81 G.1.2.2 requires `EOBRUN` to be reset at every restart marker, and
/// libjpeg does it in `process_restart`. No encoder exercises the rule: a
/// conforming encoder always flushes its EOB run before emitting `RST`, so the
/// counter is already zero at the boundary and dropping the reset changes
/// nothing on any file `cjpeg` can produce.
///
/// This stream is therefore built by hand. The first restart interval ends
/// with `EOB1` and an extra bit of 1, i.e. `EOBRUN = 3`, which the block itself
/// consumes one of — leaving two blocks' worth of skip pending across the
/// restart marker. The second interval then codes one real AC coefficient. If
/// the reset is missing, that coefficient is never read and the second block
/// stays flat.
fn progressive_eob_run_across_restart() -> Vec<u8> {
    let mut out: Vec<u8> = vec![0xFF, 0xD8];

    // DQT: Pq = 0, Tq = 0, every entry 16 so one AC coefficient is visible.
    out.extend_from_slice(&[0xFF, 0xDB, 0x00, 0x43, 0x00]);
    out.extend(std::iter::repeat_n(16u8, 64));

    // SOF2: P = 8, Y = 8, X = 16, one 1x1 component with quant table 0.
    out.extend_from_slice(&[
        0xFF, 0xC2, 0x00, 0x0B, 0x08, 0x00, 0x08, 0x00, 0x10, 0x01, 0x01, 0x11, 0x00,
    ]);

    // DHT, class 0 slot 0: one 1-bit code "0" for symbol 0x00 (DC diff of 0).
    out.extend_from_slice(&[0xFF, 0xC4, 0x00, 0x14, 0x00, 0x01]);
    out.extend(std::iter::repeat_n(0u8, 15));
    out.push(0x00);

    // SOS: DC first scan, Ss = Se = 0, no successive approximation.
    out.extend_from_slice(&[0xFF, 0xDA, 0x00, 0x08, 0x01, 0x01, 0x00, 0x00, 0x00, 0x00]);
    // Two blocks, each the 1-bit code "0"; the byte is padded with 1 bits.
    out.push(0b0011_1111);

    // DHT, class 1 slot 0: one 1-bit code and two 2-bit ones, so
    // "0" = 0x04 (zero run 0, magnitude category 4), "10" = 0x00 (EOB0) and
    // "11" = 0x10 (EOB1).
    out.extend_from_slice(&[0xFF, 0xC4, 0x00, 0x16, 0x10, 0x01, 0x02]);
    out.extend(std::iter::repeat_n(0u8, 14));
    out.extend_from_slice(&[0x04, 0x00, 0x10]);

    // DRI: restart after every block.
    out.extend_from_slice(&[0xFF, 0xDD, 0x00, 0x04, 0x00, 0x01]);

    // SOS: AC first scan over the whole band.
    out.extend_from_slice(&[0xFF, 0xDA, 0x00, 0x08, 0x01, 0x01, 0x00, 0x01, 0x3F, 0x00]);
    // Block 0: "11" (EOB1) then one extra bit "1" -> EOBRUN = 3, of which this
    // block consumes one, leaving two pending across the restart marker.
    out.push(0b1110_0000);
    out.extend_from_slice(&[0xFF, 0xD0]);
    // Block 1: "0" (run 0, size 4) then "1110" = 14, then "10" (EOB0) to close
    // the block.
    out.push(0b0111_0100);

    out.extend_from_slice(&[0xFF, 0xD9]);
    out
}

#[test]
fn an_eob_run_does_not_survive_a_restart_marker() {
    let stream = progressive_eob_run_across_restart();
    let mut decoder = Decoder::new(&stream[..]);
    let info = decoder.read_info().expect("read_info");
    assert_eq!((info.width, info.height), (16, 8));
    assert_eq!(info.process, CodingProcess::Progressive);
    let pixels = decoder.decode().expect("decode");
    assert_eq!(pixels.len(), 16 * 8);

    // Block 0 was covered by the EOB run, so it carries no AC energy at all.
    let left: Vec<u8> = (0..8)
        .flat_map(|y| (0..8).map(move |x| (y, x)))
        .map(|(y, x)| pixels[y * 16 + x])
        .collect();
    assert!(
        left.iter().all(|&p| p == left[0]),
        "block 0 should be flat, its AC band was an EOB run; got {left:?}"
    );

    // Block 1 is in the next restart interval. Its AC coefficient is only read
    // if the EOB run was cleared by the restart marker.
    let right: Vec<u8> = (0..8)
        .flat_map(|y| (8..16).map(move |x| (y, x)))
        .map(|(y, x)| pixels[y * 16 + x])
        .collect();
    let min = right.iter().copied().min().unwrap_or(0);
    let max = right.iter().copied().max().unwrap_or(0);
    assert!(
        max - min > 32,
        "block 1 is flat ({min}..{max}), so the EOB run survived the restart \
         marker and swallowed its coefficient"
    );
}
