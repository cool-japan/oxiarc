//! LZ77 compression for Brotli.
//!
//! Implements sliding-window LZ77 matching used as the first stage
//! of Brotli compression. Produces a sequence of literal bytes and
//! backward references (length, distance).

/// LZ77 compression parameters.
#[derive(Debug, Clone)]
pub struct Lz77Params {
    /// Quality level (affects match-finding effort).
    pub quality: u32,
    /// Maximum backward reference distance (window size).
    pub window_size: usize,
    /// Minimum match length.
    pub min_match_len: usize,
    /// Maximum match length.
    pub max_match_len: usize,
}

impl Default for Lz77Params {
    fn default() -> Self {
        Lz77Params {
            quality: 6,
            window_size: 1 << 22,
            min_match_len: 4,
            max_match_len: 256,
        }
    }
}

/// A single LZ77 command.
#[derive(Debug, Clone)]
pub enum Lz77Command {
    /// A literal byte.
    Literal(u8),
    /// A backward reference: copy `length` bytes from `distance` bytes ago.
    Reference {
        /// Number of bytes to copy.
        length: usize,
        /// Distance back in the output.
        distance: usize,
    },
}

/// Perform LZ77 compression on the input data.
pub fn lz77_compress(data: &[u8], params: &Lz77Params) -> Vec<Lz77Command> {
    if data.is_empty() {
        return Vec::new();
    }

    match params.quality {
        0 => lz77_no_compression(data),
        1..=3 => lz77_fast(data, params),
        _ => lz77_standard(data, params),
    }
}

/// Quality 0: no LZ77 matching, all literals.
fn lz77_no_compression(data: &[u8]) -> Vec<Lz77Command> {
    data.iter().map(|&b| Lz77Command::Literal(b)).collect()
}

/// Fast LZ77 matching (quality 1-3).
/// Uses a simple hash table for O(1) match finding.
fn lz77_fast(data: &[u8], params: &Lz77Params) -> Vec<Lz77Command> {
    let mut commands = Vec::new();
    let mut pos = 0;

    // Hash table: maps 4-byte hash to position.
    let hash_bits = 15;
    let hash_size = 1usize << hash_bits;
    let hash_mask = hash_size - 1;
    let mut hash_table = vec![0u32; hash_size];

    while pos < data.len() {
        if pos + params.min_match_len > data.len() {
            // Not enough data for a match.
            commands.push(Lz77Command::Literal(data[pos]));
            pos += 1;
            continue;
        }

        // Compute hash of 4 bytes at current position.
        let hash = hash4(&data[pos..]) & hash_mask;
        let prev_pos = hash_table[hash] as usize;
        hash_table[hash] = pos as u32;

        // Check if we have a match.
        let distance = pos - prev_pos;
        if prev_pos < pos
            && distance <= params.window_size
            && distance > 0
            && prev_pos + params.min_match_len <= data.len()
            && data[prev_pos..prev_pos + params.min_match_len]
                == data[pos..pos + params.min_match_len]
        {
            // Extend the match.
            let max_len = params.max_match_len.min(data.len() - pos);
            let mut length = params.min_match_len;
            while length < max_len
                && prev_pos + length < data.len()
                && data[prev_pos + length] == data[pos + length]
            {
                length += 1;
            }

            commands.push(Lz77Command::Reference { length, distance });
            pos += length;
        } else {
            commands.push(Lz77Command::Literal(data[pos]));
            pos += 1;
        }
    }

    commands
}

