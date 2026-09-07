//! Adversarial regression suite (track F-verify).
//!
//! Every test here pins a defect that was found by attacking the crate rather
//! than by exercising it: a panic reachable from a hostile file or from a
//! caller-supplied parameter the type system allows, and a determinism
//! property the ordinary suite cannot observe because it runs in one process
//! with one thread pool.

use oxiarc_jpeg::{
    ArithmeticConditioning, DecodeOptions, Decoder, EncodeOptions, EncodeProcess, EntropyCoding,
    InputColor, JpegError, QuantTable, QuantTableSource, RestartInterval, Scale, ScanSpec,
    Subsampling, encode_to_vec_with_options, tiff,
};

// ── D2: a hostile stream must not overflow the dequantising multiply ──────

/// A minimal 8-bit baseline grayscale stream whose DC prediction accumulates
/// past `i16` while every quantiser is `0xFFFF`.
///
/// `Pq = 1` in an 8-bit frame is not forbidden by T.81's syntax and libjpeg
/// accepts it, so the quantisers really can be 65 535. The DC coefficient is a
/// running sum of `DIFF`s, and one 15-bit `DIFF` is 32 767, so two blocks put
/// it at 65 534. `65_534 * 65_535` does not fit an `i32`.
fn accumulated_dc_stream(blocks_wide: u16) -> Vec<u8> {
    let mut out = vec![0xFFu8, 0xD8];
    let length = 2 + 1 + 128;
    out.extend_from_slice(&[0xFF, 0xDB, (length >> 8) as u8, length as u8, 0x10]);
    out.extend_from_slice(&[0xFF; 128]);
    let width = 8u16 * blocks_wide;
    out.extend_from_slice(&[0xFF, 0xC0, 0x00, 0x0B, 0x08, 0x00, 0x08]);
    out.extend_from_slice(&width.to_be_bytes());
    out.extend_from_slice(&[0x01, 0x01, 0x11, 0x00]);
    // One 1-bit DC code for category 15, one for the AC end-of-block.
    for tc_th in [0x00u8, 0x10] {
        out.extend_from_slice(&[0xFF, 0xC4, 0x00, 0x14, tc_th]);
        out.push(1);
        out.extend_from_slice(&[0u8; 15]);
        out.push(if tc_th == 0 { 15 } else { 0 });
    }
    out.extend_from_slice(&[0xFF, 0xDA, 0x00, 0x08, 0x01, 0x01, 0x00, 0x00, 0x3F, 0x00]);

    let mut bits: Vec<u8> = Vec::new();
    for _ in 0..blocks_wide {
        bits.push(0); // DC category 15
        bits.extend(std::iter::repeat_n(1u8, 15)); // DIFF = +32767
        bits.push(0); // EOB
    }
    while bits.len() % 8 != 0 {
        bits.push(1);
    }
    for chunk in bits.chunks(8) {
        let mut byte = 0u8;
        for (index, &bit) in chunk.iter().enumerate() {
            byte |= bit << (7 - index);
        }
        out.push(byte);
        if byte == 0xFF {
            out.push(0x00);
        }
    }
    out.extend_from_slice(&[0xFF, 0xD9]);
    out
}

#[test]
fn a_sixteen_bit_dqt_with_an_accumulated_dc_does_not_overflow_the_idct() {
    for blocks_wide in [1u16, 2, 3, 8, 64] {
        let data = accumulated_dc_stream(blocks_wide);
        let mut decoder = Decoder::new(&data[..]);
        let info = decoder.read_info().expect("read_info");
        assert_eq!(info.width, 8 * blocks_wide);
        let pixels = decoder.decode().expect("decode");
        assert_eq!(pixels.len(), usize::from(info.width) * 8);

        // The wide entry point walks the same IDCT.
        let mut decoder = Decoder::new(&data[..]);
        decoder.read_info().expect("read_info");
        let wide = decoder.decode_u16().expect("decode_u16");
        assert_eq!(wide.len(), pixels.len());
    }
}

