//! Burrows-Wheeler Transform for BZip2.
//!
//! The BWT is a reversible transformation that groups similar bytes together,
//! making the data more compressible.

/// Perform the Burrows-Wheeler Transform.
/// Returns the transformed data and the index of the original string.
pub fn transform(data: &[u8]) -> (Vec<u8>, u32) {
    if data.is_empty() {
        return (Vec::new(), 0);
    }

    let n = data.len();

    // For small inputs, use simple comparison sort
    // For larger inputs (>4KB), could use more sophisticated algorithms like SA-IS
    // But for typical BZip2 block sizes (900KB max), this is acceptable
    let mut indices: Vec<usize> = (0..n).collect();

    // Optimized sort: use key extraction for better cache locality
    // Pre-compute first few bytes as keys for faster initial sorting
    if n > 8 {
        // Create sort keys from first 4 bytes (or fewer for short strings)
        let key_len = n.min(4);
        let mut keys: Vec<u32> = Vec::with_capacity(n);

        for i in 0..n {
            let mut key = 0u32;
            for j in 0..key_len {
                key = (key << 8) | (data[(i + j) % n] as u32);
            }
            keys.push(key);
        }

        // Sort by key first, then full comparison if needed
        indices.sort_by(|&a, &b| {
            // Fast path: compare keys first
            match keys[a].cmp(&keys[b]) {
                std::cmp::Ordering::Equal => {
                    // Keys match, need full comparison of all rotations
                    // This is necessary for correctness
                    for i in key_len..n {
                        let byte_a = data[(a + i) % n];
                        let byte_b = data[(b + i) % n];
                        match byte_a.cmp(&byte_b) {
                            std::cmp::Ordering::Equal => continue,
                            other => return other,
                        }
                    }
                    std::cmp::Ordering::Equal
                }
                other => other,
            }
        });
    } else {
        // For very small inputs, use simple comparison
        indices.sort_by(|&a, &b| {
            for i in 0..n {
                let byte_a = data[(a + i) % n];
                let byte_b = data[(b + i) % n];
                match byte_a.cmp(&byte_b) {
                    std::cmp::Ordering::Equal => continue,
                    other => return other,
                }
            }
            std::cmp::Ordering::Equal
        });
    }

    // Find the original string position (index 0 always exists in 0..n range)
    let orig_ptr = indices
        .iter()
        .position(|&i| i == 0)
        .expect("BWT: index 0 must exist in sorted indices") as u32;

    // Extract the last column (BWT output)
    let transformed: Vec<u8> = indices.iter().map(|&i| data[(i + n - 1) % n]).collect();

    (transformed, orig_ptr)
}

/// Perform inverse Burrows-Wheeler Transform.
/// Reconstructs the original data from the transformed data and origin pointer.
pub fn inverse_transform(data: &[u8], orig_ptr: u32) -> Vec<u8> {
    if data.is_empty() {
        return Vec::new();
    }

    let n = data.len();

    // Count occurrences of each byte
    let mut counts = [0usize; 256];
    for &byte in data {
        counts[byte as usize] += 1;
    }

    // Calculate cumulative counts (starting positions for each byte in sorted order)
    let mut cumulative = [0usize; 256];
    let mut total = 0;
    for i in 0..256 {
        cumulative[i] = total;
        total += counts[i];
    }

    // Build the transformation vector (T)
    // For each position i, T[i] tells us where to go next when following the chain
    let mut transform = vec![0usize; n];
    let mut positions = cumulative;

    for (i, &byte) in data.iter().enumerate() {
        transform[positions[byte as usize]] = i;
        positions[byte as usize] += 1;
    }

    // Reconstruct the original string by following the chain
    // Start at the row that ends at orig_ptr (the original first character)
    let mut result = Vec::with_capacity(n);
    let mut idx = transform[orig_ptr as usize];

    for _ in 0..n {
        result.push(data[idx]);
        idx = transform[idx];
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bwt_empty() {
        let (transformed, ptr) = transform(b"");
        assert!(transformed.is_empty());
        assert_eq!(ptr, 0);
    }

    #[test]
    fn test_bwt_single() {
        let (transformed, ptr) = transform(b"a");
        assert_eq!(transformed, b"a");
        assert_eq!(ptr, 0);
    }

    #[test]
    fn test_bwt_banana() {
        // Classic BWT example
        let data = b"banana";
        let (transformed, ptr) = transform(data);

        // The inverse should recover the original
        let recovered = inverse_transform(&transformed, ptr);
        assert_eq!(recovered, data.as_slice());
    }

    #[test]
    fn test_bwt_roundtrip() {
        let test_cases = [
            b"hello world".as_slice(),
            b"abracadabra",
            b"mississippi",
            b"aaaaa",
            b"abcde",
            b"the quick brown fox jumps over the lazy dog",
        ];

        for data in test_cases {
            let (transformed, ptr) = transform(data);
            let recovered = inverse_transform(&transformed, ptr);
            assert_eq!(recovered, data, "Failed for: {:?}", data);
        }
    }

    #[test]
    fn test_bwt_groups_similar() {
        // BWT should group similar bytes together
        let data = b"abababab";
        let (transformed, _) = transform(data);

        // Count runs of same byte
        let mut runs = 1;
        for i in 1..transformed.len() {
            if transformed[i] != transformed[i - 1] {
                runs += 1;
            }
        }

        // Should have fewer runs than original (which alternates)
        assert!(runs <= 4, "BWT should group similar bytes");
    }
}
