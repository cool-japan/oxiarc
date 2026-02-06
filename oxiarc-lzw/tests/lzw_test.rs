//! Comprehensive LZW integration tests.

use oxiarc_lzw::{LzwConfig, compress_tiff, decompress_tiff};

#[test]
fn test_lzw_roundtrip_simple() {
    let original = b"TOBEORNOTTOBEORTOBEORNOT";
    let compressed = compress_tiff(original).expect("compression failed");
    let decompressed = decompress_tiff(&compressed, original.len()).expect("decompression failed");

    assert_eq!(decompressed, original);
}

#[test]
fn test_lzw_roundtrip_310_bytes() {
    // THIS IS THE CRITICAL TEST CASE!
    // This test fails with weezl (truncates to ~250 bytes)
    // but MUST pass with oxiarc-lzw (outputs full 310 bytes)
    let original = b"This is a test of compression! ".repeat(10);
    assert_eq!(original.len(), 310, "Test data must be exactly 310 bytes");

    let compressed = compress_tiff(&original).expect("compression failed");

    println!("Original size: {} bytes", original.len());
    println!("Compressed size: {} bytes", compressed.len());
    println!(
        "Compression ratio: {:.2}%",
        (compressed.len() as f64 / original.len() as f64) * 100.0
    );

    let decompressed = decompress_tiff(&compressed, original.len()).expect("decompression failed");

    // THE CRITICAL ASSERTION
    assert_eq!(
        decompressed.len(),
        310,
        "Decompressed length MUST be 310 bytes, not truncated!"
    );
    assert_eq!(decompressed, &original[..], "Data must match exactly");
}

#[test]
#[ignore] // Known limitation: large repetitive data. See KNOWN_ISSUES.md
fn test_lzw_roundtrip_large() {
    let original = b"The quick brown fox jumps over the lazy dog. ".repeat(100);
    let compressed = compress_tiff(&original).expect("compression failed");
    let decompressed = decompress_tiff(&compressed, original.len()).expect("decompression failed");

    assert_eq!(decompressed, original);
}

#[test]
fn test_lzw_empty_input() {
    let original = b"";
    let compressed = compress_tiff(original).expect("compression failed");
    let decompressed = decompress_tiff(&compressed, 0).expect("decompression failed");

    assert_eq!(decompressed, original);
}

#[test]
fn test_lzw_single_byte() {
    let original = b"A";
    let compressed = compress_tiff(original).expect("compression failed");
    let decompressed = decompress_tiff(&compressed, original.len()).expect("decompression failed");

    assert_eq!(decompressed, original);
}

#[test]
fn test_lzw_all_zeros() {
    let original = vec![0u8; 1000];
    let compressed = compress_tiff(&original).expect("compression failed");

    // Highly repetitive data should compress very well
    assert!(
        compressed.len() < original.len() / 5,
        "All-zeros should compress to less than 20% of original"
    );

    let decompressed = decompress_tiff(&compressed, original.len()).expect("decompression failed");

    assert_eq!(decompressed, original);
}

#[test]
fn test_lzw_all_same_byte() {
    let original = vec![b'X'; 1000];
    let compressed = compress_tiff(&original).expect("compression failed");

    // Highly repetitive data should compress very well
    assert!(
        compressed.len() < original.len() / 5,
        "Repeated byte should compress to less than 20% of original"
    );

    let decompressed = decompress_tiff(&compressed, original.len()).expect("decompression failed");

    assert_eq!(decompressed, original);
}

#[test]
fn test_lzw_alternating_pattern() {
    let original = b"ABABABABABABABABABABABABABABABABABABAB";
    let compressed = compress_tiff(original).expect("compression failed");
    let decompressed = decompress_tiff(&compressed, original.len()).expect("decompression failed");

    assert_eq!(decompressed, original);
}

