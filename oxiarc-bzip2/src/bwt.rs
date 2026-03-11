//! Burrows-Wheeler Transform for BZip2.
//!
//! The BWT is a reversible transformation that groups similar bytes together,
//! making the data more compressible.
//!
//! This implementation uses the SA-IS (Suffix Array Induced Sorting) algorithm
//! by Nong, Zhang, and Chan (2009), which runs in O(n) time and O(n) space,
//! replacing the previous O(n² log n) rotation-based sort.

// ---------------------------------------------------------------------------
// SA-IS: Suffix Array Induced Sorting
// ---------------------------------------------------------------------------

/// Sentinel value used to mark empty suffix array slots.
const EMPTY: usize = usize::MAX;

/// Type tag: S-type or L-type position.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SaType {
    S,
    L,
}

/// Classify each position in `text` as S-type or L-type.
///
/// A position `i` is S-type if the suffix starting at `i` is lexicographically
/// smaller than the suffix starting at `i+1`, and L-type otherwise.
/// The last position (sentinel) is always S-type.
fn classify_types(text: &[u32]) -> Vec<SaType> {
    let n = text.len();
    let mut types = vec![SaType::S; n];
    // Last position is S-type (sentinel).
    if n > 1 {
        for i in (0..n - 1).rev() {
            if text[i] < text[i + 1] {
                types[i] = SaType::S;
            } else if text[i] > text[i + 1] {
                types[i] = SaType::L;
            } else {
                types[i] = types[i + 1]; // inherit from successor
            }
        }
    }
    types
}

/// Returns true if position `i` is LMS (Left-Most S-type): i > 0, i is S-type, i-1 is L-type.
#[inline]
fn is_lms(types: &[SaType], i: usize) -> bool {
    i > 0 && types[i] == SaType::S && types[i - 1] == SaType::L
}

/// Compute bucket sizes (character frequencies) and their head/tail boundaries.
fn compute_buckets(text: &[u32], alphabet_size: usize) -> (Vec<usize>, Vec<usize>) {
    let mut counts = vec![0usize; alphabet_size];
    for &c in text {
        counts[c as usize] += 1;
    }
    let mut head = vec![0usize; alphabet_size];
    let mut tail = vec![0usize; alphabet_size];
    let mut sum = 0usize;
    for i in 0..alphabet_size {
        head[i] = sum;
        sum = sum.saturating_add(counts[i]);
        tail[i] = sum.saturating_sub(1);
    }
    (head, tail)
}

/// Place LMS suffixes at bucket tails (right-to-left over `lms` list).
fn place_lms(sa: &mut [usize], text: &[u32], lms: &[usize], alphabet_size: usize) {
    for v in sa.iter_mut() {
        *v = EMPTY;
    }
    let (_, mut tail) = compute_buckets(text, alphabet_size);
    for &p in lms.iter().rev() {
        let c = text[p] as usize;
        sa[tail[c]] = p;
        // saturating_sub: if tail already at bucket head, leave it (next write
        // would overwrite — but this only happens for duplicate chars in lms which
        // is prevented by the algorithm's invariants).
        tail[c] = tail[c].saturating_sub(1);
    }
}

/// Induce-sort L-type suffixes: left-to-right scan.
fn induce_l(sa: &mut [usize], text: &[u32], types: &[SaType], alphabet_size: usize) {
    let (mut head, _) = compute_buckets(text, alphabet_size);
    for i in 0..sa.len() {
        let v = sa[i];
        if v == EMPTY || v == 0 {
            // v == EMPTY: slot unused.
            // v == 0: no predecessor exists (text[0-1] undefined).
            continue;
        }
        let j = v - 1;
        if types[j] == SaType::L {
            let c = text[j] as usize;
            sa[head[c]] = j;
            head[c] += 1;
        }
    }
}

/// Induce-sort S-type suffixes: right-to-left scan.
fn induce_s(sa: &mut [usize], text: &[u32], types: &[SaType], alphabet_size: usize) {
    let (_, mut tail) = compute_buckets(text, alphabet_size);
    for i in (0..sa.len()).rev() {
        let v = sa[i];
        if v == EMPTY || v == 0 {
            continue;
        }
        let j = v - 1;
        if types[j] == SaType::S {
            let c = text[j] as usize;
            sa[tail[c]] = j;
            tail[c] = tail[c].saturating_sub(1);
        }
    }
}

/// Compare two LMS substrings by scanning until both reach their respective ends.
///
/// Two LMS substrings are equal iff every character and type matches from
/// their start positions to the next LMS position (inclusive on both sides).
fn lms_equal(text: &[u32], types: &[SaType], mut a: usize, mut b: usize) -> bool {
    // We compare character by character; at step i > 0, reaching another LMS
    // on both simultaneously means "equal"; reaching it on only one means "different".
    let n = text.len();
    let mut i = 0usize;
    loop {
        let lms_a = i > 0 && is_lms(types, a);
        let lms_b = i > 0 && is_lms(types, b);
        if lms_a && lms_b {
            return true; // both ended at same relative position
        }
        if lms_a != lms_b || a >= n || b >= n {
            return false;
        }
        if text[a] != text[b] || types[a] != types[b] {
            return false;
        }
        a += 1;
        b += 1;
        i += 1;
    }
}