/// Standard LZ77 matching (quality 4+).
/// Uses a hash chain for better match finding.
fn lz77_standard(data: &[u8], params: &Lz77Params) -> Vec<Lz77Command> {
    let mut commands = Vec::new();
    let mut pos = 0;

    // Hash chain: maps hash to list of positions.
    let hash_bits = 17;
    let hash_size = 1usize << hash_bits;
    let hash_mask = hash_size - 1;
    let mut hash_head = vec![u32::MAX; hash_size]; // Head of chain for each hash.
    let mut hash_chain = vec![u32::MAX; data.len()]; // Chain links.

    // Maximum chain length depends on quality.
    let max_chain = match params.quality {
        4..=5 => 16,
        6..=7 => 32,
        8..=9 => 64,
        _ => 128,
    };

    while pos < data.len() {
        if pos + params.min_match_len > data.len() {
            commands.push(Lz77Command::Literal(data[pos]));
            pos += 1;
            continue;
        }

        let hash = hash4(&data[pos..]) & hash_mask;

        // Search the hash chain for the best match.
        let mut best_length = params.min_match_len - 1;
        let mut best_distance = 0;
        let mut chain_pos = hash_head[hash];
        let mut chain_count = 0;

        while chain_pos != u32::MAX && chain_count < max_chain {
            let candidate = chain_pos as usize;
            let distance = pos - candidate;

            if distance > params.window_size || distance == 0 {
                break;
            }

            // Check match length.
            if candidate + best_length < data.len()
                && pos + best_length < data.len()
                && data[candidate + best_length] == data[pos + best_length]
            {
                let max_len = params.max_match_len.min(data.len() - pos);
                let mut length = 0;
                while length < max_len
                    && candidate + length < data.len()
                    && data[candidate + length] == data[pos + length]
                {
                    length += 1;
                }

                if length > best_length {
                    best_length = length;
                    best_distance = distance;

                    if length >= params.max_match_len {
                        break; // Good enough.
                    }
                }
            }

            chain_pos = hash_chain[candidate];
            chain_count += 1;
        }

        // Update hash chain.
        hash_chain[pos] = hash_head[hash];
        hash_head[hash] = pos as u32;

        if best_distance > 0 && best_length >= params.min_match_len {
            // Lazy match evaluation for higher quality levels.
            if params.quality >= 6 && pos + 1 + params.min_match_len <= data.len() {
                let next_hash = hash4(&data[pos + 1..]) & hash_mask;
                let mut next_best_length = 0;
                let mut next_chain = hash_head[next_hash];
                let mut nc = 0;

                while next_chain != u32::MAX && nc < max_chain / 2 {
                    let nc_pos = next_chain as usize;
                    let nd = pos + 1 - nc_pos;
                    if nd > params.window_size || nd == 0 {
                        break;
                    }
                    let max_len = params.max_match_len.min(data.len() - pos - 1);
                    let mut length = 0;
                    while length < max_len
                        && nc_pos + length < data.len()
                        && data[nc_pos + length] == data[pos + 1 + length]
                    {
                        length += 1;
                    }
                    if length > next_best_length {
                        next_best_length = length;
                    }
                    next_chain = hash_chain[nc_pos];
                    nc += 1;
                }

                if next_best_length > best_length + 1 {
                    // Better match at next position, emit literal now.
                    commands.push(Lz77Command::Literal(data[pos]));
                    pos += 1;
                    continue;
                }
            }

            commands.push(Lz77Command::Reference {
                length: best_length,
                distance: best_distance,
            });

            // Update hash chain for all positions in the match.
            for i in 1..best_length {
                if pos + i + params.min_match_len <= data.len() {
                    let h = hash4(&data[pos + i..]) & hash_mask;
                    hash_chain[pos + i] = hash_head[h];
                    hash_head[h] = (pos + i) as u32;
                }
            }

            pos += best_length;
        } else {
            commands.push(Lz77Command::Literal(data[pos]));
            pos += 1;
        }
    }

    commands
}

/// 4-byte hash function for LZ77 matching.
fn hash4(data: &[u8]) -> usize {
    if data.len() < 4 {
        return 0;
    }
    let v = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
    // Knuth multiplicative hash.
    ((v.wrapping_mul(0x9E37_79B9)) >> 15) as usize
}