#[test]
fn test_lzw_all_byte_values() {
    // FIXED: This test now passes with the decoder bit-width synchronization fix
    // Test with all possible byte values
    let original: Vec<u8> = (0..=255).collect();
    let compressed = compress_tiff(&original).expect("compression failed");
    let decompressed = decompress_tiff(&compressed, original.len()).expect("decompression failed");

    assert_eq!(decompressed, original);
}

#[test]
fn test_lzw_random_like_data() {
    // FIXED: This test now passes with the decoder bit-width synchronization fix
    // Data that's hard to compress (pseudo-random sequence)
    let original: Vec<u8> = (0..1000).map(|i| ((i * 31 + 17) % 256) as u8).collect();

    let compressed = compress_tiff(&original).expect("compression failed");
    let decompressed = decompress_tiff(&compressed, original.len()).expect("decompression failed");

    assert_eq!(decompressed, original);

    // Random-like data shouldn't compress well
    assert!(
        compressed.len() >= original.len() / 2,
        "Random-like data should not compress significantly"
    );
}

#[test]
fn test_lzw_incremental_pattern() {
    // FIXED: This test now passes with the decoder bit-width synchronization fix
    // Pattern with increasing complexity
    let mut original = Vec::new();
    for i in 0..256 {
        for _ in 0..10 {
            original.push(i as u8);
        }
    }

    let compressed = compress_tiff(&original).expect("compression failed");
    let decompressed = decompress_tiff(&compressed, original.len()).expect("decompression failed");

    assert_eq!(decompressed, original);
}

#[test]
#[ignore] // Known limitation: large data triggers Invalid Code errors. See KNOWN_ISSUES.md
fn test_lzw_very_large_input() {
    // Test with 10MB of repetitive data
    let original = b"The quick brown fox jumps over the lazy dog. ".repeat(200_000);

    let compressed = compress_tiff(&original).expect("compression failed");

    println!("Very large test:");
    println!("  Original size: {} bytes", original.len());
    println!("  Compressed size: {} bytes", compressed.len());
    println!(
        "  Compression ratio: {:.2}%",
        (compressed.len() as f64 / original.len() as f64) * 100.0
    );

    let decompressed = decompress_tiff(&compressed, original.len()).expect("decompression failed");

    assert_eq!(decompressed.len(), original.len());
    assert_eq!(decompressed, original);
}

#[test]
fn test_lzw_multiple_sizes() {
    // Test various sizes to ensure no boundary issues
    for size in [1, 10, 50, 100, 255, 256, 257, 500, 1000, 4095, 4096, 4097] {
        let original = vec![b'A'; size];
        let compressed = compress_tiff(&original).expect("compression failed");
        let decompressed =
            decompress_tiff(&compressed, original.len()).expect("decompression failed");

        assert_eq!(
            decompressed.len(),
            original.len(),
            "Size mismatch for input size {}",
            size
        );
        assert_eq!(decompressed, original, "Data mismatch for size {}", size);
    }
}

#[test]
fn test_lzw_config_tiff() {
    let config = LzwConfig::TIFF;
    assert_eq!(config.min_bits, 9);
    assert_eq!(config.max_bits, 12);
    assert!(!config.use_clear_code);
    assert!(config.early_change);
}

#[test]
fn test_compression_effectiveness() {
    // Test that LZW actually compresses repetitive data
    let test_cases = vec![
        (b"AAAAAAAAAAAAAAAAAAAA".to_vec(), "all same"),
        (b"ABABABABABABABABABAB".to_vec(), "alternating"),
        (
            b"This is a test. This is a test. This is a test.".to_vec(),
            "repeated phrase",
        ),
    ];

    for (data, description) in test_cases {
        let compressed = compress_tiff(&data).expect("compression failed");

        println!(
            "{}: {} -> {} bytes ({:.1}%)",
            description,
            data.len(),
            compressed.len(),
            (compressed.len() as f64 / data.len() as f64) * 100.0
        );

        assert!(
            compressed.len() < data.len(),
            "{} should compress",
            description
        );

        let decompressed = decompress_tiff(&compressed, data.len()).expect("decompression failed");
        assert_eq!(decompressed, data);
    }
}