/// Core SA-IS algorithm.
///
/// `text` must be a sequence over `0..alphabet_size` ending with the unique
/// sentinel value `0` which must not appear elsewhere in the text.
fn sais_core(text: &[u32], alphabet_size: usize) -> Vec<usize> {
    let n = text.len();
    assert!(n >= 1);

    // Classify every position.
    let types = classify_types(text);

    // Collect LMS positions in text order (skip position 0; sentinel is at n-1
    // and is always S-type with its predecessor being L-type by virtue of the
    // sentinel being strictly the smallest character).
    let lms_list: Vec<usize> = (1..n).filter(|&i| is_lms(&types, i)).collect();

    // ----- Phase 1: coarse sort of LMS substrings -----
    let mut sa = vec![EMPTY; n];
    place_lms(&mut sa, text, &lms_list, alphabet_size);
    induce_l(&mut sa, text, &types, alphabet_size);
    induce_s(&mut sa, text, &types, alphabet_size);

    // Extract sorted LMS positions (preserving their sorted order from sa).
    let sorted_lms: Vec<usize> = sa
        .iter()
        .copied()
        .filter(|&v| v != EMPTY && is_lms(&types, v))
        .collect();

    // ----- Assign ranks to LMS substrings -----
    // rank[p] = the rank of the LMS substring starting at p.
    let mut rank_map = vec![EMPTY; n];
    let mut cur_rank = 0usize;

    if let Some(&first) = sorted_lms.first() {
        rank_map[first] = 0;
        let mut prev = first;
        for &pos in &sorted_lms[1..] {
            if !lms_equal(text, &types, prev, pos) {
                cur_rank += 1;
            }
            rank_map[pos] = cur_rank;
            prev = pos;
        }
    }

    let num_unique_ranks = cur_rank + 1;

    // Build the reduced text: ranks of LMS substrings in left-to-right order.
    // This also gives us `lms_list` which is already in left-to-right order.
    let reduced: Vec<u32> = lms_list.iter().map(|&p| rank_map[p] as u32).collect();

    // ----- Phase 2: solve reduced suffix array -----
    let sorted_lms_final: Vec<usize> = if num_unique_ranks == lms_list.len() {
        // All LMS substrings are distinct: construct order directly from ranks.
        let mut order = vec![0usize; lms_list.len()];
        for (i, &r) in reduced.iter().enumerate() {
            order[r as usize] = i;
        }
        order.iter().map(|&i| lms_list[i]).collect()
    } else {
        // Recurse: append a new sentinel (rank 0 is the old sentinel which
        // appears once, so we can add another explicit sentinel for the
        // recursive call).  Actually the sentinel of the reduced string is
        // the rank of the original sentinel (which is always 0 and at the end),
        // so we already have a valid sentinel at `reduced[last]`.
        let sub_sa = sais_core(&reduced, num_unique_ranks);
        // sub_sa[0] corresponds to the reduced-text sentinel, which maps back
        // to the ORIGINAL text's sentinel LMS position.  We must include it so
        // that place_lms can seat the original sentinel in bucket 0.
        sub_sa.iter().map(|&i| lms_list[i]).collect()
    };

    // ----- Phase 3: final induction using correctly-ordered LMS positions -----
    let mut sa = vec![EMPTY; n];
    place_lms(&mut sa, text, &sorted_lms_final, alphabet_size);
    induce_l(&mut sa, text, &types, alphabet_size);
    induce_s(&mut sa, text, &types, alphabet_size);

    sa
}

/// Construct the cyclic-rotation suffix array of `text` using SA-IS.
///
/// BWT requires *cyclic* rotation sort, not plain suffix sort.  For periodic
/// inputs the two orderings differ: plain suffix sort breaks ties between two
/// suffixes by the relative order *past* the shorter one (which stops at the
/// end of the string), while cyclic rotation sort continues from the start of
/// `text`.  Using plain suffix sort therefore produces a wrong `orig_ptr` for
/// periodic strings, making the inverse BWT reconstruct the wrong data.
///
/// Fix: build the suffix array of `text ++ text ++ sentinel`.  The 2n+1
/// extended string has no repeated cyclic rotations that could confuse the
/// suffix sort, so filtering the resulting SA to positions `0..n` yields the
/// correct cyclic rotation order.
///
/// The returned slice has length `n` and maps `rank → start_position_in_text`.
pub fn suffix_array_sais(text: &[u8]) -> Vec<usize> {
    let n = text.len();
    if n == 0 {
        return Vec::new();
    }

    // Build T+T+sentinel (length 2n+1).
    // Bytes are remapped to 1..=256 so that sentinel 0 is strictly smallest.
    let mut tt: Vec<u32> = Vec::with_capacity(2 * n + 1);
    for &b in text {
        tt.push(b as u32 + 1);
    }
    for &b in text {
        tt.push(b as u32 + 1);
    }
    tt.push(0); // unique sentinel at position 2n

    // alphabet: 0 (sentinel), 1..=256 (bytes+1)
    let sa_tt = sais_core(&tt, 257);

    // sa_tt[0] == 2n (sentinel position).
    debug_assert_eq!(sa_tt[0], 2 * n, "sentinel must occupy rank 0");

    // Filter to positions in the first copy of the text (positions 0..n).
    // These appear in cyclic-rotation-sorted order.
    let mut result = Vec::with_capacity(n);
    for &pos in &sa_tt[1..] {
        if pos < n {
            result.push(pos);
        }
    }
    debug_assert_eq!(result.len(), n, "cyclic SA must contain all n positions");
    result
}

