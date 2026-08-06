//! Block splitting and context modeling for the Brotli encoder (RFC 7932
//! Sections 6, 7.1, 7.2, 7.3).
//!
//! # What this buys
//!
//! A Brotli meta-block does not have to model any of its three symbol streams
//! with a single Huffman code. It may declare several *block types* per
//! category — literals (`NBLTYPESL`), insert-and-copy commands (`NBLTYPESI`)
//! and distances (`NBLTYPESD`) — switch between them mid-stream, and, for the
//! two categories that have context maps, give each *context* within a type its
//! own prefix code as well. That matters whenever one meta-block spans regions
//! with genuinely different statistics — ASCII text next to packed binary, a
//! header next to a payload, structured records next to a blob — and, through
//! context modeling, whenever the next symbol's distribution depends on what
//! preceded it (which for text it strongly does).
//!
//! The decoder in this crate has always supported all of it (it is validated
//! against 608/608 reference streams). The encoder used to hardcode
//! `NBLTYPESL = NBLTYPESI = NBLTYPESD = 1` with trivial context maps. This
//! module finds the splits and the context clusterings; [`crate::compress`]
//! writes them.
//!
//! # How a split is chosen
//!
//! 1. The symbol stream is cut into fixed-size segments and a histogram is
//!    taken of each.
//! 2. Segments are clustered greedily by *merge cost* — the extra bits that
//!    merging two histograms would cost over coding them apart — with a new
//!    cluster opened only when keeping a segment separate saves more than a
//!    fresh prefix code costs.
//! 3. A few reassignment passes refine the labels, empty clusters are dropped,
//!    and adjacent segments with the same label collapse into runs.
//!
//! Every result is a *candidate*. The encoder measures the real encoded size
//! with and without each one and keeps the smallest, so a bad candidate can
//! cost encode time but can never cost compression ratio.
//!
//! # Context maps
//!
//! Block types only pay off if the types get distinct prefix codes, and for
//! literals and distances that binding is made by the context map rather than
//! by the block type itself: the decoder looks up `CMAPL[block_type * 64 +
//! context]` and `CMAPD[block_type * 4 + context]`. The same mechanism does
//! double duty as *context modeling* — two contexts of one block type may map
//! to different trees. This module therefore also owns the context-map
//! clustering and *writer*, including the move-to-front and zero-run-length
//! transforms that keep a 64-entries-per-type map down to a handful of bytes.

use crate::error::BrotliResult;

/// Literal contexts per block type (RFC 7932 Section 7.1).
pub(crate) const NUM_LITERAL_CONTEXTS: usize = 64;

/// Distance contexts per block type (RFC 7932 Section 7.2).
pub(crate) const NUM_DISTANCE_CONTEXTS: usize = 4;

/// Literals per histogram segment.
///
/// Small enough to locate a boundary reasonably tightly, large enough that a
/// segment histogram is not pure noise.
const SEGMENT_LEN: usize = 256;

/// Upper bound on literal block types in one meta-block.
///
/// The format allows 256; past a handful the per-type prefix codes and the
/// context map cost more than the sharper statistics recover, and the search
/// gets slower for nothing.
const MAX_BLOCK_TYPES: usize = 8;

/// Below this many literals a split cannot repay even one extra prefix code.
const MIN_LITERALS_FOR_SPLIT: usize = 4096;

/// Per-symbol merge-cost bar for opening a new literal block type.
///
/// At the 256-literal segment length this is the ~600-bit absolute bar the
/// literal splitter originally shipped with, restated in the scale-free form
/// every category now uses (see
/// [`IC_NEW_TYPE_COST_BITS_PER_SYMBOL`] for why).
const NEW_TYPE_COST_BITS_PER_SYMBOL: f64 = 2.34;

/// Insert-and-copy alphabet size (RFC 7932 Section 5).
pub(crate) const INSERT_AND_COPY_ALPHABET: usize = 704;

/// Distance alphabet size with `NPOSTFIX = 0`, `NDIRECT = 0`.
pub(crate) const DISTANCE_ALPHABET: usize = 64;

/// Commands per insert-and-copy histogram segment.
///
/// Larger than the literal segment because the alphabet is 704 wide: a short
/// segment's histogram over it is too sparse to cluster meaningfully.
const IC_SEGMENT_LEN: usize = 256;

/// Below this many commands an insert-and-copy split cannot repay a second
/// 704-symbol prefix code.
const MIN_COMMANDS_FOR_SPLIT: usize = 1024;

/// Upper bound on insert-and-copy block types.
const MAX_IC_BLOCK_TYPES: usize = 4;

/// Bits per symbol of merge cost above which a new insert-and-copy block type
/// is proposed.
///
/// **Why per symbol and not absolute.** A segment's merge cost scales linearly
/// with the segment length — merging two 256-command segments drawn from
/// disjoint parts of the alphabet costs ~512 bits, merging two 128-command ones
/// costs ~256 — so an absolute bar silently means completely different things
/// for different categories, and changing a segment length silently disables a
/// feature. Expressed per symbol, the bar states the thing actually meant:
/// "these two regions disagree by more than this many bits on every symbol."
///
/// These bars only gate *proposing* a candidate. The encoder writes every
/// candidate out and keeps the smallest, so being generous costs encode time
/// while being too conservative costs the feature entirely.
const IC_NEW_TYPE_COST_BITS_PER_SYMBOL: f64 = 0.75;

