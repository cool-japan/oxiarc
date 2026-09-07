//! LZ77 compression for Brotli.
//!
//! Implements sliding-window LZ77 matching used as the first stage
//! of Brotli compression. Produces a sequence of literal bytes and
//! backward references (length, distance).

use crate::pool::BrotliPool;

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
    lz77_compress_pooled(data, params, None)
}

/// Perform LZ77 compression, optionally drawing buffers from a pool.
///
/// When `pool` is `Some`, the LZ77 command buffer and the fixed-size hash-head
/// table are acquired from the pool instead of being freshly allocated, reducing
/// per-encode allocation pressure.
pub(crate) fn lz77_compress_pooled(
    data: &[u8],
    params: &Lz77Params,
    pool: Option<&BrotliPool>,
) -> Vec<Lz77Command> {
    lz77_compress_with_prefix(data, 0, params, pool)
}

/// Perform LZ77 compression over `data[prefix_len..]`, allowed to match back
/// into `data[..prefix_len]`.
///
/// The prefix is a *shared dictionary* ([`crate::shared_dict`]): its bytes seed
/// the match finder but produce no commands, and matches into it may reach
/// farther back than `params.window_size`, because a shared dictionary sits
/// beyond the declared window in Brotli's distance space. A match that starts
/// in the prefix is capped at the prefix's end, so no command ever straddles
/// the boundary — which keeps the emitted distance a single well-defined value.
///
/// `prefix_len == 0` reproduces [`lz77_compress_pooled`] exactly, bit for bit;
/// that is what keeps the dictionary-free encoder's output frozen.
pub(crate) fn lz77_compress_with_prefix(
    data: &[u8],
    prefix_len: usize,
    params: &Lz77Params,
    pool: Option<&BrotliPool>,
) -> Vec<Lz77Command> {
    if data.len() <= prefix_len {
        return Vec::new();
    }

    match params.quality {
        0 => lz77_no_compression_pooled(&data[prefix_len..], pool),
        1..=3 => lz77_fast_pooled(data, prefix_len, params, pool),
        _ => lz77_standard_pooled(data, prefix_len, params, pool),
    }
}

/// Quality 0: no LZ77 matching, all literals, optionally pooled command buf.
fn lz77_no_compression_pooled(data: &[u8], pool: Option<&BrotliPool>) -> Vec<Lz77Command> {
    if let Some(p) = pool {
        let mut guard = p.get_lz77_cmd();
        guard
            .buf
            .extend(data.iter().map(|&b| Lz77Command::Literal(b)));
        // Take the filled buffer out of the guard; the guard's drop will return
        // the now-empty Vec back to the pool (capacity preserved).
        std::mem::take(&mut guard.buf)
    } else {
        data.iter().map(|&b| Lz77Command::Literal(b)).collect()
    }
}