/// The progressive spelling of the same attack: `Al = 13` multiplies a coded
/// coefficient by 8 192 before it ever reaches the coefficient buffer, so one
/// block is enough.
#[test]
fn a_progressive_successive_approximation_shift_does_not_overflow_the_idct() {
    let mut data = vec![0xFFu8, 0xD8];
    let length = 2 + 1 + 128;
    data.extend_from_slice(&[0xFF, 0xDB, (length >> 8) as u8, length as u8, 0x10]);
    data.extend_from_slice(&[0xFF; 128]);
    // SOF2, 8-bit, 8x8, one component.
    data.extend_from_slice(&[
        0xFF, 0xC2, 0x00, 0x0B, 0x08, 0x00, 0x08, 0x00, 0x08, 0x01, 0x01, 0x11, 0x00,
    ]);
    data.extend_from_slice(&[0xFF, 0xC4, 0x00, 0x14, 0x00]);
    data.push(1);
    data.extend_from_slice(&[0u8; 15]);
    data.push(15);
    // One DC scan with Ah = 0, Al = 13.
    data.extend_from_slice(&[0xFF, 0xDA, 0x00, 0x08, 0x01, 0x01, 0x00, 0x00, 0x00, 0x0D]);
    // DC category 15 with all-ones magnitude, then padding.
    data.extend_from_slice(&[0x7F, 0xFF, 0x00, 0xFF, 0x00]);
    data.extend_from_slice(&[0xFF, 0xD9]);

    let mut decoder = Decoder::new(&data[..]);
    decoder.read_info().expect("read_info");
    let pixels = decoder.decode().expect("decode");
    assert_eq!(pixels.len(), 64);
}

// ── D1: strided destinations ─────────────────────────────────────────────

fn baseline_rgb(width: u16, height: u16) -> Vec<u8> {
    let pixels: Vec<u8> = (0..usize::from(width) * usize::from(height) * 3)
        .map(|i| (i * 7 % 251) as u8)
        .collect();
    encode_to_vec_with_options(
        &pixels,
        width,
        height,
        InputColor::Rgb,
        &EncodeOptions::default(),
    )
    .expect("encode")
}

#[test]
fn a_stride_no_buffer_can_hold_is_a_named_error_not_an_overflow() {
    let data = baseline_rgb(8, 8);
    for stride in [usize::MAX, usize::MAX / 2, usize::MAX / 3, 1 << 62] {
        let mut out = vec![0u8; 8 * 8 * 3];
        let mut decoder = Decoder::new(&data[..]);
        decoder.read_info().expect("read_info");
        assert!(
            matches!(
                decoder.decode_into_strided(&mut out, stride),
                Err(JpegError::BufferTooSmall { .. })
            ),
            "stride {stride} must be reported, never multiplied"
        );

        let mut out = vec![0u16; 8 * 8 * 3];
        let mut decoder = Decoder::new(&data[..]);
        decoder.read_info().expect("read_info");
        assert!(matches!(
            decoder.decode_into_u16_strided(&mut out, stride),
            Err(JpegError::BufferTooSmall { .. })
        ));
    }
}

#[test]
fn strided_destinations_are_checked_at_every_boundary() {
    let (width, height) = (17u16, 5u16);
    let data = baseline_rgb(width, height);
    let row = usize::from(width) * 3;
    let rows = usize::from(height);

    // Exactly enough: full strides for every row but the last.
    for stride in [row, row + 1, row * 2, row + 7] {
        let need = stride * (rows - 1) + row;
        let mut out = vec![0u8; need];
        let mut decoder = Decoder::new(&data[..]);
        decoder.read_info().expect("read_info");
        decoder
            .decode_into_strided(&mut out, stride)
            .unwrap_or_else(|e| panic!("stride {stride} with exactly {need} bytes: {e}"));

        // One byte short must be refused, never truncated.
        let mut out = vec![0u8; need - 1];
        let mut decoder = Decoder::new(&data[..]);
        decoder.read_info().expect("read_info");
        assert!(matches!(
            decoder.decode_into_strided(&mut out, stride),
            Err(JpegError::BufferTooSmall { .. })
        ));
    }

    // A stride narrower than one row is refused rather than wrapping rows.
    for stride in [0usize, 1, row - 1] {
        let mut out = vec![0u8; row * rows];
        let mut decoder = Decoder::new(&data[..]);
        decoder.read_info().expect("read_info");
        assert!(matches!(
            decoder.decode_into_strided(&mut out, stride),
            Err(JpegError::BufferTooSmall { .. })
        ));
    }
}

/// A strided write must leave the padding between rows untouched: that is the
/// property a tiled TIFF reader depends on when it decodes into a
/// sub-rectangle of a larger image.
#[test]
fn a_strided_write_never_touches_the_padding_between_rows() {
    let (width, height) = (9u16, 4u16);
    let data = baseline_rgb(width, height);
    let row = usize::from(width) * 3;
    let stride = row + 11;
    let mut strided = vec![0xA5u8; stride * usize::from(height)];
    let mut decoder = Decoder::new(&data[..]);
    decoder.read_info().expect("read_info");
    decoder
        .decode_into_strided(&mut strided, stride)
        .expect("strided decode");

    let mut packed = vec![0u8; row * usize::from(height)];
    let mut decoder = Decoder::new(&data[..]);
    decoder.read_info().expect("read_info");
    decoder.decode_into(&mut packed).expect("packed decode");

    for y in 0..usize::from(height) {
        assert_eq!(
            &strided[y * stride..y * stride + row],
            &packed[y * row..y * row + row],
            "row {y}"
        );
        assert!(
            strided[y * stride + row..(y + 1) * stride]
                .iter()
                .all(|&b| b == 0xA5),
            "padding after row {y} was written"
        );
    }
}

