//! The `rayon` feature must not change a single byte.
//!
//! Restart intervals are the only thing this crate parallelises, on both
//! sides, and they are also the one thing that does **not** change what a
//! stream decodes to: T.81 E.1.4 resets the predictions and the entropy coder
//! at each marker, so an image encoded with restarts decodes to exactly the
//! same samples as the same image encoded without them. That gives every test
//! here a reference the parallel path cannot influence.
//!
//! The file compiles and runs with or without the feature; without it the
//! same assertions hold trivially, which is what keeps the serial path
//! covered by the same gate.

use oxiarc_jpeg::{
    Decoder, EncodeOptions, EncodeProcess, EntropyCoding, InputColor, RestartInterval, Subsampling,
    encode_to_vec_with_options,
};

/// Big enough that the parallel path engages: 320x256 at 4:2:0 is 20x16 MCUs
/// and at 4:4:4 40x32, both above the threshold below which splitting costs
/// more than it saves.
const WIDTH: u16 = 320;
const HEIGHT: u16 = 256;

fn source(width: usize, height: usize, channels: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(width * height * channels);
    for y in 0..height {
        for x in 0..width {
            let values = [
                ((x * 7 + y * 13) % 256) as u8,
                if (x / 5 + y / 3) % 2 == 0 { 255 } else { 32 },
                ((x * 3 + y * 11) % 256) as u8,
            ];
            for c in 0..channels {
                out.push(values[c % 3]);
            }
        }
    }
    out
}

fn decode(jpeg: &[u8]) -> Vec<u8> {
    Decoder::new(jpeg).decode().expect("decode")
}

/// Restart markers change the entropy coding and nothing else, so a stream
/// with them must decode to the same samples as one without — whichever path
/// each of the two took.
#[test]
fn restart_intervals_do_not_change_the_decoded_image() {
    let entropies: &[EntropyCoding] = &[
        EntropyCoding::Huffman,
        #[cfg(feature = "arithmetic")]
        EntropyCoding::Arithmetic,
    ];
    let pixels = source(usize::from(WIDTH), usize::from(HEIGHT), 3);
    for &entropy in entropies {
        for &subsampling in &[Subsampling::S444, Subsampling::S420] {
            let base = EncodeOptions {
                quality: 85,
                subsampling,
                entropy,
                ..Default::default()
            };
            let serial = encode_to_vec_with_options(
                &pixels,
                WIDTH,
                HEIGHT,
                InputColor::Rgb,
                &EncodeOptions {
                    restart_interval: RestartInterval::None,
                    ..base.clone()
                },
            )
            .expect("encode");
            let expected = decode(&serial);

            for &interval in &[
                RestartInterval::McuRows(1),
                RestartInterval::McuRows(3),
                RestartInterval::Mcus(7),
                RestartInterval::Mcus(64),
            ] {
                let jpeg = encode_to_vec_with_options(
                    &pixels,
                    WIDTH,
                    HEIGHT,
                    InputColor::Rgb,
                    &EncodeOptions {
                        restart_interval: interval,
                        ..base.clone()
                    },
                )
                .expect("encode");
                assert!(
                    jpeg.windows(2)
                        .any(|w| w[0] == 0xFF && (0xD0..=0xD7).contains(&w[1])),
                    "{interval:?} wrote no restart markers"
                );
                assert_eq!(
                    decode(&jpeg),
                    expected,
                    "{entropy:?} {subsampling:?} {interval:?}"
                );
            }
        }
    }
}

/// Grayscale, twelve-bit and progressive frames go down other paths; none of
/// them may change either.
#[test]
fn other_processes_are_unaffected_by_restart_intervals() {
    let entropies: &[EntropyCoding] = &[
        EntropyCoding::Huffman,
        #[cfg(feature = "arithmetic")]
        EntropyCoding::Arithmetic,
    ];
    let gray = source(usize::from(WIDTH), usize::from(HEIGHT), 1);
    for &entropy in entropies {
        for &process in &[EncodeProcess::Sequential, EncodeProcess::Progressive] {
            let base = EncodeOptions {
                quality: 80,
                process,
                entropy,
                ..Default::default()
            };
            let serial = encode_to_vec_with_options(
                &gray,
                WIDTH,
                HEIGHT,
                InputColor::Luma,
                &EncodeOptions {
                    restart_interval: RestartInterval::None,
                    ..base.clone()
                },
            )
            .expect("encode");
            let with_restarts = encode_to_vec_with_options(
                &gray,
                WIDTH,
                HEIGHT,
                InputColor::Luma,
                &EncodeOptions {
                    restart_interval: RestartInterval::McuRows(2),
                    ..base
                },
            )
            .expect("encode");
            assert_eq!(
                decode(&with_restarts),
                decode(&serial),
                "{entropy:?} {process:?}"
            );
        }
    }
}