/// Compute the total output size from a command sequence.
pub fn commands_output_size(commands: &[Lz77Command]) -> usize {
    let mut size = 0;
    for cmd in commands {
        match cmd {
            Lz77Command::Literal(_) => size += 1,
            Lz77Command::Reference { length, .. } => size += length,
        }
    }
    size
}

/// Decompose a command sequence back into bytes (for verification).
pub fn decompose_commands(commands: &[Lz77Command], window_size: usize) -> Vec<u8> {
    let mut output = Vec::new();

    for cmd in commands {
        match cmd {
            Lz77Command::Literal(b) => {
                output.push(*b);
            }
            Lz77Command::Reference { length, distance } => {
                let start = if *distance <= output.len() {
                    output.len() - distance
                } else {
                    0
                };
                for i in 0..*length {
                    let src_idx = start + (i % distance.min(&window_size));
                    if src_idx < output.len() {
                        output.push(output[src_idx]);
                    }
                }
            }
        }
    }

    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_no_compression() {
        let data = b"Hello";
        let commands = lz77_no_compression(data);
        assert_eq!(commands.len(), 5);
        for (i, cmd) in commands.iter().enumerate() {
            match cmd {
                Lz77Command::Literal(b) => assert_eq!(*b, data[i]),
                _ => panic!("expected literal"),
            }
        }
    }

    #[test]
    fn test_fast_compression() {
        let data = b"abcabcabcabc";
        let params = Lz77Params {
            quality: 1,
            ..Default::default()
        };
        let commands = lz77_fast(data, &params);
        // Should find repeated patterns.
        let output = decompose_commands(&commands, params.window_size);
        assert_eq!(output, data);
    }

    #[test]
    fn test_standard_compression() {
        let data = b"the quick brown fox jumps over the quick brown fox";
        let params = Lz77Params {
            quality: 6,
            ..Default::default()
        };
        let commands = lz77_standard(data, &params);
        let output = decompose_commands(&commands, params.window_size);
        assert_eq!(output, data);
    }

    #[test]
    fn test_commands_output_size() {
        let commands = vec![
            Lz77Command::Literal(b'a'),
            Lz77Command::Literal(b'b'),
            Lz77Command::Reference {
                length: 5,
                distance: 2,
            },
        ];
        assert_eq!(commands_output_size(&commands), 7);
    }

    #[test]
    fn test_hash4_consistency() {
        let data1 = b"abcd";
        let data2 = b"abcd";
        assert_eq!(hash4(data1), hash4(data2));
    }

    #[test]
    fn test_empty_input() {
        let commands = lz77_compress(b"", &Lz77Params::default());
        assert!(commands.is_empty());
    }

    #[test]
    fn test_single_byte() {
        let commands = lz77_compress(b"x", &Lz77Params::default());
        assert_eq!(commands.len(), 1);
        match &commands[0] {
            Lz77Command::Literal(b) => assert_eq!(*b, b'x'),
            _ => panic!("expected literal"),
        }
    }

    #[test]
    fn test_repeated_bytes() {
        let data = vec![b'a'; 1000];
        let params = Lz77Params {
            quality: 6,
            ..Default::default()
        };
        let commands = lz77_compress(&data, &params);
        let output = decompose_commands(&commands, params.window_size);
        assert_eq!(output, data);
        // Should have fewer commands than bytes (compression).
        assert!(commands.len() < data.len());
    }

    #[test]
    fn test_roundtrip_various_quality() {
        let data = b"Brotli is a data format specification for data streams compressed with specific algorithms.";
        for quality in 0..=9 {
            let params = Lz77Params {
                quality,
                ..Default::default()
            };
            let commands = lz77_compress(data, &params);
            let output = decompose_commands(&commands, params.window_size);
            assert_eq!(
                output,
                data.to_vec(),
                "roundtrip failed at quality {quality}"
            );
        }
    }
}