// ── D3: a zero quantiser must not divide by zero ─────────────────────────

#[test]
fn a_zero_quantiser_is_rejected_rather_than_dividing_by_zero() {
    for zero_at in [0usize, 1, 40, 63] {
        let mut values = [16u16; 64];
        values[zero_at] = 0;
        let table = QuantTable::from_natural(values);
        let options = EncodeOptions {
            quant_tables: QuantTableSource::Custom(Box::new([Some(table); 4])),
            ..Default::default()
        };
        let pixels = vec![37u8; 8 * 8 * 3];
        assert!(
            matches!(
                encode_to_vec_with_options(&pixels, 8, 8, InputColor::Rgb, &options),
                Err(JpegError::InvalidEncodeParameter {
                    parameter: "quant_tables",
                    ..
                })
            ),
            "a zero at {zero_at} must be rejected"
        );
    }

    // A table with no zero is still accepted, and `Flat(0)` still clamps.
    let table = QuantTable::from_natural([1u16; 64]);
    let options = EncodeOptions {
        quant_tables: QuantTableSource::Custom(Box::new([Some(table); 4])),
        ..Default::default()
    };
    let pixels = vec![37u8; 8 * 8 * 3];
    assert!(encode_to_vec_with_options(&pixels, 8, 8, InputColor::Rgb, &options).is_ok());
    let options = EncodeOptions {
        quant_tables: QuantTableSource::Flat(0),
        ..Default::default()
    };
    assert!(encode_to_vec_with_options(&pixels, 8, 8, InputColor::Rgb, &options).is_ok());
}

// ── D4: DAC conditioning the decoder would refuse ────────────────────────

#[test]
fn out_of_range_arithmetic_conditioning_never_reaches_the_stream() {
    let pixels = vec![37u8; 16 * 16 * 3];
    // `Kx` outside 1..=63, and `L > U`: T.81 B.2.4.3.
    for (dc, ac) in [
        (0x10u8, 0u8),
        (0x10, 64),
        (0x10, 255),
        (0x0F, 5),
        (0x01, 5),
        (0x5A, 5),
        (0xF0, 0),
    ] {
        let options = EncodeOptions {
            entropy: EntropyCoding::Arithmetic,
            arithmetic: ArithmeticConditioning {
                dc: [dc; 4],
                ac: [ac; 4],
            },
            ..Default::default()
        };
        let result = encode_to_vec_with_options(&pixels, 16, 16, InputColor::Rgb, &options);
        // Every value in this table is illegal, so the parameter check must
        // fire — and it must fire *before* the "no arithmetic coder in this
        // build" refusal, so that the diagnosis is the same either way.
        assert!(
            matches!(
                result,
                Err(JpegError::InvalidEncodeParameter {
                    parameter: "arithmetic",
                    ..
                })
            ),
            "dc {dc:#04X} ac {ac} must be reported as an invalid parameter, got {:?}",
            result.map(|bytes| bytes.len())
        );
    }

    // Every legal spelling must still encode *and* decode: the check must not
    // have narrowed what the encoder accepts.
    for (dc, ac) in [(0x10u8, 5u8), (0x00, 1), (0xFF, 63), (0x21, 17), (0x88, 3)] {
        let options = EncodeOptions {
            entropy: EntropyCoding::Arithmetic,
            arithmetic: ArithmeticConditioning {
                dc: [dc; 4],
                ac: [ac; 4],
            },
            ..Default::default()
        };
        let result = encode_to_vec_with_options(&pixels, 16, 16, InputColor::Rgb, &options);
        #[cfg(feature = "arithmetic")]
        {
            let bytes = result.unwrap_or_else(|e| panic!("dc {dc:#04X} ac {ac}: {e}"));
            let mut decoder = Decoder::new(&bytes[..]);
            decoder.read_info().expect("read_info");
            assert_eq!(decoder.decode().expect("decode").len(), 16 * 16 * 3);
        }
        #[cfg(not(feature = "arithmetic"))]
        {
            // Without the coder the frame is refused — but as `Unsupported`,
            // never as an invalid parameter, because the parameter is legal.
            assert!(
                matches!(result, Err(JpegError::Unsupported(_))),
                "dc {dc:#04X} ac {ac}: {:?}",
                result.map(|bytes| bytes.len())
            );
        }
    }

    // The TIFF `JPEGTables` writers route through `build_plan` too, so the
    // same value cannot reach tag 347 either.
    for (dc, ac) in [(0x10u8, 0u8), (0x10, 200), (0x0F, 5)] {
        let options = EncodeOptions {
            entropy: EntropyCoding::Arithmetic,
            arithmetic: ArithmeticConditioning {
                dc: [dc; 4],
                ac: [ac; 4],
            },
            ..EncodeOptions::tiff_strip(75)
        };
        assert!(
            matches!(
                oxiarc_jpeg::table_set(&options, InputColor::Rgb),
                Err(JpegError::InvalidEncodeParameter {
                    parameter: "arithmetic",
                    ..
                })
            ),
            "table_set must refuse dc {dc:#04X} ac {ac}"
        );
        let mut blob = Vec::new();
        let mut encoder = oxiarc_jpeg::Encoder::with_options(&mut blob, options);
        assert!(
            encoder
                .write_tables_only(oxiarc_jpeg::TablesMode::BOTH, InputColor::Rgb)
                .is_err(),
            "write_tables_only must refuse dc {dc:#04X} ac {ac}"
        );
        drop(encoder);
        assert!(blob.is_empty(), "nothing may be written before the refusal");
    }

    // The legal spelling still produces a blob that parses back unchanged.
    let options = EncodeOptions {
        entropy: EntropyCoding::Arithmetic,
        arithmetic: ArithmeticConditioning {
            dc: [0x21; 4],
            ac: [17; 4],
        },
        ..EncodeOptions::tiff_strip(75)
    };
    let set = oxiarc_jpeg::table_set(&options, InputColor::Rgb).expect("table_set");
    let blob = set.emit(oxiarc_jpeg::TablesMode::BOTH);
    let reparsed = oxiarc_jpeg::TableSet::parse(&blob).expect("the blob must parse back");
    assert_eq!(reparsed.arithmetic, set.arithmetic);

    // A Huffman plan ignores the field, so an out-of-range value there is not
    // an error — narrowing that would be a gratuitous break.
    let options = EncodeOptions {
        entropy: EntropyCoding::Huffman,
        arithmetic: ArithmeticConditioning {
            dc: [0x0F; 4],
            ac: [200; 4],
        },
        ..Default::default()
    };
    assert!(encode_to_vec_with_options(&pixels, 16, 16, InputColor::Rgb, &options).is_ok());
}