/// A lossless frame is exact whatever the interval, and whichever coder.
#[test]
fn lossless_restart_intervals_stay_lossless() {
    let entropies: &[EntropyCoding] = &[
        EntropyCoding::Huffman,
        #[cfg(feature = "arithmetic")]
        EntropyCoding::Arithmetic,
    ];
    let pixels = source(128, 96, 3);
    for &entropy in entropies {
        for &interval in &[RestartInterval::None, RestartInterval::McuRows(4)] {
            let options = EncodeOptions {
                process: EncodeProcess::Lossless {
                    predictor: 6,
                    point_transform: 0,
                },
                restart_interval: interval,
                entropy,
                ..Default::default()
            };
            let jpeg = encode_to_vec_with_options(&pixels, 128, 96, InputColor::Rgb, &options)
                .expect("encode");
            assert_eq!(decode(&jpeg), pixels, "{entropy:?} {interval:?}");
        }
    }
}

/// Byte-for-byte: the encoder's output must not depend on how the work was
/// scheduled. Encoding the same input twice inside one process exercises the
/// same path twice, so this is a consistency check rather than a
/// serial-versus-parallel one; `README.md` records the cross-configuration
/// comparison, which is a `cargo test` with and without `--features rayon`.
#[test]
fn encoding_is_deterministic() {
    let pixels = source(usize::from(WIDTH), usize::from(HEIGHT), 3);
    let options = EncodeOptions {
        quality: 90,
        restart_interval: RestartInterval::Mcus(5),
        ..Default::default()
    };
    let first =
        encode_to_vec_with_options(&pixels, WIDTH, HEIGHT, InputColor::Rgb, &options).expect("a");
    let second =
        encode_to_vec_with_options(&pixels, WIDTH, HEIGHT, InputColor::Rgb, &options).expect("b");
    assert_eq!(first, second);
}

