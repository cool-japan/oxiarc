//! Calculate compression ratios for benchmark report

use oxiarc_lzw::compress_tiff;

fn generate_uniform(size: usize) -> Vec<u8> {
    vec![0xAA; size]
}

fn generate_random(size: usize) -> Vec<u8> {
    let mut data = Vec::with_capacity(size);
    let mut seed: u64 = 0x123456789ABCDEF0;
    for _ in 0..size {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        data.push((seed >> 32) as u8);
    }
    data
}

fn generate_repetitive(size: usize) -> Vec<u8> {
    let pattern = b"TOBEORNOTTOBEORTOBEORNOT";
    let mut data = Vec::with_capacity(size);
    while data.len() < size {
        let remaining = size - data.len();
        let chunk_size = remaining.min(pattern.len());
        data.extend_from_slice(&pattern[..chunk_size]);
    }
    data
}

fn generate_text(size: usize) -> Vec<u8> {
    let text = b"The quick brown fox jumps over the lazy dog. \
                 Pack my box with five dozen liquor jugs. \
                 How vexingly quick daft zebras jump! ";
    let mut data = Vec::with_capacity(size);
    while data.len() < size {
        let remaining = size - data.len();
        let chunk_size = remaining.min(text.len());
        data.extend_from_slice(&text[..chunk_size]);
    }
    data
}

fn generate_image(size: usize) -> Vec<u8> {
    let mut data = Vec::with_capacity(size);
    let side = (size as f64).sqrt() as usize;

    for y in 0..side {
        for x in 0..side {
            let value = ((x * 255 / side) + (y * 255 / side)) / 2;
            data.push(value.min(255) as u8);
        }
    }

    while data.len() < size {
        data.push(128);
    }

    data
}

#[test]
fn calculate_compression_ratios() {
    let sizes = [
        ("small_64KB", 64 * 1024),
        ("medium_256KB", 256 * 1024),
        ("large_1MB", 1024 * 1024),
    ];

    println!("\n=== COMPRESSION RATIOS ===\n");
    println!("| Pattern | Size | Original | Compressed | Ratio | Savings |");
    println!("|---------|------|----------|------------|-------|---------|");

    for (size_name, size) in sizes {
        let patterns = [
            ("uniform", generate_uniform(size)),
            ("random", generate_random(size)),
            ("repetitive", generate_repetitive(size)),
            ("text", generate_text(size)),
            ("image", generate_image(size)),
        ];

        for (pattern_name, data) in patterns {
            let compressed = compress_tiff(&data).ok().unwrap_or_default();
            let ratio = data.len() as f64 / compressed.len() as f64;
            let savings = (1.0 - compressed.len() as f64 / data.len() as f64) * 100.0;

            println!(
                "| {:<11} | {:<12} | {:>8} | {:>10} | {:>5.2}x | {:>6.1}% |",
                pattern_name,
                size_name,
                data.len(),
                compressed.len(),
                ratio,
                savings
            );
        }
    }
}
