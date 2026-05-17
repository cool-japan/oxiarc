//! Integration tests for LZMA bounded-memory streaming.

use oxiarc_lzma::{Error, LzmaCompressor, LzmaDecompressor, lzma2_compress, lzma2_decompress};

// ─── compress succeeds within budget ─────────────────────────────────────────

#[test]
fn test_budget_compress_succeeds() {
    let input = vec![0x42u8; 4 * 1024 * 1024]; // 4 MiB
    let compressor = LzmaCompressor::new().with_memory_budget(64 * 1024 * 1024);
    let result = compressor.compress(&input);
    assert!(
        result.is_ok(),
        "compress should succeed within 64 MiB budget"
    );
}

// ─── compress rejected when budget is too small for the dictionary ────────────

#[test]
fn test_budget_compress_exceeds() {
    let input = vec![0x42u8; 4 * 1024 * 1024]; // 4 MiB
    // 64 KiB budget is smaller than the 8 MiB dict for level 6 alone
    let compressor = LzmaCompressor::new().with_memory_budget(64 * 1024);
    let result = compressor.compress(&input);
    assert!(
        matches!(result, Err(Error::MemoryBudgetExceeded { .. })),
        "expected MemoryBudgetExceeded, got: {:?}",
        result
    );
}

// ─── decompress succeeds within budget ───────────────────────────────────────

#[test]
fn test_budget_decompress_succeeds() {
    let input = vec![0xABu8; 512 * 1024];
    let compressed = lzma2_compress(&input, 1).expect("compress");
    let decompressor = LzmaDecompressor::new().with_memory_budget(64 * 1024 * 1024);
    let result = decompressor.decompress(&compressed);
    assert_eq!(result.expect("decompress"), input);
}

// ─── roundtrip at level 1 ────────────────────────────────────────────────────

#[test]
fn test_budget_roundtrip_level_1() {
    let input: Vec<u8> = (0u32..1024 * 1024).map(|i| (i % 251) as u8).collect();
    let budget = 64 * 1024 * 1024;
    let compressor =
        LzmaCompressor::with_level(oxiarc_lzma::LzmaLevel::new(1)).with_memory_budget(budget);
    let compressed = compressor.compress(&input).expect("compress");
    let decompressor =
        LzmaDecompressor::with_level(oxiarc_lzma::LzmaLevel::new(1)).with_memory_budget(budget);
    let decompressed = decompressor.decompress(&compressed).expect("decompress");
    assert_eq!(decompressed, input);
}

// ─── roundtrip at level 5 ────────────────────────────────────────────────────

#[test]
fn test_budget_roundtrip_level_5() {
    let input: Vec<u8> = (0u32..2 * 1024 * 1024).map(|i| (i % 199) as u8).collect();
    let budget = 64 * 1024 * 1024;
    let compressor =
        LzmaCompressor::with_level(oxiarc_lzma::LzmaLevel::new(5)).with_memory_budget(budget);
    let compressed = compressor.compress(&input).expect("compress");
    let decompressor =
        LzmaDecompressor::with_level(oxiarc_lzma::LzmaLevel::new(5)).with_memory_budget(budget);
    let decompressed = decompressor.decompress(&compressed).expect("decompress");
    assert_eq!(decompressed, input);
}

// ─── roundtrip at level 9 (small input to keep test fast) ────────────────────
//
// Level 9 uses a 64 MiB dictionary so the budget must exceed 64 MiB +
// scratch_overhead + input_size.  128 MiB is sufficient.

#[test]
fn test_budget_roundtrip_level_9() {
    let input: Vec<u8> = (0u32..512 * 1024).map(|i| (i % 127) as u8).collect();
    let budget = 128 * 1024 * 1024; // 128 MiB — accommodates 64 MiB dict + overhead
    let compressor =
        LzmaCompressor::with_level(oxiarc_lzma::LzmaLevel::new(9)).with_memory_budget(budget);
    let compressed = compressor.compress(&input).expect("compress");
    let decompressor =
        LzmaDecompressor::with_level(oxiarc_lzma::LzmaLevel::new(9)).with_memory_budget(budget);
    let decompressed = decompressor.decompress(&compressed).expect("decompress");
    assert_eq!(decompressed, input);
}

// ─── default budget (64 MiB) handles 4 MiB input ────────────────────────────

#[test]
fn test_budget_default_works() {
    let input = vec![0x55u8; 4 * 1024 * 1024];
    let result = LzmaCompressor::new().compress(&input);
    assert!(
        result.is_ok(),
        "default 64 MiB budget must handle 4 MiB input"
    );
}

// ─── budget-bounded compress → serial decompress ─────────────────────────────

#[test]
fn test_budget_roundtrip_with_non_budgeted_decoder() {
    let input: Vec<u8> = b"abcdefgh"
        .iter()
        .cycle()
        .take(2 * 1024 * 1024)
        .cloned()
        .collect();
    let compressor = LzmaCompressor::new().with_memory_budget(64 * 1024 * 1024);
    let compressed = compressor.compress(&input).expect("compress");
    // Use the raw serial shim — same LZMA2 format
    let decompressed = lzma2_decompress(&compressed).expect("serial decompress");
    assert_eq!(decompressed, input);
}
