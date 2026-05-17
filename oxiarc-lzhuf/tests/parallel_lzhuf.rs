//! Integration tests for parallel LHA archive compression.
//!
//! These tests are gated behind `#[cfg(feature = "parallel")]` — they only
//! compile and run when the crate is built with `--features parallel`.

#[cfg(feature = "parallel")]
mod tests {
    use oxiarc_lzhuf::{
        LzhEntryInput, LzhMethod, ParallelLzhBuilder, decode_lzh, lzh_compress_parallel,
    };

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    /// Generate `n` entries, each with `size` bytes of pseudo-random-ish data.
    fn make_entries_owned(n: usize, size: usize) -> Vec<(String, Vec<u8>)> {
        (0..n)
            .map(|i| {
                let name = format!("entry_{i:03}.bin");
                // Produce repeating-pattern data with some variety per entry.
                let data: Vec<u8> = (0..size)
                    .map(|j| ((j.wrapping_mul(7) ^ i.wrapping_mul(13)) & 0xFF) as u8)
                    .collect();
                (name, data)
            })
            .collect()
    }

    /// Parse a level-1 LZH archive from raw bytes.
    ///
    /// Returns `Vec<(method_id, compressed_size, original_size, crc16, compressed_payload)>`.
    ///
    /// This minimal parser handles only the level-1 header format produced by
    /// `lzh_compress_parallel` — it is *not* a general-purpose LZH parser.
    ///
    /// Level-1 header byte layout (all offsets are absolute from `pos`):
    /// ```text
    ///  [0]         header_size_byte  u8  = (total_header - 2), i.e. counts bytes [2..end_of_header]
    ///  [1]         checksum          u8
    ///  [2..7]      method id         5 bytes
    ///  [7..11]     compressed_size   u32 LE
    ///  [11..15]    original_size     u32 LE
    ///  [15..19]    mtime             u32 LE
    ///  [19]        attribute         u8
    ///  [20]        level             u8 = 1
    ///  [21]        fname_len         u8
    ///  [22..22+F]  filename          F bytes
    ///  [22+F..+2]  crc16             u16 LE
    ///  [24+F]      os_id             u8
    ///  [25+F..+2]  next_ext_size     u16 LE = 0
    /// ```
    ///
    /// Total header size = 27 + fname_len bytes.
    /// header_size_byte  = 25 + fname_len  (= total - 2).
    /// NOTE: header_size_byte counts bytes [2..(2 + 25 + fname_len)] = 25 + fname_len.
    fn parse_lzh_archive(archive: &[u8]) -> Vec<(String, u32, u32, u16, Vec<u8>)> {
        let mut pos = 0usize;
        let mut members = Vec::new();

        loop {
            if pos >= archive.len() {
                break;
            }

            // End-of-archive marker: header_size == 0
            let header_size_byte = archive[pos];
            if header_size_byte == 0 {
                break;
            }

            assert!(
                archive.len() > pos + 21,
                "archive truncated before header fields"
            );

            let method_bytes = &archive[pos + 2..pos + 7];
            let method_id = String::from_utf8_lossy(method_bytes).into_owned();

            let compressed_size = u32::from_le_bytes([
                archive[pos + 7],
                archive[pos + 8],
                archive[pos + 9],
                archive[pos + 10],
            ]);
            let original_size = u32::from_le_bytes([
                archive[pos + 11],
                archive[pos + 12],
                archive[pos + 13],
                archive[pos + 14],
            ]);

            let fname_len = archive[pos + 21] as usize;

            let crc16_offset = pos + 22 + fname_len;
            assert!(
                archive.len() > crc16_offset + 1,
                "archive truncated before crc16"
            );
            let crc16 = u16::from_le_bytes([archive[crc16_offset], archive[crc16_offset + 1]]);

            // Full header = 27 + fname_len bytes; data follows immediately.
            let header_total = 27 + fname_len;
            let data_start = pos + header_total;
            let data_end = data_start + compressed_size as usize;

            assert!(
                archive.len() >= data_end,
                "archive truncated before data end (expected {data_end}, have {}, fname_len={fname_len}, compressed_size={compressed_size})",
                archive.len()
            );

            let payload = archive[data_start..data_end].to_vec();
            members.push((method_id, compressed_size, original_size, crc16, payload));

            pos = data_end;
        }

        members
    }

    // -----------------------------------------------------------------------
    // Tests
    // -----------------------------------------------------------------------

    /// Three entries compressed with Lh5; decode every member and assert
    /// byte-identical to originals.
    #[test]
    fn test_parallel_basic() {
        let owned = make_entries_owned(3, 512);
        let entries: Vec<LzhEntryInput<'_>> = owned
            .iter()
            .map(|(n, d)| LzhEntryInput { name: n, data: d })
            .collect();

        let archive =
            lzh_compress_parallel(&entries, LzhMethod::Lh5).expect("parallel compress failed");

        let members = parse_lzh_archive(&archive);
        assert_eq!(members.len(), 3, "expected 3 members");

        for (i, (method_id, _comp_size, orig_size, _crc16, payload)) in members.iter().enumerate() {
            assert_eq!(method_id, "-lh5-", "unexpected method id for entry {i}");
            let method = LzhMethod::from_id(method_id.as_bytes())
                .expect("failed to parse LzhMethod from id");
            let decompressed =
                decode_lzh(payload, method, *orig_size as u64).expect("decompression failed");
            assert_eq!(
                decompressed, owned[i].1,
                "decoded entry {i} does not match original"
            );
        }

        // Archive must end with 0x00 terminator.
        assert_eq!(
            archive.last().copied(),
            Some(0x00),
            "missing archive terminator"
        );
    }

