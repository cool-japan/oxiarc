//! Move-to-Front Transform for BZip2.
//!
//! MTF transforms a stream by replacing each byte with its position
//! in a dynamic list. After each byte, that byte is moved to the front
//! of the list. This converts local byte clusters into many zeros.

/// Perform Move-to-Front transform.
/// Returns the transformed data.
pub fn transform(data: &[u8]) -> Vec<u8> {
    if data.is_empty() {
        return Vec::new();
    }

    // Initialize the symbol list
    let mut list: Vec<u8> = (0..=255).collect();
    let mut result = Vec::with_capacity(data.len());

    for &byte in data {
        // Find the position of the byte in the list (always exists since list contains 0-255)
        let pos = list
            .iter()
            .position(|&b| b == byte)
            .expect("MTF: byte must exist in 0-255 list");
        result.push(pos as u8);

        // Move the byte to the front
        if pos > 0 {
            list.remove(pos);
            list.insert(0, byte);
        }
    }

    result
}

/// Perform inverse Move-to-Front transform.
pub fn inverse_transform(data: &[u8]) -> Vec<u8> {
    if data.is_empty() {
        return Vec::new();
    }

    // Initialize the symbol list
    let mut list: Vec<u8> = (0..=255).collect();
    let mut result = Vec::with_capacity(data.len());

    for &pos in data {
        let byte = list[pos as usize];
        result.push(byte);

        // Move the byte to the front
        if pos > 0 {
            list.remove(pos as usize);
            list.insert(0, byte);
        }
    }

    result
}

/// Optimized MTF using a limited alphabet.
/// Only includes symbols that appear in the input.
#[allow(dead_code)]
pub fn transform_with_alphabet(data: &[u8], alphabet: &[u8]) -> Vec<u8> {
    if data.is_empty() {
        return Vec::new();
    }

    let mut list = alphabet.to_vec();
    let mut result = Vec::with_capacity(data.len());

    for &byte in data {
        if let Some(pos) = list.iter().position(|&b| b == byte) {
            result.push(pos as u8);

            // Move to front
            if pos > 0 {
                list.remove(pos);
                list.insert(0, byte);
            }
        }
    }

    result
}

/// Inverse MTF with limited alphabet.
#[allow(dead_code)]
pub fn inverse_transform_with_alphabet(data: &[u8], alphabet: &[u8]) -> Vec<u8> {
    if data.is_empty() {
        return Vec::new();
    }

    let mut list = alphabet.to_vec();
    let mut result = Vec::with_capacity(data.len());

    for &pos in data {
        let byte = list[pos as usize];
        result.push(byte);

        // Move to front
        if pos > 0 {
            list.remove(pos as usize);
            list.insert(0, byte);
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mtf_empty() {
        assert!(transform(b"").is_empty());
    }

    #[test]
    fn test_mtf_single() {
        let result = transform(b"a");
        assert_eq!(result, vec![b'a']); // 'a' is at position 97
    }

    #[test]
    fn test_mtf_repeated() {
        // Repeated bytes should produce zeros after the first
        let result = transform(b"aaaa");
        assert_eq!(result, vec![b'a', 0, 0, 0]); // First 'a' at pos 97, then 0s
    }

    #[test]
    fn test_mtf_roundtrip() {
        let test_cases = [
            b"hello".as_slice(),
            b"banana",
            b"abracadabra",
            b"the quick brown fox",
        ];

        for data in test_cases {
            let transformed = transform(data);
            let recovered = inverse_transform(&transformed);
            assert_eq!(recovered, data, "Failed for: {:?}", data);
        }
    }

    #[test]
    fn test_mtf_produces_low_values() {
        // After BWT, similar bytes are grouped, so MTF should produce many low values
        let data = b"bbbbbaaaacccc";
        let transformed = transform(data);

        // Count zeros
        let zeros = transformed.iter().filter(|&&b| b == 0).count();
        // Should have many zeros due to runs
        assert!(
            zeros > data.len() / 2,
            "MTF should produce many zeros for runs"
        );
    }

    #[test]
    fn test_mtf_with_alphabet() {
        let data = b"abab";
        let alphabet = [b'a', b'b'];
        let transformed = transform_with_alphabet(data, &alphabet);

        // 'a' at pos 0, 'b' at pos 1, 'a' at pos 1 (after 'b' moved front), 'b' at pos 1
        assert_eq!(transformed, vec![0, 1, 1, 1]);

        let recovered = inverse_transform_with_alphabet(&transformed, &alphabet);
        assert_eq!(recovered, data.as_slice());
    }
}