/// Distance-emitting commands per distance histogram segment.
const DIST_SEGMENT_LEN: usize = 64;

/// Below this many distance commands a distance split cannot repay itself.
const MIN_DISTANCES_FOR_SPLIT: usize = 512;

/// Upper bound on distance block types.
const MAX_DIST_BLOCK_TYPES: usize = 4;

/// Per-symbol merge-cost bar for opening a new distance block type.
const DIST_NEW_TYPE_COST_BITS_PER_SYMBOL: f64 = 0.75;

/// Upper bound on distinct literal prefix codes (`NTREESL`).
///
/// Each costs a 256-symbol descriptor, so the useful range is small even
/// though the format allows 256.
const MAX_LITERAL_TREES: usize = 16;

/// Per-symbol merge-cost bar for opening a new literal *context* cluster.
///
/// Context clusters are compared as whole `(block type, context)` cells rather
/// than fixed-length segments, so the per-symbol form is what makes the bar
/// meaningful across cells of wildly different occupancy.
const LITERAL_TREE_COST_BITS_PER_SYMBOL: f64 = 0.5;

/// Upper bound on distinct distance prefix codes (`NTREESD`).
const MAX_DISTANCE_TREES: usize = 8;

/// Per-symbol merge-cost bar for opening a new distance context cluster.
const DISTANCE_TREE_COST_BITS_PER_SYMBOL: f64 = 0.5;

/// A chosen block split for one symbol category.
pub(crate) struct SymbolSplit {
    /// `(block_type, symbol_count)` runs, in stream order. The first run's
    /// type is always 0, because the format makes the initial block type
    /// implicit.
    pub(crate) runs: Vec<(u8, u32)>,
    /// Number of distinct block types (always >= 2).
    pub(crate) num_types: usize,
}

/// Tuning for one category's split search.
struct SplitParams {
    alphabet: usize,
    segment_len: usize,
    max_types: usize,
    min_symbols: usize,
    new_type_cost_bits_per_symbol: f64,
}

/// A complete literal-coding plan: block types plus the context map that binds
/// prefix codes to `(block type, context)` pairs.
pub(crate) struct LiteralPlan {
    /// Block-type runs measured in literals.
    pub(crate) runs: Vec<(u8, u32)>,
    /// `NBLTYPESL`.
    pub(crate) num_types: usize,
    /// `CMAPL`, of length `num_types * 64`.
    pub(crate) context_map: Vec<u8>,
    /// `NTREESL`.
    pub(crate) num_trees: usize,
    /// One 256-entry histogram per tree.
    pub(crate) histograms: Vec<Vec<u32>>,
}

/// A complete distance-coding plan, structured like [`LiteralPlan`] but over
/// the 4-context distance space.
pub(crate) struct DistancePlan {
    /// Block-type runs measured in *distance-emitting* commands only —
    /// implicit distance-code-0 commands do not advance the distance block
    /// counter (RFC 7932 Section 9.3).
    pub(crate) runs: Vec<(u8, u32)>,
    /// `NBLTYPESD`.
    pub(crate) num_types: usize,
    /// `CMAPD`, of length `num_types * 4`.
    pub(crate) context_map: Vec<u8>,
    /// `NTREESD`.
    pub(crate) num_trees: usize,
    /// One 64-entry histogram per tree.
    pub(crate) histograms: Vec<Vec<u32>>,
}

/// Shannon cost of coding a histogram with its own optimal prefix code, in
/// bits (ignoring the integer-length rounding a real Huffman code incurs).
fn histogram_bits(histogram: &[u32]) -> f64 {
    let total: u64 = histogram.iter().map(|&c| u64::from(c)).sum();
    if total == 0 {
        return 0.0;
    }
    let total_f = total as f64;
    histogram
        .iter()
        .filter(|&&count| count > 0)
        .map(|&count| {
            let count_f = f64::from(count);
            -count_f * (count_f / total_f).log2()
        })
        .sum()
}

/// Extra bits spent by coding `a` and `b` with one shared code instead of two.
///
/// Always non-negative: merging can only blur the two distributions together.
fn merge_cost(a: &[u32], b: &[u32]) -> f64 {
    let merged: Vec<u32> = a
        .iter()
        .zip(b.iter())
        .map(|(&x, &y)| x.saturating_add(y))
        .collect();
    histogram_bits(&merged) - histogram_bits(a) - histogram_bits(b)
}

/// Cost in bits of coding `segment`'s bytes with `cluster`'s distribution.
///
/// Add-half smoothing keeps a byte the cluster has never seen from costing
/// infinity, which would make an otherwise good assignment unreachable.
fn cross_entropy_bits(segment: &[u32], cluster: &[u32]) -> f64 {
    let cluster_total: u64 = cluster.iter().map(|&c| u64::from(c)).sum();
    let denominator = cluster_total as f64 + 128.0;
    segment
        .iter()
        .zip(cluster.iter())
        .filter(|&(&count, _)| count > 0)
        .map(|(&count, &cluster_count)| {
            let probability = (f64::from(cluster_count) + 0.5) / denominator;
            -f64::from(count) * probability.log2()
        })
        .sum()
}