/// Fast LZ77 matching (quality 1-3), optionally reusing the command buffer from pool.
/// Uses a simple hash table for O(1) match finding.
fn lz77_fast_pooled(
    data: &[u8],
    prefix_len: usize,
    params: &Lz77Params,
    pool: Option<&BrotliPool>,
) -> Vec<Lz77Command> {
    // Acquire a pooled command buffer or a fresh Vec.
    let mut commands: Vec<Lz77Command> = pool
        .map(|p| {
            let mut g = p.get_lz77_cmd();
            std::mem::take(&mut g.buf)
            // g drops here — returns the now-empty (but capacity-preserving) Vec to pool.
            // On next call, pool has the allocation back.
        })
        .unwrap_or_default();

    let mut pos = prefix_len;

    // Hash table: maps 4-byte hash to position.
    let hash_bits = 15;
    let hash_size = 1usize << hash_bits;
    let hash_mask = hash_size - 1;
    let mut hash_table = vec![0u32; hash_size];

    // The shared-dictionary prefix gets its *own* table rather than seeding the
    // main one. With a single slot per hash, seeding would evict the recent,
    // cheap-to-code positions this matcher depends on, and a distant dictionary
    // match would then replace a nearby in-data one of the same length — which
    // measurably makes the output *larger*. Keeping them apart lets the near
    // match win every tie.
    let dict_table = if prefix_len == 0 {
        Vec::new()
    } else {
        let mut t = vec![u32::MAX; hash_size];
        for p in 0..prefix_len.saturating_sub(params.min_match_len - 1) {
            t[hash4(&data[p..]) & hash_mask] = p as u32;
        }
        t
    };

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

        // In-data candidate first: it is the cheaper distance whenever both
        // match equally far.
        let mut best: Option<(usize, usize)> = None; // (length, source position)
        let distance = pos - prev_pos;
        if prev_pos >= prefix_len
            && prev_pos < pos
            && distance <= params.window_size
            && distance > 0
            && prev_pos + params.min_match_len <= data.len()
            && data[prev_pos..prev_pos + params.min_match_len]
                == data[pos..pos + params.min_match_len]
        {
            best = Some((
                extend_match(data, prev_pos, pos, params, usize::MAX),
                prev_pos,
            ));
        }

        // Shared-dictionary candidate: taken only when strictly longer. The
        // match stops at the end of the prefix so no command straddles the
        // dictionary boundary.
        if !dict_table.is_empty() {
            let cand = dict_table[hash] as usize;
            if cand != u32::MAX as usize
                && cand + params.min_match_len <= prefix_len
                && pos - cand <= params.window_size + prefix_len
                && data[cand..cand + params.min_match_len] == data[pos..pos + params.min_match_len]
            {
                let length = extend_match(data, cand, pos, params, prefix_len - cand);
                if length >= params.min_match_len
                    && best.is_none_or(|(best_len, best_src)| {
                        match_gain(length, pos - cand) > match_gain(best_len, pos - best_src)
                    })
                {
                    best = Some((length, cand));
                }
            }
        }

        if let Some((length, source)) = best {
            commands.push(Lz77Command::Reference {
                length,
                distance: pos - source,
            });
            pos += length;
        } else {
            commands.push(Lz77Command::Literal(data[pos]));
            pos += 1;
        }
    }

    commands
}

/// Approximate bit saving of coding `length` bytes as a match at `distance`
/// instead of as literals.
///
/// Eight bits saved per byte matched, minus a `log2(distance)` estimate of what
/// the distance code costs. It exists to arbitrate between a *near* match and a
/// *shared dictionary* match: dictionary distances are inherently large, so
/// picking the longer match unconditionally can (and measurably does) make the
/// output bigger than not using the dictionary at all. Only consulted when a
/// dictionary is attached, so the dictionary-free encoder's output is untouched.
fn match_gain(length: usize, distance: usize) -> i64 {
    (length as i64) * 8 - (usize::BITS - distance.leading_zeros()) as i64
}

/// Longest match between `data[source..]` and `data[pos..]`, capped at
/// `params.max_match_len`, the end of `data` and `boundary` bytes.
fn extend_match(
    data: &[u8],
    source: usize,
    pos: usize,
    params: &Lz77Params,
    boundary: usize,
) -> usize {
    let max_len = params.max_match_len.min(data.len() - pos).min(boundary);
    let mut length = 0;
    while length < max_len
        && source + length < data.len()
        && data[source + length] == data[pos + length]
    {
        length += 1;
    }
    length
}