// ── OJPEG: malformed tags and truncated strips ───────────────────────────

#[test]
fn truncating_an_ojpeg_interchange_stream_never_panics() {
    let stream = baseline_rgb(16, 16);
    let geometry = tiff::OJpegGeometry {
        width: 16,
        height: 16,
        samples_per_pixel: 3,
        photometric: 6,
        subsampling: (2, 2),
        ..Default::default()
    };
    let mut decoded = 0usize;
    for n in 0..=stream.len() {
        let tags = tiff::OJpegTags {
            jpeg_proc: Some(1),
            interchange: Some(&stream[..n]),
            ..Default::default()
        };
        let _ = tiff::reconstruct_ojpeg(&tags, &geometry, &[]);
        if tiff::decode_ojpeg(&tags, &geometry, &[], &DecodeOptions::raw()).is_ok() {
            decoded += 1;
        }
        // The same bytes as a *strip*, with the tags supplying nothing.
        let bare = tiff::OJpegTags {
            jpeg_proc: Some(1),
            ..Default::default()
        };
        let _ = tiff::reconstruct_ojpeg(&bare, &geometry, &stream[..n]);
        let _ = tiff::decode_ojpeg(&bare, &geometry, &stream[..n], &DecodeOptions::raw());
    }
    assert!(
        decoded > 0,
        "the sweep never reached a decodable prefix, so it proved nothing"
    );
}