/// Greedy-then-refine clustering of `segments` into at most `max_clusters`
/// groups, returning one label per segment.
///
/// Shared by every split and context-clustering search in this module: they
/// differ only in what a "segment" is and how expensive a new cluster is.
fn cluster_histograms(
    segments: &[Vec<u32>],
    alphabet: usize,
    max_clusters: usize,
    new_cluster_cost_bits_per_symbol: f64,
) -> Vec<usize> {
    let mut clusters: Vec<Vec<u32>> = vec![segments[0].clone()];
    let mut labels: Vec<usize> = vec![0; segments.len()];

    // 1. Greedy online pass: each segment joins the cluster it blurs least, or
    //    opens a new one when staying separate is worth a fresh prefix code.
    for (index, segment) in segments.iter().enumerate().skip(1) {
        let mut best = 0usize;
        let mut best_cost = f64::INFINITY;
        for (cluster_index, cluster) in clusters.iter().enumerate() {
            let cost = merge_cost(cluster, segment);
            if cost < best_cost {
                best_cost = cost;
                best = cluster_index;
            }
        }
        // Merge cost scales with how many symbols the segment carries, so
        // the bar has to scale with it too; comparing an absolute bar against
        // a per-segment total would make the threshold depend on the segment
        // length rather than on how different the distributions are.
        let symbols: u64 = segment.iter().map(|&count| u64::from(count)).sum();
        let bar = new_cluster_cost_bits_per_symbol * symbols as f64;
        if best_cost > bar && clusters.len() < max_clusters {
            labels[index] = clusters.len();
            clusters.push(segment.clone());
        } else {
            labels[index] = best;
            for (slot, &count) in clusters[best].iter_mut().zip(segment.iter()) {
                *slot = slot.saturating_add(count);
            }
        }
    }

    // 2. Refinement: recompute each cluster from its members, then reassign
    //    every segment to the cluster that codes it most cheaply.
    for _ in 0..3 {
        let mut recomputed = vec![vec![0u32; alphabet]; clusters.len()];
        for (segment, &label) in segments.iter().zip(labels.iter()) {
            for (slot, &count) in recomputed[label].iter_mut().zip(segment.iter()) {
                *slot = slot.saturating_add(count);
            }
        }
        clusters = recomputed;
        let mut changed = false;
        for (segment, label) in segments.iter().zip(labels.iter_mut()) {
            let mut best = *label;
            let mut best_cost = f64::INFINITY;
            for (cluster_index, cluster) in clusters.iter().enumerate() {
                let cost = cross_entropy_bits(segment, cluster);
                if cost < best_cost {
                    best_cost = cost;
                    best = cluster_index;
                }
            }
            if best != *label {
                *label = best;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    labels
}

/// Renumber `labels` in order of first appearance and report how many distinct
/// labels survived.
///
/// The format leaves the *initial* block type implicit at 0, so the first
/// segment's label must renumber to 0 — which first-appearance order
/// guarantees.
fn compact_labels(labels: &mut [usize], upper_bound: usize) -> usize {
    let mut remap = vec![usize::MAX; upper_bound];
    let mut next = 0usize;
    for &label in labels.iter() {
        if remap[label] == usize::MAX {
            remap[label] = next;
            next += 1;
        }
    }
    for label in labels.iter_mut() {
        *label = remap[*label];
    }
    next
}

/// Collapse per-segment labels into `(block_type, symbol_count)` runs.
///
/// `total` is the true stream length, so the final (possibly short) segment
/// contributes its real size rather than a full `segment_len`.
fn labels_to_runs(labels: &[usize], segment_len: usize, total: usize) -> Vec<(u8, u32)> {
    let mut runs: Vec<(u8, u32)> = Vec::new();
    for (index, &label) in labels.iter().enumerate() {
        let length = if index + 1 == labels.len() {
            let remainder = total % segment_len;
            if remainder == 0 {
                segment_len
            } else {
                remainder
            }
        } else {
            segment_len
        } as u32;
        match runs.last_mut() {
            Some((run_type, run_len)) if usize::from(*run_type) == label => {
                *run_len += length;
            }
            _ => runs.push((label as u8, length)),
        }
    }
    runs
}

/// Split a symbol stream into block types, or `None` when one code describes it
/// well enough.
///
/// `symbols` is a flat sequence of alphabet indices in stream order.
fn split_symbols(symbols: &[u16], params: &SplitParams) -> Option<SymbolSplit> {
    if symbols.len() < params.min_symbols {
        return None;
    }
    let segments: Vec<Vec<u32>> = symbols
        .chunks(params.segment_len)
        .map(|chunk| {
            let mut histogram = vec![0u32; params.alphabet];
            for &symbol in chunk {
                histogram[usize::from(symbol)] += 1;
            }
            histogram
        })
        .collect();
    if segments.len() < 2 {
        return None;
    }

    let mut labels = cluster_histograms(
        &segments,
        params.alphabet,
        params.max_types,
        params.new_type_cost_bits_per_symbol,
    );
    let num_types = compact_labels(&mut labels, params.max_types);
    if num_types < 2 {
        return None;
    }
    let runs = labels_to_runs(&labels, params.segment_len, symbols.len());
    if runs.len() < 2 {
        return None;
    }
    debug_assert_eq!(
        runs.iter().map(|&(_, len)| len as usize).sum::<usize>(),
        symbols.len()
    );
    Some(SymbolSplit { runs, num_types })
}

/// Find a literal block split for `literals`.
///
/// Retained as a thin wrapper over [`split_symbols`] so the literal path reads
/// the same as the two new ones.
fn split_literal_types(literals: &[u8]) -> Option<SymbolSplit> {
    let symbols: Vec<u16> = literals.iter().map(|&byte| u16::from(byte)).collect();
    split_symbols(
        &symbols,
        &SplitParams {
            alphabet: 256,
            segment_len: SEGMENT_LEN,
            max_types: MAX_BLOCK_TYPES,
            min_symbols: MIN_LITERALS_FOR_SPLIT,
            new_type_cost_bits_per_symbol: NEW_TYPE_COST_BITS_PER_SYMBOL,
        },
    )
}

/// Find an insert-and-copy block split.
pub(crate) fn split_insert_and_copy(symbols: &[u16]) -> Option<SymbolSplit> {
    split_symbols(
        symbols,
        &SplitParams {
            alphabet: INSERT_AND_COPY_ALPHABET,
            segment_len: IC_SEGMENT_LEN,
            max_types: MAX_IC_BLOCK_TYPES,
            min_symbols: MIN_COMMANDS_FOR_SPLIT,
            new_type_cost_bits_per_symbol: IC_NEW_TYPE_COST_BITS_PER_SYMBOL,
        },
    )
}

/// Find a distance block split over the *distance-emitting* commands only.
fn split_distance_types(symbols: &[u16]) -> Option<SymbolSplit> {
    split_symbols(
        symbols,
        &SplitParams {
            alphabet: DISTANCE_ALPHABET,
            segment_len: DIST_SEGMENT_LEN,
            max_types: MAX_DIST_BLOCK_TYPES,
            min_symbols: MIN_DISTANCES_FOR_SPLIT,
            new_type_cost_bits_per_symbol: DIST_NEW_TYPE_COST_BITS_PER_SYMBOL,
        },
    )
}

/// Cluster per-`(block type, context)` histograms into prefix codes and build
/// the context map that binds them.
///
/// Returns `(map, num_trees, histograms)`. Cells with no observations are
/// mapped to tree 0, which is always present and costs nothing extra.
fn build_context_map(
    cells: &[Vec<u32>],
    alphabet: usize,
    max_trees: usize,
    new_tree_cost_bits_per_symbol: f64,
) -> (Vec<u8>, usize, Vec<Vec<u32>>) {
    // Cluster only the cells that actually carry symbols; an empty cell has no
    // preference and would otherwise drag a cluster toward nothing.
    let occupied: Vec<usize> = cells
        .iter()
        .enumerate()
        .filter(|(_, histogram)| histogram.iter().any(|&count| count > 0))
        .map(|(index, _)| index)
        .collect();

    if occupied.len() < 2 {
        let mut merged = vec![0u32; alphabet];
        for index in &occupied {
            for (slot, &count) in merged.iter_mut().zip(cells[*index].iter()) {
                *slot = slot.saturating_add(count);
            }
        }
        return (vec![0u8; cells.len()], 1, vec![merged]);
    }

    let occupied_histograms: Vec<Vec<u32>> =
        occupied.iter().map(|&index| cells[index].clone()).collect();
    let mut labels = cluster_histograms(
        &occupied_histograms,
        alphabet,
        max_trees,
        new_tree_cost_bits_per_symbol,
    );
    let num_trees = compact_labels(&mut labels, max_trees);

    let mut map = vec![0u8; cells.len()];
    for (position, &cell_index) in occupied.iter().enumerate() {
        map[cell_index] = labels[position] as u8;
    }
    let mut histograms = vec![vec![0u32; alphabet]; num_trees];
    for (position, &cell_index) in occupied.iter().enumerate() {
        let tree = labels[position];
        for (slot, &count) in histograms[tree].iter_mut().zip(cells[cell_index].iter()) {
            *slot = slot.saturating_add(count);
        }
    }
    (map, num_trees, histograms)
}

/// Build a complete literal-coding plan.
///
/// `literals` is the meta-block's literal stream in order, and `contexts` the
/// RFC 7932 Section 7.1 context ID of each literal under the declared context
/// mode. Both `split` (block types) and context clustering are optional
/// improvements over the single-code baseline: passing `allow_split = false`
/// with an all-equal context map reproduces it exactly.
///
/// Returns `None` when nothing beyond the baseline was found — neither more
/// than one block type nor more than one tree — so the caller can skip the
/// candidate entirely.
pub(crate) fn plan_literals(
    literals: &[u8],
    contexts: &[u8],
    allow_split: bool,
    allow_context_modeling: bool,
) -> Option<LiteralPlan> {
    debug_assert_eq!(literals.len(), contexts.len());
    let split = if allow_split {
        split_literal_types(literals)
    } else {
        None
    };
    let (runs, num_types) = match split {
        Some(ref found) => (found.runs.clone(), found.num_types),
        None => (vec![(0u8, literals.len() as u32)], 1),
    };

    // Per-(block type, context) histograms, walking the runs exactly as the
    // decoder will.
    let mut cells = vec![vec![0u32; 256]; num_types * NUM_LITERAL_CONTEXTS];
    let mut position = 0usize;
    for &(block_type, length) in &runs {
        for offset in 0..length as usize {
            let index = position + offset;
            let context = usize::from(contexts[index]);
            let cell = usize::from(block_type) * NUM_LITERAL_CONTEXTS + context;
            cells[cell][usize::from(literals[index])] += 1;
        }
        position += length as usize;
    }
    debug_assert_eq!(position, literals.len());

    let (context_map, num_trees, histograms) = if allow_context_modeling {
        build_context_map(
            &cells,
            256,
            MAX_LITERAL_TREES,
            LITERAL_TREE_COST_BITS_PER_SYMBOL,
        )
    } else {
        // One tree per block type: every context of a type shares a code.
        let mut map = Vec::with_capacity(num_types * NUM_LITERAL_CONTEXTS);
        for block_type in 0..num_types {
            map.extend(std::iter::repeat_n(block_type as u8, NUM_LITERAL_CONTEXTS));
        }
        let mut histograms = vec![vec![0u32; 256]; num_types];
        for (cell_index, cell) in cells.iter().enumerate() {
            let block_type = cell_index / NUM_LITERAL_CONTEXTS;
            for (slot, &count) in histograms[block_type].iter_mut().zip(cell.iter()) {
                *slot = slot.saturating_add(count);
            }
        }
        (map, num_types, histograms)
    };

    if num_types < 2 && num_trees < 2 {
        return None;
    }
    Some(LiteralPlan {
        runs,
        num_types,
        context_map,
        num_trees,
        histograms,
    })
}

/// Build a complete distance-coding plan.
///
/// `symbols` and `contexts` cover the *distance-emitting* commands only, in
/// order; implicit distance-code-0 commands are excluded because they neither
/// consume a distance code nor advance the distance block counter.
pub(crate) fn plan_distances(
    symbols: &[u16],
    contexts: &[u8],
    allow_split: bool,
    allow_context_modeling: bool,
) -> Option<DistancePlan> {
    debug_assert_eq!(symbols.len(), contexts.len());
    if symbols.is_empty() {
        return None;
    }
    let split = if allow_split {
        split_distance_types(symbols)
    } else {
        None
    };
    let (runs, num_types) = match split {
        Some(ref found) => (found.runs.clone(), found.num_types),
        None => (vec![(0u8, symbols.len() as u32)], 1),
    };

    let mut cells = vec![vec![0u32; DISTANCE_ALPHABET]; num_types * NUM_DISTANCE_CONTEXTS];
    let mut position = 0usize;
    for &(block_type, length) in &runs {
        for offset in 0..length as usize {
            let index = position + offset;
            let context = usize::from(contexts[index]);
            let cell = usize::from(block_type) * NUM_DISTANCE_CONTEXTS + context;
            cells[cell][usize::from(symbols[index])] += 1;
        }
        position += length as usize;
    }
    debug_assert_eq!(position, symbols.len());

    let (context_map, num_trees, histograms) = if allow_context_modeling {
        build_context_map(
            &cells,
            DISTANCE_ALPHABET,
            MAX_DISTANCE_TREES,
            DISTANCE_TREE_COST_BITS_PER_SYMBOL,
        )
    } else {
        let mut map = Vec::with_capacity(num_types * NUM_DISTANCE_CONTEXTS);
        for block_type in 0..num_types {
            map.extend(std::iter::repeat_n(block_type as u8, NUM_DISTANCE_CONTEXTS));
        }
        let mut histograms = vec![vec![0u32; DISTANCE_ALPHABET]; num_types];
        for (cell_index, cell) in cells.iter().enumerate() {
            let block_type = cell_index / NUM_DISTANCE_CONTEXTS;
            for (slot, &count) in histograms[block_type].iter_mut().zip(cell.iter()) {
                *slot = slot.saturating_add(count);
            }
        }
        (map, num_types, histograms)
    };

    if num_types < 2 && num_trees < 2 {
        return None;
    }
    Some(DistancePlan {
        runs,
        num_types,
        context_map,
        num_trees,
        histograms,
    })
}

// ---------------------------------------------------------------------------
// Context map serialization (RFC 7932 Section 7.3)
// ---------------------------------------------------------------------------

/// Forward move-to-front transform, the inverse of the decoder's
/// `inverse_move_to_front`.
///
/// Turns a map that is constant within each block type into a sequence that is
/// almost all zeros, which the zero-run coding below then collapses.
fn move_to_front(data: &mut [u8]) {
    let mut table = [0u8; 256];
    for (index, slot) in table.iter_mut().enumerate() {
        *slot = index as u8;
    }
    for value in data.iter_mut() {
        let target = *value;
        let position = table
            .iter()
            .position(|&entry| entry == target)
            .unwrap_or_default();
        *value = position as u8;
        for shift in (1..=position).rev() {
            table[shift] = table[shift - 1];
        }
        table[0] = target;
    }
}

/// One context-map symbol together with its extra bits.
struct ContextMapSymbol {
    symbol: u16,
    extra_value: u32,
    extra_bits: u8,
}

/// Largest zero-run exponent the writer uses.
///
/// `RLEMAX` costs `RLEMAX` extra symbols in the context-map alphabet; 16 lets a
/// single symbol cover runs up to 131071 entries, far more than the 256 * 64
/// maximum map size, so no run ever needs splitting.
const CONTEXT_MAP_RLEMAX: u32 = 16;

/// Encode a move-to-front-transformed map as symbols with zero runs collapsed.
///
/// The decoder reads symbol 0 as a single zero, symbols `1..=RLEMAX` as a zero
/// run of `(1 << symbol) + extra` entries, and anything above `RLEMAX` as the
/// literal value `symbol - RLEMAX`.
fn encode_context_map_symbols(transformed: &[u8], rlemax: u32) -> Vec<ContextMapSymbol> {
    let mut symbols = Vec::new();
    let mut index = 0usize;
    while index < transformed.len() {
        if transformed[index] != 0 {
            symbols.push(ContextMapSymbol {
                symbol: u16::from(transformed[index]) + rlemax as u16,
                extra_value: 0,
                extra_bits: 0,
            });
            index += 1;
            continue;
        }
        let start = index;
        while index < transformed.len() && transformed[index] == 0 {
            index += 1;
        }
        let mut run = index - start;
        // Symbol `e` in `1..=rlemax` covers a run of `(1 << e) + extra` with
        // `extra` in `0..(1 << e)`, i.e. any length in `[2^e, 2^(e+1) - 1]`.
        // Runs of exactly one zero have no such symbol and use symbol 0.
        while run >= 2 {
            let exponent = (usize::BITS - 1 - run.leading_zeros()).min(rlemax);
            let base = 1usize << exponent;
            let covered = run.min(2 * base - 1);
            symbols.push(ContextMapSymbol {
                symbol: exponent as u16,
                extra_value: (covered - base) as u32,
                extra_bits: exponent as u8,
            });
            run -= covered;
        }
        if run == 1 {
            symbols.push(ContextMapSymbol {
                symbol: 0,
                extra_value: 0,
                extra_bits: 0,
            });
        }
    }
    symbols
}

/// Write a context map (RFC 7932 Section 7.3): RLEMAX, a prefix code over the
/// `num_trees + RLEMAX` symbol alphabet, the map symbols, and the IMTF flag.
pub(crate) fn write_context_map(
    writer: &mut crate::bit_writer::BitWriter,
    map: &[u8],
    num_trees: usize,
) -> BrotliResult<()> {
    let mut transformed = map.to_vec();
    move_to_front(&mut transformed);

    let rlemax = CONTEXT_MAP_RLEMAX;
    writer.write_bit(true)?;
    writer.write_bits(rlemax - 1, 4)?;

    let symbols = encode_context_map_symbols(&transformed, rlemax);
    let alphabet = num_trees as u32 + rlemax;
    let mut frequencies = vec![0u32; alphabet as usize];
    for entry in &symbols {
        frequencies[usize::from(entry.symbol)] += 1;
    }
    let tree = crate::huffman::build_and_write_prefix_code(writer, &frequencies, alphabet)?;
    for entry in &symbols {
        tree.encode_symbol(writer, entry.symbol)?;
        if entry.extra_bits > 0 {
            writer.write_bits(entry.extra_value, u32::from(entry.extra_bits))?;
        }
    }

    // The map was move-to-front transformed, so the decoder must undo it.
    writer.write_bit(true)?;
    Ok(())
}

/// Write an `NBLTYPES` / `NTREES` field with the Section 9.2 variable-length
/// code (the exact inverse of the decoder's `read_block_type_count`).
pub(crate) fn write_block_type_count(
    writer: &mut crate::bit_writer::BitWriter,
    count: usize,
) -> BrotliResult<()> {
    if count <= 1 {
        writer.write_bit(false)?;
        return Ok(());
    }
    writer.write_bit(true)?;
    if count == 2 {
        writer.write_bits(0, 3)?;
        return Ok(());
    }
    // The decoder computes `(1 << n) + 1 + extra`, so `n` is the index of the
    // top set bit of `count - 1` and `extra` is what remains.
    let n = u32::BITS - 1 - ((count - 1) as u32).leading_zeros();
    let extra = (count as u32) - 1 - (1 << n);
    writer.write_bits(n, 3)?;
    writer.write_bits(extra, n)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bit_writer::BitWriter;

    /// The forward transform must be the exact inverse of the decoder's.
    #[test]
    fn move_to_front_inverts_the_decoder_transform() {
        fn inverse(data: &mut [u8]) {
            let mut table = [0u8; 256];
            for (index, slot) in table.iter_mut().enumerate() {
                *slot = index as u8;
            }
            for value in data.iter_mut() {
                let index = usize::from(*value);
                let recovered = table[index];
                *value = recovered;
                for shift in (1..=index).rev() {
                    table[shift] = table[shift - 1];
                }
                table[0] = recovered;
            }
        }

        /// The old one-code-per-block-type map: still the shape the writer
        /// must handle when context modeling is disabled.
        fn per_type_map(num_types: usize) -> Vec<u8> {
            let mut map = Vec::with_capacity(num_types * NUM_LITERAL_CONTEXTS);
            for block_type in 0..num_types {
                map.extend(std::iter::repeat_n(block_type as u8, NUM_LITERAL_CONTEXTS));
            }
            map
        }

        for num_types in 2..=8usize {
            let original = per_type_map(num_types);
            let mut transformed = original.clone();
            move_to_front(&mut transformed);
            let mut recovered = transformed.clone();
            inverse(&mut recovered);
            assert_eq!(recovered, original, "num_types={num_types}");
        }

        let mut arbitrary: Vec<u8> = (0..200u32).map(|i| ((i * 37) % 11) as u8).collect();
        let original = arbitrary.clone();
        move_to_front(&mut arbitrary);
        inverse(&mut arbitrary);
        assert_eq!(arbitrary, original);
    }

    /// The zero-run coder must reproduce the input under the decoder's rules.
    #[test]
    fn context_map_symbols_replay_to_the_original() {
        fn replay(symbols: &[ContextMapSymbol], rlemax: u32) -> Vec<u8> {
            let mut out = Vec::new();
            for entry in symbols {
                let symbol = u32::from(entry.symbol);
                if symbol == 0 {
                    out.push(0);
                } else if symbol <= rlemax {
                    let run = (1usize << symbol) + entry.extra_value as usize;
                    out.resize(out.len() + run, 0);
                } else {
                    out.push((symbol - rlemax) as u8);
                }
            }
            out
        }

        fn per_type_map(num_types: usize) -> Vec<u8> {
            let mut map = Vec::with_capacity(num_types * NUM_LITERAL_CONTEXTS);
            for block_type in 0..num_types {
                map.extend(std::iter::repeat_n(block_type as u8, NUM_LITERAL_CONTEXTS));
            }
            map
        }

        for num_types in 2..=8usize {
            let mut transformed = per_type_map(num_types);
            move_to_front(&mut transformed);
            let symbols = encode_context_map_symbols(&transformed, CONTEXT_MAP_RLEMAX);
            assert_eq!(
                replay(&symbols, CONTEXT_MAP_RLEMAX),
                transformed,
                "num_types={num_types}"
            );
        }

        // Isolated zeros, pairs and a long run all take different branches.
        let awkward = vec![0u8, 3, 0, 0, 5, 0, 0, 0, 0, 0, 0, 0, 1, 0];
        let symbols = encode_context_map_symbols(&awkward, CONTEXT_MAP_RLEMAX);
        assert_eq!(replay(&symbols, CONTEXT_MAP_RLEMAX), awkward);
    }

    /// The `NBLTYPES` writer must round-trip through the decoder's reader for
    /// every value the format allows.
    #[test]
    fn block_type_count_round_trips() {
        for count in 1..=256usize {
            let mut writer = BitWriter::new();
            write_block_type_count(&mut writer, count).expect("count is writable");
            // Pad so the reader always has whole bytes available.
            writer.write_bits(0, 16).expect("padding");
            let bytes = writer.finish();
            let mut reader = crate::bit_reader::BitReader::new(&bytes);
            let decoded = crate::decompress::read_block_type_count(&mut reader)
                .expect("written count parses");
            assert_eq!(decoded as usize, count, "round trip failed for {count}");
        }
    }

    /// A literal stream made of two statistically distinct halves.
    fn two_population_literals() -> Vec<u8> {
        let mut literals = Vec::new();
        literals.extend(
            std::iter::repeat_n(b'a', 8192).enumerate().map(
                |(i, c)| {
                    if i % 5 == 0 { b'b' } else { c }
                },
            ),
        );
        literals.extend((0..8192u32).map(|i| (i.wrapping_mul(2654435761) >> 24) as u8));
        literals
    }

    /// LSB6 context of each literal, given the two preceding literals.
    fn lsb6_contexts(literals: &[u8]) -> Vec<u8> {
        literals
            .iter()
            .enumerate()
            .map(|(index, _)| {
                let p1 = if index >= 1 { literals[index - 1] } else { 0 };
                p1 & 0x3F
            })
            .collect()
    }

    /// A stream made of two statistically distinct halves must be split, and
    /// the runs must cover every literal exactly once.
    #[test]
    fn splits_a_two_population_stream() {
        let literals = two_population_literals();
        let split = split_literal_types(&literals).expect("two populations must split");
        assert!(split.num_types >= 2);
        assert_eq!(split.runs[0].0, 0, "first run must use block type 0");
        let covered: u32 = split.runs.iter().map(|&(_, len)| len).sum();
        assert_eq!(covered as usize, literals.len());
    }

    /// A homogeneous stream has nothing to split, and a short one cannot repay
    /// the extra prefix codes.
    #[test]
    fn does_not_split_uniform_or_tiny_streams() {
        let uniform = vec![b'z'; 20_000];
        assert!(split_literal_types(&uniform).is_none());
        let tiny: Vec<u8> = (0..1000u32).map(|i| (i % 251) as u8).collect();
        assert!(split_literal_types(&tiny).is_none());
    }

    /// A literal plan's context map must be sized, bounded and total: one entry
    /// per `(block type, context)` cell, every entry naming a real tree.
    #[test]
    fn literal_plan_context_map_is_well_formed() {
        let literals = two_population_literals();
        let contexts = lsb6_contexts(&literals);
        for (split, model) in [(true, false), (false, true), (true, true)] {
            let Some(plan) = plan_literals(&literals, &contexts, split, model) else {
                continue;
            };
            assert_eq!(
                plan.context_map.len(),
                plan.num_types * NUM_LITERAL_CONTEXTS,
                "split={split} model={model}"
            );
            assert_eq!(plan.histograms.len(), plan.num_trees);
            assert!(
                plan.context_map
                    .iter()
                    .all(|&tree| usize::from(tree) < plan.num_trees),
                "every map entry must name an existing tree"
            );
            let covered: u32 = plan.runs.iter().map(|&(_, len)| len).sum();
            assert_eq!(covered as usize, literals.len());
            // Every literal is counted exactly once across the trees.
            let total: u32 = plan
                .histograms
                .iter()
                .map(|histogram| histogram.iter().sum::<u32>())
                .sum();
            assert_eq!(total as usize, literals.len());
        }
    }

    /// Context modeling alone (no block split) must still find more than one
    /// tree on data whose byte distribution depends on the previous byte.
    #[test]
    fn context_modeling_finds_multiple_trees_without_splitting() {
        // Even positions hold a digit, odd positions a letter, so the LSB6
        // context of the preceding byte predicts the alphabet exactly.
        let literals: Vec<u8> = (0..20_000u32)
            .map(|i| {
                if i % 2 == 0 {
                    b'0' + (i % 10) as u8
                } else {
                    b'a' + (i % 26) as u8
                }
            })
            .collect();
        let contexts = lsb6_contexts(&literals);
        let plan =
            plan_literals(&literals, &contexts, false, true).expect("context modeling must apply");
        assert_eq!(plan.num_types, 1, "block splitting was disabled");
        assert!(
            plan.num_trees >= 2,
            "context modeling must separate the two alphabets (got {} trees)",
            plan.num_trees
        );
    }

    /// The insert-and-copy splitter must find the boundary in a command stream
    /// whose two halves use disjoint symbol ranges, and refuse a uniform one.
    #[test]
    fn splits_and_declines_insert_and_copy_streams() {
        let mut symbols: Vec<u16> = Vec::new();
        symbols.extend((0..4000u32).map(|i| (i % 40) as u16));
        symbols.extend((0..4000u32).map(|i| 600 + (i % 40) as u16));
        let split = split_insert_and_copy(&symbols).expect("two populations must split");
        assert!(split.num_types >= 2);
        assert_eq!(
            split
                .runs
                .iter()
                .map(|&(_, len)| len as usize)
                .sum::<usize>(),
            symbols.len()
        );

        let uniform: Vec<u16> = std::iter::repeat_n(7u16, 8000).collect();
        assert!(split_insert_and_copy(&uniform).is_none());
        let tiny: Vec<u16> = (0..100u32).map(|i| i as u16).collect();
        assert!(split_insert_and_copy(&tiny).is_none());
    }

    /// A distance plan must be well-formed the same way a literal plan is.
    #[test]
    fn distance_plan_is_well_formed() {
        let mut symbols: Vec<u16> = Vec::new();
        symbols.extend((0..2000u32).map(|i| (i % 6) as u16));
        symbols.extend((0..2000u32).map(|i| 40 + (i % 6) as u16));
        let contexts: Vec<u8> = (0..symbols.len()).map(|i| (i % 4) as u8).collect();

        for (split, model) in [(true, false), (false, true), (true, true)] {
            let Some(plan) = plan_distances(&symbols, &contexts, split, model) else {
                continue;
            };
            assert_eq!(
                plan.context_map.len(),
                plan.num_types * NUM_DISTANCE_CONTEXTS
            );
            assert_eq!(plan.histograms.len(), plan.num_trees);
            assert!(
                plan.context_map
                    .iter()
                    .all(|&tree| usize::from(tree) < plan.num_trees)
            );
            assert_eq!(
                plan.runs
                    .iter()
                    .map(|&(_, len)| len as usize)
                    .sum::<usize>(),
                symbols.len()
            );
            let total: u32 = plan
                .histograms
                .iter()
                .map(|histogram| histogram.iter().sum::<u32>())
                .sum();
            assert_eq!(total as usize, symbols.len());
        }
    }

    /// An empty distance stream has nothing to plan.
    #[test]
    fn empty_distance_stream_has_no_plan() {
        assert!(plan_distances(&[], &[], true, true).is_none());
    }

    /// A plan that finds neither extra block types nor extra trees must report
    /// `None`, so the caller does not pay header bits for nothing.
    #[test]
    fn a_baseline_only_plan_is_reported_as_none() {
        let literals = vec![b'q'; 20_000];
        let contexts = lsb6_contexts(&literals);
        assert!(plan_literals(&literals, &contexts, true, true).is_none());
    }
}