    /// Same input → byte-identical output across two calls.
    #[test]
    fn test_parallel_determinism() {
        let owned = make_entries_owned(4, 256);
        let entries: Vec<LzhEntryInput<'_>> = owned
            .iter()
            .map(|(n, d)| LzhEntryInput { name: n, data: d })
            .collect();

        let a1 = lzh_compress_parallel(&entries, LzhMethod::Lh5).expect("first compress failed");
        let a2 = lzh_compress_parallel(&entries, LzhMethod::Lh5).expect("second compress failed");

        assert_eq!(a1, a2, "parallel compression must be deterministic");
    }

    /// Round-trip for every supported method: lh0, lh4, lh5, lh6, lh7.
    #[test]
    fn test_parallel_methods() {
        for method in [
            LzhMethod::Lh0,
            LzhMethod::Lh4,
            LzhMethod::Lh5,
            LzhMethod::Lh6,
            LzhMethod::Lh7,
        ] {
            let owned = make_entries_owned(3, 128);
            // Throw in one slightly larger entry for good measure.
            let large_name = "large.bin".to_string();
            let large_data: Vec<u8> = (0u8..=255).cycle().take(2048).collect();
            let mut all_owned = owned;
            all_owned.push((large_name, large_data));

            let entries: Vec<LzhEntryInput<'_>> = all_owned
                .iter()
                .map(|(n, d)| LzhEntryInput { name: n, data: d })
                .collect();

            let archive = lzh_compress_parallel(&entries, method)
                .unwrap_or_else(|e| panic!("parallel compress failed for {method:?}: {e}"));

            let members = parse_lzh_archive(&archive);
            assert_eq!(
                members.len(),
                all_owned.len(),
                "member count mismatch for {method:?}"
            );

            for (i, (_method_id, _comp_size, orig_size, _crc16, payload)) in
                members.iter().enumerate()
            {
                let decompressed =
                    decode_lzh(payload, method, *orig_size as u64).unwrap_or_else(|e| {
                        panic!("decompression failed for {method:?} entry {i}: {e}")
                    });
                assert_eq!(
                    decompressed, all_owned[i].1,
                    "roundtrip failed for {method:?} entry {i}"
                );
            }
        }
    }

    /// One-entry archive: compressed payload must be byte-identical to
    /// what the serial `encode_lzh` call produces for the same input.
    #[test]
    fn test_parallel_single_entry() {
        use oxiarc_lzhuf::encode_lzh;

        let data: Vec<u8> = b"hello parallel single entry world"
            .iter()
            .cycle()
            .take(256)
            .copied()
            .collect();
        let entries = vec![LzhEntryInput {
            name: "single.txt",
            data: &data,
        }];

        let archive = lzh_compress_parallel(&entries, LzhMethod::Lh5).expect("compress failed");

        let members = parse_lzh_archive(&archive);
        assert_eq!(members.len(), 1, "expected exactly 1 member");

        // Extract the compressed payload from the archive member.
        let payload = &members[0].4;

        // Compare with the output of the serial encoder.
        let serial_compressed = encode_lzh(&data, LzhMethod::Lh5).expect("serial encode failed");

        assert_eq!(
            payload, &serial_compressed,
            "parallel payload must match serial encode_lzh output"
        );
    }

    /// Empty entries slice → output is just the `0x00` terminator.
    #[test]
    fn test_parallel_empty_archive() {
        let archive = lzh_compress_parallel(&[], LzhMethod::Lh5).expect("empty compress failed");

        assert_eq!(
            archive,
            vec![0x00u8],
            "empty archive must be a single 0x00 terminator byte"
        );
    }

    /// ParallelLzhBuilder API round-trip smoke test.
    #[test]
    fn test_parallel_builder_api() {
        let owned = make_entries_owned(2, 128);
        let entries: Vec<LzhEntryInput<'_>> = owned
            .iter()
            .map(|(n, d)| LzhEntryInput { name: n, data: d })
            .collect();

        let archive = ParallelLzhBuilder::new(LzhMethod::Lh5)
            .with_num_threads(2)
            .build(&entries)
            .expect("builder build failed");

        let members = parse_lzh_archive(&archive);
        assert_eq!(members.len(), 2);
    }

    /// Overlong filename returns an error, not a panic.
    #[test]
    fn test_parallel_overlong_filename_error() {
        let long_name: String = "a".repeat(256);
        let entries = vec![LzhEntryInput {
            name: &long_name,
            data: b"payload",
        }];
        let result = lzh_compress_parallel(&entries, LzhMethod::Lh5);
        assert!(
            result.is_err(),
            "expected Err for overlong filename but got Ok"
        );
    }
}