// ---------------------------------------------------------------------------
// BWT encode / decode using the suffix array
// ---------------------------------------------------------------------------

/// Perform the Burrows-Wheeler Transform using SA-IS suffix array construction.
///
/// Returns `(bwt_output, primary_index)`.  The `primary_index` identifies the
/// row of the BWT matrix that corresponds to the original (unsorted) string.
pub fn transform(data: &[u8]) -> (Vec<u8>, u32) {
    if data.is_empty() {
        return (Vec::new(), 0);
    }

    let n = data.len();
    let sa = suffix_array_sais(data);

    let mut bwt: Vec<u8> = Vec::with_capacity(n);
    let mut primary_index = 0u32;

    for (rank, &pos) in sa.iter().enumerate() {
        if pos == 0 {
            primary_index = rank as u32;
            bwt.push(data[n - 1]);
        } else {
            bwt.push(data[pos - 1]);
        }
    }

    (bwt, primary_index)
}

/// Perform inverse Burrows-Wheeler Transform.
///
/// Reconstructs the original data from the transformed data and origin pointer.
pub fn inverse_transform(data: &[u8], orig_ptr: u32) -> Vec<u8> {
    if data.is_empty() {
        return Vec::new();
    }

    let n = data.len();

    // Count occurrences of each byte.
    let mut counts = [0usize; 256];
    for &byte in data {
        counts[byte as usize] += 1;
    }

    // Cumulative counts: starting positions for each byte in sorted order.
    let mut cumulative = [0usize; 256];
    let mut total = 0;
    for i in 0..256 {
        cumulative[i] = total;
        total += counts[i];
    }

    // Build the transformation vector T:
    // T[sorted_rank] = position_in_bwt.
    let mut t_vec = vec![0usize; n];
    let mut positions = cumulative;
    for (i, &byte) in data.iter().enumerate() {
        t_vec[positions[byte as usize]] = i;
        positions[byte as usize] += 1;
    }

    // Reconstruct original by following the chain from orig_ptr.
    let mut result = Vec::with_capacity(n);
    let mut idx = t_vec[orig_ptr as usize];
    for _ in 0..n {
        result.push(data[idx]);
        idx = t_vec[idx];
    }

    result
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

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
        let data = b"banana";
        let (transformed, ptr) = transform(data);
        let recovered = inverse_transform(&transformed, ptr);
        assert_eq!(recovered, data.as_slice());
    }

    #[test]
    fn test_bwt_roundtrip() {
        let test_cases: &[&[u8]] = &[
            b"hello world",
            b"abracadabra",
            b"mississippi",
            b"aaaaa",
            b"abcde",
            b"the quick brown fox jumps over the lazy dog",
        ];

        for data in test_cases {
            let (transformed, ptr) = transform(data);
            let recovered = inverse_transform(&transformed, ptr);
            assert_eq!(recovered, *data, "BWT roundtrip failed for: {:?}", data);
        }
    }

    #[test]
    fn test_bwt_groups_similar() {
        let data = b"abababab";
        let (transformed, _) = transform(data);

        let mut runs = 1usize;
        for i in 1..transformed.len() {
            if transformed[i] != transformed[i - 1] {
                runs += 1;
            }
        }
        assert!(
            runs <= 4,
            "BWT should group similar bytes; got {} runs",
            runs
        );
    }

    #[test]
    fn test_suffix_array_sais_basic() {
        let text = b"banana";
        let n = text.len();
        let sa = suffix_array_sais(text);
        assert_eq!(sa.len(), n);
        // SA must be a permutation of 0..n.
        let mut seen = vec![false; n];
        for &pos in &sa {
            assert!(pos < n, "position {} out of range", pos);
            assert!(!seen[pos], "duplicate position {}", pos);
            seen[pos] = true;
        }
    }

    #[test]
    fn test_suffix_array_sorted_order() {
        let text = b"mississippi";
        let sa = suffix_array_sais(text);
        for i in 1..sa.len() {
            let a = &text[sa[i - 1]..];
            let b = &text[sa[i]..];
            assert!(a <= b, "SA out of order at rank {}: {:?} > {:?}", i, a, b);
        }
    }

    #[test]
    fn test_bwt_large_repetitive() {
        let data: Vec<u8> = b"abcde".iter().cycle().take(100_000).copied().collect();
        let (transformed, idx) = transform(&data);
        let recovered = inverse_transform(&transformed, idx);
        assert_eq!(recovered, data);
    }
}