/// A 64-bit FNV-1a hash, so a pinned value can stand in for a whole stream.
fn digest(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// The cross-configuration gate: these digests were measured **without**
/// `--features rayon` and are asserted **with and without** it, so a
/// scheduling change that altered a single byte would fail here rather than
/// in some downstream project's golden test.
///
/// Both fixtures carry restart markers and are large enough to be split, so
/// each digest covers a real parallel encode as well as a real parallel
/// decode (`restart_intervals_do_not_change_the_decoded_image` decodes them).
#[test]
fn parallel_and_serial_encoders_produce_the_same_bytes() {
    let pixels = source(usize::from(WIDTH), usize::from(HEIGHT), 3);

    let huffman = EncodeOptions {
        quality: 85,
        subsampling: Subsampling::S420,
        restart_interval: RestartInterval::McuRows(1),
        ..Default::default()
    };
    let bytes = encode_to_vec_with_options(&pixels, WIDTH, HEIGHT, InputColor::Rgb, &huffman)
        .expect("encode");
    assert_eq!(
        digest(&bytes),
        HUFFMAN_RESTART_DIGEST,
        "the Huffman restart stream changed ({} bytes)",
        bytes.len()
    );

    #[cfg(feature = "arithmetic")]
    {
        let arithmetic = EncodeOptions {
            entropy: EntropyCoding::Arithmetic,
            ..huffman
        };
        let bytes =
            encode_to_vec_with_options(&pixels, WIDTH, HEIGHT, InputColor::Rgb, &arithmetic)
                .expect("encode");
        assert_eq!(
            digest(&bytes),
            ARITHMETIC_RESTART_DIGEST,
            "the arithmetic restart stream changed ({} bytes)",
            bytes.len()
        );
    }
}

const HUFFMAN_RESTART_DIGEST: u64 = 12_938_618_031_895_654_287;
#[cfg(feature = "arithmetic")]
const ARITHMETIC_RESTART_DIGEST: u64 = 15_491_527_774_637_948_187;

/// The decode-side twin of the digest above, and the only cross-configuration
/// gate on the parallel **decoder** that needs no reference tools.
///
/// The equality tests in this file compare a parallel decode against a serial
/// decode computed in the same process, which catches a band that decodes
/// wrongly but not a scheduling change that shifts every band the same way.
/// These digests were measured **without** `--features rayon` and are
/// asserted with and without it, so the parallel decoder is pinned to bytes a
/// serial build produced. Both fixtures carry a restart marker per MCU row
/// and are large enough to split, and the arithmetic one is the case that
/// reached the parallel decoder only after the scan dispatch was reordered.
///
/// A failure here means the *decoded samples* changed. If that is an intended
/// decoder change rather than a scheduling bug, re-measure both constants in
/// a build without the `rayon` feature.
#[test]
fn decoded_samples_match_digests_pinned_in_the_serial_configuration() {
    let pixels = source(usize::from(WIDTH), usize::from(HEIGHT), 3);
    let huffman = EncodeOptions {
        quality: 85,
        subsampling: Subsampling::S420,
        restart_interval: RestartInterval::McuRows(1),
        ..Default::default()
    };
    let bytes = encode_to_vec_with_options(&pixels, WIDTH, HEIGHT, InputColor::Rgb, &huffman)
        .expect("encode");
    let samples = decode(&bytes);
    assert_eq!(
        digest(&samples),
        HUFFMAN_RESTART_DECODE_DIGEST,
        "the Huffman restart decode changed ({} samples)",
        samples.len()
    );

    #[cfg(feature = "arithmetic")]
    {
        let arithmetic = EncodeOptions {
            entropy: EntropyCoding::Arithmetic,
            ..huffman
        };
        let bytes =
            encode_to_vec_with_options(&pixels, WIDTH, HEIGHT, InputColor::Rgb, &arithmetic)
                .expect("encode");
        let samples = decode(&bytes);
        assert_eq!(
            digest(&samples),
            ARITHMETIC_RESTART_DECODE_DIGEST,
            "the arithmetic restart decode changed ({} samples)",
            samples.len()
        );
    }
}

const HUFFMAN_RESTART_DECODE_DIGEST: u64 = 10_778_683_219_384_373_318;
/// Equal to `HUFFMAN_RESTART_DECODE_DIGEST` on purpose: only the entropy
/// coder differs between the two fixtures, so the coefficients — and so the
/// samples — are identical. That the two constants coincide is itself a
/// check on the QM coder.
#[cfg(feature = "arithmetic")]
const ARITHMETIC_RESTART_DECODE_DIGEST: u64 = 10_778_683_219_384_373_318;

/// The final band of a split scan is a *partial* one whenever the height is
/// not a multiple of the MCU height: every band above it covers whole MCU
/// rows, and the last one has to be clipped when its plane is merged back
/// into the frame. That clip is the only geometry-dependent step in the
/// merge, so it gets a case of its own.
///
/// 320x250 at 4:2:0 is 20x16 MCUs — above the threshold below which the scan
/// is not split — and 250 is 15 whole MCU rows plus 10 pixel rows; at 4:4:4
/// it is 40x32 MCUs with 2 rows left over. The 314-wide variant adds a
/// partial MCU *column*, so the clipped band is ragged in both directions.
#[test]
fn a_partial_last_mcu_row_survives_the_parallel_merge() {
    let entropies: &[EntropyCoding] = &[
        EntropyCoding::Huffman,
        #[cfg(feature = "arithmetic")]
        EntropyCoding::Arithmetic,
    ];
    for &(width, height) in &[(320u16, 250u16), (314, 250)] {
        let pixels = source(usize::from(width), usize::from(height), 3);
        for &entropy in entropies {
            for &subsampling in &[Subsampling::S444, Subsampling::S420] {
                let base = EncodeOptions {
                    quality: 88,
                    subsampling,
                    entropy,
                    ..Default::default()
                };
                let serial = encode_to_vec_with_options(
                    &pixels,
                    width,
                    height,
                    InputColor::Rgb,
                    &EncodeOptions {
                        restart_interval: RestartInterval::None,
                        ..base.clone()
                    },
                )
                .expect("encode");
                let expected = decode(&serial);
                assert_eq!(
                    expected.len(),
                    usize::from(width) * usize::from(height) * 3,
                    "{width}x{height} decoded to the wrong size"
                );

                for &interval in &[RestartInterval::McuRows(1), RestartInterval::McuRows(2)] {
                    let jpeg = encode_to_vec_with_options(
                        &pixels,
                        width,
                        height,
                        InputColor::Rgb,
                        &EncodeOptions {
                            restart_interval: interval,
                            ..base.clone()
                        },
                    )
                    .expect("encode");
                    assert!(
                        jpeg.windows(2)
                            .any(|w| w[0] == 0xFF && (0xD0..=0xD7).contains(&w[1])),
                        "{interval:?} wrote no restart markers"
                    );
                    assert_eq!(
                        decode(&jpeg),
                        expected,
                        "{width}x{height} {entropy:?} {subsampling:?} {interval:?}"
                    );
                }
            }
        }
    }
}