#[test]
fn malformed_ojpeg_table_tags_are_named_errors() {
    let geometry = tiff::OJpegGeometry {
        width: 8,
        height: 8,
        ..Default::default()
    };
    let strip = vec![0x00u8; 16];

    // Tag 519 entries must be exactly 64 bytes.
    for length in [0usize, 1, 63, 65, 4096] {
        let tags = tiff::OJpegTags {
            q_tables: vec![vec![1u8; length]],
            dc_tables: vec![{
                let mut t = vec![0u8; 16];
                t[0] = 1;
                t.push(0);
                t
            }],
            ac_tables: vec![{
                let mut t = vec![0u8; 16];
                t[0] = 1;
                t.push(0);
                t
            }],
            ..Default::default()
        };
        assert!(
            tiff::reconstruct_ojpeg(&tags, &geometry, &strip).is_err(),
            "a {length}-byte quantisation table must be refused"
        );
    }

    // Tags 520/521 need sixteen BITS counts and that many values.
    for table in [vec![], vec![0u8; 15], {
        let mut t = vec![0u8; 16];
        t[0] = 5; // claims five values
        t.push(0);
        t
    }] {
        let tags = tiff::OJpegTags {
            q_tables: vec![vec![1u8; 64]],
            dc_tables: vec![table.clone()],
            ..Default::default()
        };
        assert!(tiff::reconstruct_ojpeg(&tags, &geometry, &strip).is_err());
        let tags = tiff::OJpegTags {
            q_tables: vec![vec![1u8; 64]],
            ac_tables: vec![table],
            ..Default::default()
        };
        assert!(tiff::reconstruct_ojpeg(&tags, &geometry, &strip).is_err());
    }

    // An unknown JPEGProc, and a zero-sized strip geometry.
    let tags = tiff::OJpegTags {
        jpeg_proc: Some(7),
        q_tables: vec![vec![1u8; 64]],
        ..Default::default()
    };
    assert!(tiff::reconstruct_ojpeg(&tags, &geometry, &strip).is_err());
    let zero = tiff::OJpegGeometry {
        width: 0,
        ..geometry
    };
    let tags = tiff::OJpegTags {
        q_tables: vec![vec![1u8; 64]],
        ..Default::default()
    };
    assert!(tiff::reconstruct_ojpeg(&tags, &zero, &strip).is_err());
    // An empty strip with no interchange stream has nothing to decode.
    assert!(tiff::reconstruct_ojpeg(&tags, &geometry, &[]).is_err());
}

// ── rayon: the band layout depends on the thread count ───────────────────

/// FNV-1a, so the digest needs no dependency.
#[cfg(feature = "rayon")]
fn digest(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x1000_0000_01b3);
    }
    hash
}

/// Fixtures large enough that `plan_bands` really splits them, at every
/// entropy coder the parallel path handles.
#[cfg(feature = "rayon")]
fn parallel_fixture_digests() -> Vec<(String, u64)> {
    let (width, height) = (320u16, 256u16);
    let pixels: Vec<u8> = (0..usize::from(width) * usize::from(height) * 3)
        .map(|i| ((i * 31 + i / 977) % 251) as u8)
        .collect();
    let mut out = Vec::new();
    let mut coders = vec![("huffman", EntropyCoding::Huffman)];
    if cfg!(feature = "arithmetic") {
        coders.push(("arithmetic", EntropyCoding::Arithmetic));
    }
    for (coder, entropy) in coders {
        for (label, subsampling) in [("420", Subsampling::S420), ("444", Subsampling::S444)] {
            for rows in [1u16, 2] {
                let options = EncodeOptions {
                    entropy,
                    subsampling,
                    restart_interval: RestartInterval::McuRows(rows),
                    ..Default::default()
                };
                let jpeg =
                    encode_to_vec_with_options(&pixels, width, height, InputColor::Rgb, &options)
                        .expect("encode");
                let mut decoder = Decoder::new(&jpeg[..]);
                decoder.read_info().expect("read_info");
                let samples = decoder.decode().expect("decode");
                out.push((
                    format!("{coder}/{label}/rst{rows}"),
                    digest(&jpeg) ^ digest(&samples).rotate_left(17),
                ));
            }
        }
    }
    // A height that is not a whole number of MCU rows, so the last band is
    // short and the merge has to clip it.
    let (width, height) = (320u16, 250u16);
    let pixels: Vec<u8> = (0..usize::from(width) * usize::from(height) * 3)
        .map(|i| ((i * 17 + 3) % 251) as u8)
        .collect();
    let options = EncodeOptions {
        restart_interval: RestartInterval::McuRows(1),
        ..Default::default()
    };
    let jpeg = encode_to_vec_with_options(&pixels, width, height, InputColor::Rgb, &options)
        .expect("encode");
    let mut decoder = Decoder::new(&jpeg[..]);
    decoder.read_info().expect("read_info");
    let samples = decoder.decode().expect("decode");
    out.push((
        "partial-last-band".to_string(),
        digest(&jpeg) ^ digest(&samples).rotate_left(17),
    ));
    out
}

