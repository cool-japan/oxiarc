//! Property tests over the table machinery and the decoder's entry points.

use oxiarc_jpeg::{
    DecodeLimits, DecodeOptions, Decoder, HuffmanTable, Marker, QuantTable, TableSet, TablesMode,
    decode_abbreviated_into, quality_scaling_factor, sample, tiff,
};
use proptest::prelude::*;

/// Normalise 16 arbitrary counts into a `BITS` list that satisfies the Kraft
/// inequality, so `HuffmanTable::new` is guaranteed to accept it.
fn normalise_bits(raw: [u8; 16]) -> ([u8; 16], usize) {
    let mut bits = [0u8; 16];
    let mut capacity: u32 = 2;
    let mut total = 0usize;
    for (length, slot) in bits.iter_mut().enumerate() {
        let want = u32::from(raw[length]);
        let take = want.min(capacity).min(255);
        *slot = take as u8;
        total += take as usize;
        capacity = (capacity - take) * 2;
        if capacity == 0 {
            break;
        }
        capacity = capacity.min(1 << 16);
    }
    (bits, total)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// Any byte string is decoded or rejected, never a panic.
    #[test]
    fn arbitrary_bytes_never_panic(data in proptest::collection::vec(any::<u8>(), 0..600)) {
        let options = DecodeOptions {
            limits: DecodeLimits::strict(),
            ..DecodeOptions::default()
        };
        let mut decoder = Decoder::with_options(data.as_slice(), options);
        if decoder.read_info().is_ok() {
            let _ = decoder.decode();
        }
        let _ = TableSet::parse(&data);
        let _ = tiff::parse_jpeg_tables(&data);
    }

    /// An arbitrary split of an arbitrary buffer into tables and scan is safe.
    #[test]
    fn arbitrary_abbreviated_splits_never_panic(
        data in proptest::collection::vec(any::<u8>(), 0..400),
        split in 0usize..400,
    ) {
        let split = split.min(data.len());
        let tables = TableSet::parse(&data[..split]).unwrap_or_default();
        let mut out = [0u8; 512];
        let _ = decode_abbreviated_into(
            Some(&tables),
            &data[split..],
            &DecodeOptions::strict(),
            &mut out,
        );
    }

    /// Building a canonical table from a Kraft-valid `BITS` always succeeds,
    /// and a `DHT` round-trip reproduces it exactly.
    #[test]
    fn canonical_tables_round_trip(raw in any::<[u8; 16]>(), seed in any::<u8>()) {
        let (bits, total) = normalise_bits(raw);
        let values: Vec<u8> = (0..total).map(|i| (i as u8).wrapping_add(seed)).collect();
        let table = HuffmanTable::new(bits, values.clone()).expect("Kraft-valid table");
        prop_assert_eq!(table.bits(), &bits);
        prop_assert_eq!(table.values(), values.as_slice());

        let mut set = TableSet::default();
        set.dc_huffman[0] = Some(table.clone());
        set.ac_huffman[3] = Some(table.clone());
        let blob = set.emit(TablesMode::HUFF);
        let reparsed = TableSet::parse(&blob).expect("re-parse");
        prop_assert_eq!(reparsed.dc_huffman[0].as_ref(), Some(&table));
        prop_assert_eq!(reparsed.ac_huffman[3].as_ref(), Some(&table));
        prop_assert_eq!(reparsed.emit(TablesMode::HUFF), blob);
    }

    /// Quality scaling is monotone and stays inside the baseline range.
    #[test]
    fn quant_scaling_is_monotone_and_bounded(quality in 1u8..=100) {
        let table = QuantTable::annex_k_luma().scaled_for_quality(quality, true);
        prop_assert!(table.natural().iter().all(|&v| (1..=255).contains(&v)));
        if quality < 100 {
            let next = QuantTable::annex_k_luma().scaled_for_quality(quality + 1, true);
            for (a, b) in table.natural().iter().zip(next.natural().iter()) {
                prop_assert!(a >= b, "quality {quality}: {a} < {b}");
            }
            prop_assert!(
                quality_scaling_factor(quality) >= quality_scaling_factor(quality + 1)
            );
        }
    }

    /// A `DQT` round-trip is exact for any set of legal quantiser values.
    #[test]
    fn quant_tables_round_trip(values in proptest::array::uniform32(1u16..=32_767)) {
        let mut natural = [1u16; 64];
        natural[..32].copy_from_slice(&values);
        let table = QuantTable::from_natural(natural);
        let mut set = TableSet::default();
        set.quant[2] = Some(table);
        let blob = set.emit(TablesMode::QUANT);
        let reparsed = TableSet::parse(&blob).expect("re-parse");
        let recovered = reparsed.quant[2].expect("table");
        prop_assert_eq!(recovered.natural(), &natural);
    }

    /// Every marker code round-trips through the enum.
    #[test]
    fn marker_codes_round_trip(code in 1u8..=254) {
        if let Some(marker) = Marker::from_code(code) {
            prop_assert_eq!(marker.code(), code);
            prop_assert_eq!(marker.as_u16(), 0xFF00 | u16::from(code));
        }
    }

    /// A strided decode writes the same rows as a packed one, whatever the
    /// stride, and never touches the padding.
    #[test]
    fn strided_decode_matches_packed(extra in 0usize..64) {
        let mut decoder = Decoder::new(&sample::RGB_8X8_420[..]);
        decoder.read_info().expect("info");
        let packed = decoder.decode().expect("decode");

        let width = 8 * 3;
        let stride = width + extra;
        let mut canvas = vec![0x5Au8; stride * 8];
        let mut decoder = Decoder::new(&sample::RGB_8X8_420[..]);
        decoder.read_info().expect("info");
        decoder.decode_into_strided(&mut canvas, stride).expect("strided");
        for y in 0..8 {
            prop_assert_eq!(
                &canvas[y * stride..y * stride + width],
                &packed[y * width..y * width + width]
            );
            if y + 1 < 8 {
                prop_assert!(
                    canvas[y * stride + width..(y + 1) * stride]
                        .iter()
                        .all(|&b| b == 0x5A)
                );
            }
        }
    }

    /// Merging tables into a strip and loading them out of band agree.
    #[test]
    fn merge_and_load_tables_agree(mode_bits in 0u16..4) {
        let tables = TableSet::parse(&sample::RGB_8X8_420_TABLES).expect("tables");
        let blob = tables.emit(TablesMode::from_bits(mode_bits));
        let merged = tiff::merge_jpeg_tables(&blob, &sample::RGB_8X8_420_SCAN).expect("merge");

        let reparsed = TableSet::parse(&blob).expect("reparse");
        let mut via_tables = vec![0u8; 8 * 8 * 3];
        let by_tables = decode_abbreviated_into(
            Some(&reparsed),
            &sample::RGB_8X8_420_SCAN,
            &DecodeOptions::default(),
            &mut via_tables,
        );
        let mut via_merge = vec![0u8; 8 * 8 * 3];
        let by_merge = decode_abbreviated_into(
            None,
            &merged,
            &DecodeOptions::default(),
            &mut via_merge,
        );
        prop_assert_eq!(by_tables.is_ok(), by_merge.is_ok());
        if by_tables.is_ok() {
            prop_assert_eq!(via_tables, via_merge);
        }
    }
}
