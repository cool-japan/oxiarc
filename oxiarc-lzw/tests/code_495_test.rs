//! Test to reproduce the "Invalid LZW code: 495" error

use oxiarc_lzw::{LzwConfig, LzwDecoder, LzwEncoder};

/// Create test pattern that triggers the bug (same as failing test)
fn create_test_pattern_u8(width: u64, height: u64) -> Vec<u8> {
    let mut data = Vec::with_capacity((width * height) as usize);
    for y in 0..height {
        for x in 0..width {
            data.push(((x + y) % 256) as u8);
        }
    }
    data
}

/// Create gradient pattern (from passing test)
fn create_gradient_pattern_u8(width: u64, height: u64) -> Vec<u8> {
    let mut data = Vec::with_capacity((width * height) as usize);
    for y in 0..height {
        for x in 0..width {
            data.push(((x + y) / 4) as u8);
        }
    }
    data
}

#[test]
fn test_failing_pattern_full_image() {
    let width = 512u64;
    let height = 512u64;
    let data = create_test_pattern_u8(width, height);

    println!("Testing full 512x512 image with modulo pattern");
    println!("Data size: {} bytes", data.len());

    let config = LzwConfig::TIFF;
    let mut encoder = LzwEncoder::new(config).expect("Failed to create encoder");

    let compressed = encoder.encode(&data).expect("Failed to encode");
    println!("Compressed size: {} bytes", compressed.len());

    let mut decoder = LzwDecoder::new(config).expect("Failed to create decoder");
    let decompressed = decoder
        .decode(&compressed, data.len())
        .expect("Failed to decode");

    assert_eq!(decompressed.len(), data.len(), "Size mismatch");
    assert_eq!(decompressed, data, "Data mismatch");
    println!("✓ Test passed!");
}

#[test]
fn test_passing_pattern_full_image() {
    let width = 512u64;
    let height = 512u64;
    let data = create_gradient_pattern_u8(width, height);

    println!("Testing full 512x512 image with gradient pattern");
    println!("Data size: {} bytes", data.len());

    let config = LzwConfig::TIFF;
    let mut encoder = LzwEncoder::new(config).expect("Failed to create encoder");

    let compressed = encoder.encode(&data).expect("Failed to encode");
    println!("Compressed size: {} bytes", compressed.len());

    let mut decoder = LzwDecoder::new(config).expect("Failed to create decoder");
    let decompressed = decoder
        .decode(&compressed, data.len())
        .expect("Failed to decode");

    assert_eq!(decompressed.len(), data.len(), "Size mismatch");
    assert_eq!(decompressed, data, "Data mismatch");
    println!("✓ Test passed!");
}

#[test]
fn test_single_tile_256x256() {
    // Test a single 256x256 tile (same as COG tile size)
    let width = 256u64;
    let height = 256u64;
    let data = create_test_pattern_u8(width, height);

    println!("Testing single 256x256 tile");
    println!("Data size: {} bytes", data.len());

    let config = LzwConfig::TIFF;
    let mut encoder = LzwEncoder::new(config).expect("Failed to create encoder");

    let compressed = encoder.encode(&data).expect("Failed to encode");
    println!("Compressed size: {} bytes", compressed.len());

    let mut decoder = LzwDecoder::new(config).expect("Failed to create decoder");
    let decompressed = decoder
        .decode(&compressed, data.len())
        .expect("Failed to decode");

    assert_eq!(decompressed.len(), data.len(), "Size mismatch");
    assert_eq!(decompressed, data, "Data mismatch");
    println!("✓ Test passed!");
}