/// The parallel decoder sizes its bands from `rayon::current_num_threads()`,
/// so the band layout — and therefore the merge — is a function of the thread
/// count. Everything else in the suite runs in one process with one pool and
/// cannot see that. This runs the same fixtures in child processes at six
/// thread counts and requires every digest to match.
///
/// Instrumented during development: two bands at one thread, four at three,
/// and eight, sixteen or thirty-two at seventeen — including one MCU row per
/// band, which no other test reaches.
#[cfg(feature = "rayon")]
#[test]
fn every_rayon_thread_count_decodes_to_the_same_samples() {
    const CHILD: &str = "OXIARC_JPEG_FVERIFY_THREADS_CHILD";
    if std::env::var_os(CHILD).is_some() {
        for (label, value) in parallel_fixture_digests() {
            println!("FVERIFY {label} {value:016x}");
        }
        return;
    }

    let exe = std::env::current_exe().expect("test binary");
    let mut reference: Option<Vec<(String, u64)>> = None;
    for threads in ["1", "2", "3", "5", "8", "17"] {
        let output = std::process::Command::new(&exe)
            .arg("--exact")
            .arg("every_rayon_thread_count_decodes_to_the_same_samples")
            .arg("--nocapture")
            .env(CHILD, "1")
            .env("RAYON_NUM_THREADS", threads)
            .output()
            .expect("spawn the test binary");
        assert!(
            output.status.success(),
            "child with RAYON_NUM_THREADS={threads} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let text = String::from_utf8_lossy(&output.stdout);
        let digests: Vec<(String, u64)> = text
            .lines()
            .filter_map(|line| line.strip_prefix("FVERIFY "))
            .filter_map(|line| {
                let (label, value) = line.split_once(' ')?;
                Some((label.to_string(), u64::from_str_radix(value, 16).ok()?))
            })
            .collect();
        assert!(
            !digests.is_empty(),
            "child with RAYON_NUM_THREADS={threads} printed no digest, so the \
             comparison would be vacuous; stdout was:\n{text}"
        );
        match &reference {
            None => reference = Some(digests),
            Some(expected) => assert_eq!(
                &digests, expected,
                "RAYON_NUM_THREADS={threads} changed the result"
            ),
        }
    }
}

// ── a few more shapes the ordinary suite does not build ──────────────────

/// A progressive script that names one component per scan across a frame with
/// mismatched sampling factors, decoded back through the coefficient buffer.
#[test]
fn a_non_interleaved_progressive_script_round_trips_at_awkward_sizes() {
    for (width, height) in [(1u16, 1u16), (1, 33), (33, 1), (17, 19), (65, 3)] {
        let pixels: Vec<u8> = (0..usize::from(width) * usize::from(height) * 3)
            .map(|i| (i % 251) as u8)
            .collect();
        let script = vec![
            ScanSpec::dc(vec![0, 1, 2], 0, 1),
            ScanSpec::ac(0, 1, 63, 0, 2),
            ScanSpec::ac(1, 1, 63, 0, 1),
            ScanSpec::ac(2, 1, 63, 0, 1),
            ScanSpec::dc(vec![0, 1, 2], 1, 0),
            ScanSpec::ac(0, 1, 63, 2, 1),
            ScanSpec::ac(0, 1, 63, 1, 0),
            ScanSpec::ac(1, 1, 63, 1, 0),
            ScanSpec::ac(2, 1, 63, 1, 0),
        ];
        let options = EncodeOptions {
            process: EncodeProcess::Progressive,
            progressive_script: Some(script),
            subsampling: Subsampling::S420,
            ..Default::default()
        };
        let jpeg = encode_to_vec_with_options(&pixels, width, height, InputColor::Rgb, &options)
            .unwrap_or_else(|e| panic!("{width}x{height}: {e}"));
        let mut decoder = Decoder::new(&jpeg[..]);
        let info = decoder.read_info().expect("read_info");
        assert_eq!((info.width, info.height), (width, height));
        let samples = decoder.decode().expect("decode");
        assert_eq!(
            samples.len(),
            usize::from(width) * usize::from(height) * 3,
            "{width}x{height}"
        );
    }
}

// ── restart resynchronisation on malformed data ──────────────────────────

/// Removing, renumbering or duplicating an `RSTn` must terminate promptly and
/// report, never spin. The arithmetic decoder is the interesting one: T.81
/// D.2.6 lets it read fabricated zeros past the end of the data, so a wrong
/// restart marker is one of the few conditions that can actually stop it.
#[test]
fn damaged_restart_markers_terminate_and_report() {
    let (width, height) = (128u16, 64u16);
    let pixels: Vec<u8> = (0..usize::from(width) * usize::from(height) * 3)
        .map(|i| ((i * 13) % 251) as u8)
        .collect();
    let mut coders = vec![EntropyCoding::Huffman];
    if cfg!(feature = "arithmetic") {
        coders.push(EntropyCoding::Arithmetic);
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    for entropy in coders {
        for process in [EncodeProcess::Sequential, EncodeProcess::Progressive] {
            let options = EncodeOptions {
                entropy,
                process,
                restart_interval: RestartInterval::McuRows(1),
                ..Default::default()
            };
            let jpeg =
                encode_to_vec_with_options(&pixels, width, height, InputColor::Rgb, &options)
                    .expect("encode");
            // Every `RSTn` in the stream, by offset of its `0xFF`.
            let markers: Vec<usize> = (0..jpeg.len().saturating_sub(1))
                .filter(|&i| jpeg[i] == 0xFF && (0xD0..=0xD7).contains(&jpeg[i + 1]))
                .collect();
            assert!(
                markers.len() >= 3,
                "the fixture must carry restart markers or the test proves nothing"
            );
            for &at in markers.iter().take(6) {
                // (a) the marker deleted outright.
                let mut damaged = jpeg.clone();
                damaged.drain(at..at + 2);
                let _ = decode_all(&damaged);
                // (b) renumbered to the wrong index.
                let mut damaged = jpeg.clone();
                damaged[at + 1] = 0xD0 + ((damaged[at + 1] - 0xD0 + 3) & 7);
                let _ = decode_all(&damaged);
                // (c) turned into a different marker entirely.
                let mut damaged = jpeg.clone();
                damaged[at + 1] = 0xDA;
                let _ = decode_all(&damaged);
                // (d) duplicated.
                let mut damaged = jpeg.clone();
                let pair = [damaged[at], damaged[at + 1]];
                damaged.splice(at..at, pair);
                let _ = decode_all(&damaged);
                // (e) preceded by a run of fill bytes, which is legal.
                let mut damaged = jpeg.clone();
                damaged.splice(at..at, [0xFFu8; 5]);
                let _ = decode_all(&damaged);
                assert!(
                    std::time::Instant::now() < deadline,
                    "the sweep exceeded its wall-clock budget"
                );
            }
        }
    }
}

/// Decode at `numerator/8` (strictly), discarding the outcome. A companion
/// to [`decode_all`] for [`inserting_and_deleting_bytes_never_panics`]: that
/// sweep's whole point is byte insertion/deletion, which shifts every later
/// segment length and offset — exactly the damage class most likely to
/// desynchronise the per-component `output_size` this track's plane
/// geometry now carries from the frame the entropy decoder actually walks.
fn decode_scaled(data: &[u8], numerator: u8) {
    let options = DecodeOptions {
        scale: Scale::new(numerator).expect("1..=16"),
        ..DecodeOptions::default()
    };
    let mut decoder = Decoder::with_options(data, options);
    if decoder.read_info().is_ok() {
        let _ = decoder.decode();
    }
}

/// Decode both strictly and tolerantly, discarding the outcome.
fn decode_all(data: &[u8]) -> bool {
    let mut ok = false;
    let mut decoder = Decoder::new(data);
    if decoder.read_info().is_ok() && decoder.decode().is_ok() {
        ok = true;
    }
    let tolerant = DecodeOptions {
        tolerate_truncated: true,
        ..DecodeOptions::default()
    };
    let mut decoder = Decoder::with_options(data, tolerant);
    if decoder.read_info().is_ok() {
        let _ = decoder.decode();
    }
    ok
}

/// A legal fill-byte run before every `RSTn` must decode to exactly the same
/// samples as the stream without it (T.81 B.1.1.2).
#[test]
fn fill_bytes_before_a_restart_marker_change_nothing() {
    let (width, height) = (64u16, 48u16);
    let pixels: Vec<u8> = (0..usize::from(width) * usize::from(height) * 3)
        .map(|i| ((i * 29) % 251) as u8)
        .collect();
    let mut coders = vec![EntropyCoding::Huffman];
    if cfg!(feature = "arithmetic") {
        coders.push(EntropyCoding::Arithmetic);
    }
    for entropy in coders {
        let options = EncodeOptions {
            entropy,
            restart_interval: RestartInterval::McuRows(1),
            ..Default::default()
        };
        let jpeg = encode_to_vec_with_options(&pixels, width, height, InputColor::Rgb, &options)
            .expect("encode");
        let mut decoder = Decoder::new(&jpeg[..]);
        decoder.read_info().expect("read_info");
        let reference = decoder.decode().expect("decode");

        let mut padded = Vec::with_capacity(jpeg.len() * 2);
        let mut index = 0usize;
        let mut inserted = 0usize;
        while index < jpeg.len() {
            if index + 1 < jpeg.len()
                && jpeg[index] == 0xFF
                && (0xD0..=0xD7).contains(&jpeg[index + 1])
            {
                padded.extend_from_slice(&[0xFF; 4]);
                inserted += 1;
            }
            padded.push(jpeg[index]);
            index += 1;
        }
        assert!(inserted >= 2, "no restart marker was padded");
        let mut decoder = Decoder::new(&padded[..]);
        decoder.read_info().expect("read_info");
        let samples = decoder.decode().expect("decode with fill bytes");
        assert_eq!(
            samples, reference,
            "{entropy:?}: fill bytes changed the image"
        );
    }
}

// ── byte insertion and deletion, the class `corrupt_no_panic` omits ──────

/// A deterministic xorshift, so a failure is reproducible from the seed.
struct Xorshift(u64);

impl Xorshift {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            (self.next() % n as u64) as usize
        }
    }
}