/// Standard LZ77 matching (quality 4+), optionally reusing the hash-head buffer.
///
/// The hash-head table is `1 << 17 = 131 072` u32 entries (512 KiB).  Pooling it
/// avoids a large fresh allocation on every quality-4+ encode call.
fn lz77_standard_pooled(
    data: &[u8],
    prefix_len: usize,
    params: &Lz77Params,
    pool: Option<&BrotliPool>,
) -> Vec<Lz77Command> {
    let mut commands = Vec::new();
    let mut pos = prefix_len;

    let hash_bits = 17;
    let hash_size = 1usize << hash_bits;
    let hash_mask = hash_size - 1;

    // Acquire hash_head from the pool (512 KiB) or allocate fresh.
    // We use an enum to avoid keeping a mutable borrow on the guard while also
    // having a local Vec; the guard is dropped at function end, returning the
    // buffer to the pool.
    enum HashHeadStorage {
        Pooled(crate::pool::PooledU32Buf),
        Local(Vec<u32>),
    }
    let mut storage = if let Some(p) = pool {
        HashHeadStorage::Pooled(p.get_hash_u32())
    } else {
        HashHeadStorage::Local(vec![u32::MAX; hash_size])
    };
    let hash_head: &mut [u32] = match storage {
        HashHeadStorage::Pooled(ref mut g) => &mut g.buf,
        HashHeadStorage::Local(ref mut v) => v.as_mut_slice(),
    };

    let mut hash_chain = vec![u32::MAX; data.len()]; // data-length-sized: not poolable

    // Seed the chains with the shared-dictionary prefix. It produces no
    // commands but is a legal match source.
    for p in 0..prefix_len.saturating_sub(params.min_match_len - 1) {
        let h = hash4(&data[p..]) & hash_mask;
        hash_chain[p] = hash_head[h];
        hash_head[h] = p as u32;
    }

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

        let mut best_length = params.min_match_len - 1;
        let mut best_distance = 0;
        let mut chain_pos = hash_head[hash];
        let mut chain_count = 0;

        while chain_pos != u32::MAX && chain_count < max_chain {
            let candidate = chain_pos as usize;
            let distance = pos - candidate;

            if distance == 0 || distance > params.window_size + prefix_len {
                break;
            }
            if distance > params.window_size && candidate >= prefix_len {
                // Outside the window and not in the shared dictionary. Older
                // candidates in this chain may still be *inside* the
                // dictionary, so keep walking rather than stopping — except
                // with no dictionary, where nothing older can qualify and
                // stopping is both correct and what the frozen encoder does.
                if prefix_len == 0 {
                    break;
                }
                chain_pos = hash_chain[candidate];
                chain_count += 1;
                continue;
            }

            if candidate + best_length < data.len()
                && pos + best_length < data.len()
                && data[candidate + best_length] == data[pos + best_length]
            {
                // Stop a dictionary match at the end of the prefix so no
                // command straddles the boundary.
                let boundary = if candidate < prefix_len {
                    prefix_len - candidate
                } else {
                    usize::MAX
                };
                let max_len = params.max_match_len.min(data.len() - pos).min(boundary);
                let mut length = 0;
                while length < max_len
                    && candidate + length < data.len()
                    && data[candidate + length] == data[pos + length]
                {
                    length += 1;
                }

                let better = if prefix_len == 0 {
                    length > best_length
                } else {
                    // With a dictionary attached, a longer match at a much
                    // larger distance can cost more than it saves.
                    length >= params.min_match_len
                        && (best_distance == 0
                            || match_gain(length, distance)
                                > match_gain(best_length, best_distance))
                };
                if better {
                    best_length = length;
                    best_distance = distance;

                    if length >= params.max_match_len {
                        break;
                    }
                }
            }

            chain_pos = hash_chain[candidate];
            chain_count += 1;
        }

        hash_chain[pos] = hash_head[hash];
        hash_head[hash] = pos as u32;

        if best_distance > 0 && best_length >= params.min_match_len {
            if params.quality >= 6 && pos + 1 + params.min_match_len <= data.len() {
                let next_hash = hash4(&data[pos + 1..]) & hash_mask;
                let mut next_best_length = 0;
                let mut next_chain = hash_head[next_hash];
                let mut nc = 0;

                while next_chain != u32::MAX && nc < max_chain / 2 {
                    let nc_pos = next_chain as usize;
                    let nd = pos + 1 - nc_pos;
                    if nd == 0 || nd > params.window_size + prefix_len {
                        break;
                    }
                    if nd > params.window_size && nc_pos >= prefix_len {
                        if prefix_len == 0 {
                            break;
                        }
                        next_chain = hash_chain[nc_pos];
                        nc += 1;
                        continue;
                    }
                    let boundary = if nc_pos < prefix_len {
                        prefix_len - nc_pos
                    } else {
                        usize::MAX
                    };
                    let max_len = params.max_match_len.min(data.len() - pos - 1).min(boundary);
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
                    commands.push(Lz77Command::Literal(data[pos]));
                    pos += 1;
                    continue;
                }
            }

            commands.push(Lz77Command::Reference {
                length: best_length,
                distance: best_distance,
            });

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

    // RAII: storage drops here, returning hash_head to the pool (if pooled).
    drop(storage);
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
        let params = Lz77Params {
            quality: 0,
            ..Default::default()
        };
        let commands = lz77_compress(data, &params);
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
        let commands = lz77_compress(data, &params);
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
        let commands = lz77_compress(data, &params);
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