/// `tests/corrupt_no_panic.rs` sweeps truncation, single-byte corruption and
/// segment lengths, all of which preserve the *length* of everything after
/// the damage. Inserting or deleting a byte shifts every later offset, which
/// is what turns a well-formed segment into one whose length field points
/// into the middle of the next, and is the shape neither that file nor
/// `tests/fuzz_seeds.rs` produces.
///
/// The count is fixed rather than timed so a failure is reproducible; the
/// wall-clock assertion only catches a hang. Set
/// `OXIARC_JPEG_ADVERSARIAL_ITERATIONS` to run a longer sweep by hand.
#[test]
fn inserting_and_deleting_bytes_never_panics() {
    let iterations: usize = std::env::var("OXIARC_JPEG_ADVERSARIAL_ITERATIONS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(60_000);
    let mut fixtures: Vec<Vec<u8>> = vec![
        oxiarc_jpeg::sample::GRAY_1X1.to_vec(),
        oxiarc_jpeg::sample::GRAY_1X1_TABLES.to_vec(),
        oxiarc_jpeg::sample::GRAY_1X1_SCAN.to_vec(),
        oxiarc_jpeg::sample::RGB_8X8_420.to_vec(),
        oxiarc_jpeg::sample::RGB_8X8_420_TABLES.to_vec(),
        oxiarc_jpeg::sample::RGB_8X8_420_SCAN.to_vec(),
    ];
    for name in [
        "progressive_2x2.jpg",
        "progressive_restart.jpg",
        "restart_2x2.jpg",
        "lossless_rgb.jpg",
        "twelve_bit_gray.jpg",
    ] {
        let path = format!("{}/tests/data/{name}", env!("CARGO_MANIFEST_DIR"));
        fixtures.push(std::fs::read(&path).unwrap_or_else(|e| panic!("{path}: {e}")));
    }
    // An arithmetic stream too, built here rather than committed.
    #[cfg(feature = "arithmetic")]
    {
        let pixels = vec![91u8; 32 * 32 * 3];
        let options = EncodeOptions {
            entropy: EntropyCoding::Arithmetic,
            restart_interval: RestartInterval::McuRows(1),
            ..Default::default()
        };
        fixtures.push(
            encode_to_vec_with_options(&pixels, 32, 32, InputColor::Rgb, &options)
                .expect("arithmetic fixture"),
        );
    }
    assert!(fixtures.len() >= 11, "the corpus lost a fixture");

    let start = std::time::Instant::now();
    let mut rng = Xorshift(0x0123_4567_89ab_cdef);
    for _ in 0..iterations {
        let base = &fixtures[rng.below(fixtures.len())];
        let mut data = base.clone();
        for _ in 0..1 + rng.below(4) {
            if data.is_empty() {
                break;
            }
            let at = rng.below(data.len());
            match rng.below(4) {
                0 => {
                    data.remove(at);
                }
                1 => data.insert(at, rng.next() as u8),
                2 => data.insert(at, 0xFF),
                _ => {
                    let run = (1 + rng.below(8)).min(data.len() - at);
                    data.drain(at..at + run);
                }
            }
        }
        let _ = decode_all(&data);
        // Cycle through an exact-kernel, a component-bump, a general-kernel
        // and the unscaled case rather than trying every numerator on every
        // one of the (up to 60 000, by default) iterations.
        decode_scaled(&data, [1u8, 2, 5, 8][rng.below(4)]);
        let mut out = [0u8; 4096];
        let _ = oxiarc_jpeg::TableSet::parse(&data).map(|tables| {
            oxiarc_jpeg::decode_abbreviated_into(
                Some(&tables),
                &data,
                &DecodeOptions::raw(),
                &mut out,
            )
        });
        let _ = tiff::parse_jpeg_tables(&data);
        let _ = tiff::merge_jpeg_tables(&oxiarc_jpeg::sample::RGB_8X8_420_TABLES, &data);
    }
    assert!(
        start.elapsed() < std::time::Duration::from_secs(300),
        "the sweep exceeded its wall-clock budget"
    );
}
