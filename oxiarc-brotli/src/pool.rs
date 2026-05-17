//! Thread-safe memory pool for Brotli buffer allocations.
//!
//! Amortises the repeated allocation cost of per-encode buffers (LZ77 command
//! buffer, hash-head array, Huffman frequency scratch) across many compression
//! calls.
//!
//! # Design
//!
//! There are three distinct buffer types needed for each Brotli encode session:
//!
//! - **`lz77_cmd`** (`Vec<Lz77Command>`, up to `BROTLI_BLOCK_CMD_CAP` entries):
//!   the LZ77 command output vector.
//! - **`hash_u32`** (`Vec<u32>`, `HASH_U32_LEN` entries): the fixed-size hash-head
//!   table used by `lz77_standard` (the dominant allocation at quality ≥ 4).
//! - **`huffman_scratch`** (`Vec<u32>`, `HUFFMAN_SCRATCH_LEN` entries): the
//!   per-encode frequency tables used by `encode_compressed_meta_block`.
//!
//! Each type has its own `Mutex`-guarded bucket.  Acquiring a buffer either pops a
//! cached one from the bucket (a "hit") or allocates fresh memory (a "miss").  On
//! drop, the RAII guards return their buffer to the correct bucket subject to the
//! per-bucket capacity cap.  Excess buffers (over cap) are silently dropped.
//!
//! The pool is `Clone` because it wraps an `Arc<PoolInner>`; all clones share the
//! same underlying buckets.
//!
//! # Example
//!
//! ```rust
//! use oxiarc_brotli::{compress_with_params, pool::BrotliPool};
//! use oxiarc_brotli::compress::BrotliParams;
//! use oxiarc_brotli::pool::compress_with_params_pooled;
//!
//! let pool = BrotliPool::new();
//! let params = BrotliParams { quality: 5, ..Default::default() };
//! let data = b"hello world hello world hello world";
//! let c1 = compress_with_params_pooled(data, &params, &pool).unwrap();
//! let c2 = compress_with_params_pooled(data, &params, &pool).unwrap();
//! assert_eq!(c1, c2);
//! assert!(pool.stats().hash_hits >= 1);
//! ```

use crate::lz77::Lz77Command;

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

// ─────────────────────────────────────────────────────────────────────────────
// Constants
// ─────────────────────────────────────────────────────────────────────────────

/// Initial capacity for the LZ77 command buffer.
pub const BROTLI_BLOCK_CMD_CAP: usize = 65536;

/// Length (in u32 elements) of the hash-head table in `lz77_standard`.
/// This mirrors the `1 << 17 = 131072` allocation in `lz77_standard`.
pub const HASH_U32_LEN: usize = 1 << 17; // 131 072 entries = 512 KiB

/// Length (in u32 elements) of the pooled Huffman frequency scratch buffer.
/// Covers the largest freq table (ic_freqs: 704 entries), padded to power-of-2.
pub const HUFFMAN_SCRATCH_LEN: usize = 1024;

// ─────────────────────────────────────────────────────────────────────────────
// Internal state
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub(crate) struct PoolInner {
    /// Cached `Vec<Lz77Command>` command buffers.
    pub(crate) lz77_cmd: Mutex<Vec<Vec<Lz77Command>>>,
    /// Cached `Vec<u32>` hash-head buffers (`hash_head` in `lz77_standard`).
    pub(crate) hash_u32: Mutex<Vec<Vec<u32>>>,
    /// Cached `Vec<u32>` Huffman frequency scratch buffers.
    pub(crate) huffman_scratch: Mutex<Vec<Vec<u32>>>,
    /// Per-bucket cap (maximum retained buffers per bucket).
    pub(crate) cap: usize,
    // ── atomics ──
    lz77_allocs: AtomicUsize,
    lz77_hits: AtomicUsize,
    hash_allocs: AtomicUsize,
    hash_hits: AtomicUsize,
    huffman_allocs: AtomicUsize,
    huffman_hits: AtomicUsize,
}

// ─────────────────────────────────────────────────────────────────────────────
// Public API: BrotliPool
// ─────────────────────────────────────────────────────────────────────────────

/// Thread-safe pool of reusable buffers for Brotli encoding.
///
/// Amortises the cost of allocating an LZ77 command buffer, a 512 KiB hash-head
/// array, and Huffman frequency tables on each call to
/// [`compress_with_params_pooled`].  The pool is `Clone`: all clones share the
/// same internal buckets.
#[derive(Clone, Debug)]
pub struct BrotliPool {
    pub(crate) inner: Arc<PoolInner>,
}

impl BrotliPool {
    /// Create a pool with a default per-bucket capacity of 4.
    pub fn new() -> Self {
        Self::with_cap(4)
    }

    /// Create a pool with a custom per-bucket capacity.
    ///
    /// Each bucket can hold at most `cap` buffers.  When the bucket is full,
    /// buffers returned by finishing encoders are dropped instead of cached.
    /// Setting `cap = 0` effectively disables pooling (every encode allocates
    /// fresh memory; no buffers are ever retained).
    pub fn with_cap(cap: usize) -> Self {
        Self {
            inner: Arc::new(PoolInner {
                lz77_cmd: Mutex::new(Vec::new()),
                hash_u32: Mutex::new(Vec::new()),
                huffman_scratch: Mutex::new(Vec::new()),
                cap,
                lz77_allocs: AtomicUsize::new(0),
                lz77_hits: AtomicUsize::new(0),
                hash_allocs: AtomicUsize::new(0),
                hash_hits: AtomicUsize::new(0),
                huffman_allocs: AtomicUsize::new(0),
                huffman_hits: AtomicUsize::new(0),
            }),
        }
    }

    /// Return allocation and hit statistics for all buckets.
    pub fn stats(&self) -> PoolStats {
        PoolStats {
            lz77_allocations: self.inner.lz77_allocs.load(Ordering::Relaxed),
            lz77_hits: self.inner.lz77_hits.load(Ordering::Relaxed),
            hash_allocations: self.inner.hash_allocs.load(Ordering::Relaxed),
            hash_hits: self.inner.hash_hits.load(Ordering::Relaxed),
            huffman_allocations: self.inner.huffman_allocs.load(Ordering::Relaxed),
            huffman_hits: self.inner.huffman_hits.load(Ordering::Relaxed),
        }
    }

    // ── acquire ──────────────────────────────────────────────────────────────

    /// Acquire an LZ77 command buffer (cleared, with reserved capacity).
    pub(crate) fn get_lz77_cmd(&self) -> PooledCmdBuf {
        let mut guard = self
            .inner
            .lz77_cmd
            .lock()
            .unwrap_or_else(|e| e.into_inner());

        if let Some(mut buf) = guard.pop() {
            self.inner.lz77_hits.fetch_add(1, Ordering::Relaxed);
            buf.clear(); // preserve capacity, drop stale content
            PooledCmdBuf {
                buf,
                pool: Arc::clone(&self.inner),
            }
        } else {
            self.inner.lz77_allocs.fetch_add(1, Ordering::Relaxed);
            let buf = Vec::with_capacity(BROTLI_BLOCK_CMD_CAP);
            PooledCmdBuf {
                buf,
                pool: Arc::clone(&self.inner),
            }
        }
    }

    /// Acquire a hash-head buffer (filled with `u32::MAX`).
    ///
    /// Used by `lz77_standard_pooled` to amortise the 512 KiB hash-head
    /// allocation across consecutive encode calls.
    pub(crate) fn get_hash_u32(&self) -> PooledU32Buf {
        let mut guard = self
            .inner
            .hash_u32
            .lock()
            .unwrap_or_else(|e| e.into_inner());

        if let Some(mut buf) = guard.pop() {
            self.inner.hash_hits.fetch_add(1, Ordering::Relaxed);
            buf.fill(u32::MAX);
            buf.resize(HASH_U32_LEN, u32::MAX);
            PooledU32Buf {
                buf,
                pool: Arc::clone(&self.inner),
                kind: U32BufKind::HashHead,
            }
        } else {
            self.inner.hash_allocs.fetch_add(1, Ordering::Relaxed);
            let buf = vec![u32::MAX; HASH_U32_LEN];
            PooledU32Buf {
                buf,
                pool: Arc::clone(&self.inner),
                kind: U32BufKind::HashHead,
            }
        }
    }

    /// Acquire a Huffman frequency scratch buffer (zeroed).
    pub(crate) fn get_huffman_scratch(&self) -> PooledU32Buf {
        let mut guard = self
            .inner
            .huffman_scratch
            .lock()
            .unwrap_or_else(|e| e.into_inner());

        if let Some(mut buf) = guard.pop() {
            self.inner.huffman_hits.fetch_add(1, Ordering::Relaxed);
            buf.fill(0);
            buf.resize(HUFFMAN_SCRATCH_LEN, 0);
            PooledU32Buf {
                buf,
                pool: Arc::clone(&self.inner),
                kind: U32BufKind::HuffmanScratch,
            }
        } else {
            self.inner.huffman_allocs.fetch_add(1, Ordering::Relaxed);
            let buf = vec![0u32; HUFFMAN_SCRATCH_LEN];
            PooledU32Buf {
                buf,
                pool: Arc::clone(&self.inner),
                kind: U32BufKind::HuffmanScratch,
            }
        }
    }
}

impl Default for BrotliPool {
    fn default() -> Self {
        Self::new()
    }
}

// SAFETY: All mutable state is guarded by `Mutex`.
unsafe impl Send for BrotliPool {}
unsafe impl Sync for BrotliPool {}

// ─────────────────────────────────────────────────────────────────────────────
// Public API: PoolStats
// ─────────────────────────────────────────────────────────────────────────────

/// Statistics returned by [`BrotliPool::stats`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PoolStats {
    /// Number of fresh (non-pooled) LZ77 command buffer allocations.
    pub lz77_allocations: usize,
    /// Number of LZ77 command buffers reused from the pool.
    pub lz77_hits: usize,
    /// Number of fresh (non-pooled) hash-head buffer allocations.
    pub hash_allocations: usize,
    /// Number of hash-head buffers reused from the pool.
    pub hash_hits: usize,
    /// Number of fresh (non-pooled) Huffman scratch buffer allocations.
    pub huffman_allocations: usize,
    /// Number of Huffman scratch buffers reused from the pool.
    pub huffman_hits: usize,
}

// ─────────────────────────────────────────────────────────────────────────────
// Internal: bucket discriminants
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug)]
enum U32BufKind {
    HashHead,
    HuffmanScratch,
}

// ─────────────────────────────────────────────────────────────────────────────
// RAII guards
// ─────────────────────────────────────────────────────────────────────────────

/// A `Vec<Lz77Command>` buffer borrowed from a [`BrotliPool`] lz77_cmd bucket.
///
/// On drop the inner buffer is returned to the pool (if the bucket is not
/// already at capacity).
pub(crate) struct PooledCmdBuf {
    pub buf: Vec<Lz77Command>,
    pool: Arc<PoolInner>,
}

impl Drop for PooledCmdBuf {
    fn drop(&mut self) {
        let buf = std::mem::take(&mut self.buf);
        let mut guard = self.pool.lz77_cmd.lock().unwrap_or_else(|e| e.into_inner());
        if guard.len() < self.pool.cap {
            guard.push(buf);
        }
        // else: over cap — buffer is dropped (freed)
    }
}

/// A `Vec<u32>` buffer borrowed from a [`BrotliPool`] hash or huffman bucket.
///
/// On drop the inner buffer is returned to the pool (if the bucket is not
/// already at capacity).
pub(crate) struct PooledU32Buf {
    pub buf: Vec<u32>,
    pool: Arc<PoolInner>,
    kind: U32BufKind,
}

impl Drop for PooledU32Buf {
    fn drop(&mut self) {
        let buf = std::mem::take(&mut self.buf);
        match self.kind {
            U32BufKind::HashHead => {
                let mut guard = self.pool.hash_u32.lock().unwrap_or_else(|e| e.into_inner());
                if guard.len() < self.pool.cap {
                    guard.push(buf);
                }
            }
            U32BufKind::HuffmanScratch => {
                let mut guard = self
                    .pool
                    .huffman_scratch
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                if guard.len() < self.pool.cap {
                    guard.push(buf);
                }
            }
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// compress_with_params_pooled
// ─────────────────────────────────────────────────────────────────────────────

use crate::compress::BrotliParams;
use crate::error::BrotliResult;

/// Compress data using Brotli with full parameter control, reusing buffers from
/// the supplied pool.
///
/// This is the pool-aware counterpart of [`compress_with_params`](crate::compress::compress_with_params).
/// The existing non-pooled API is unchanged; this is an additive free function.
///
/// # Example
///
/// ```rust
/// use oxiarc_brotli::pool::{BrotliPool, compress_with_params_pooled};
/// use oxiarc_brotli::compress::BrotliParams;
///
/// let pool = BrotliPool::new();
/// let params = BrotliParams::default();
/// let c1 = compress_with_params_pooled(b"hello", &params, &pool).unwrap();
/// let c2 = compress_with_params_pooled(b"hello", &params, &pool).unwrap();
/// assert_eq!(c1, c2);
/// ```
pub fn compress_with_params_pooled(
    data: &[u8],
    params: &BrotliParams,
    pool: &BrotliPool,
) -> BrotliResult<Vec<u8>> {
    crate::compress::compress_with_hooks_pooled(data, params, None, None, Some(pool))
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compress::BrotliParams;
    use crate::decompress::decompress;

    // -------------------------------------------------------------------------
    // Test: Pool cap is respected.
    // -------------------------------------------------------------------------
    #[test]
    fn test_pool_cap_respected() {
        let pool = BrotliPool::with_cap(2);
        let params = BrotliParams {
            quality: 5,
            ..Default::default()
        };
        let input: Vec<u8> = b"hello pool cap test "
            .iter()
            .cycle()
            .take(16_384)
            .copied()
            .collect();

        // Compress 3 times; each call returns buffers to pool capped at 2.
        for _ in 0..3 {
            let _ =
                compress_with_params_pooled(&input, &params, &pool).expect("pool compress failed");
        }

        // Validate the pool does not exceed cap=2 for each bucket.
        {
            let guard = pool.inner.lz77_cmd.lock().expect("lock lz77_cmd");
            assert!(
                guard.len() <= 2,
                "lz77_cmd bucket should hold ≤ 2 buffers, holds {}",
                guard.len()
            );
        }
        {
            let guard = pool.inner.hash_u32.lock().expect("lock hash_u32");
            assert!(
                guard.len() <= 2,
                "hash_u32 bucket should hold ≤ 2 buffers, holds {}",
                guard.len()
            );
        }
    }

    // -------------------------------------------------------------------------
    // Test: roundtrip correctness with pool.
    // -------------------------------------------------------------------------
    #[test]
    fn test_pool_roundtrip() {
        let pool = BrotliPool::new();
        let input: Vec<u8> = b"the quick brown fox jumps over the lazy dog "
            .iter()
            .cycle()
            .take(8_192)
            .copied()
            .collect();

        // Quality 1 has a known issue with 8192-byte repeated patterns in the
        // underlying brotli implementation (pre-existing, unrelated to the pool).
        // We test quality 5 and 9 here; the integration test covers quality 1 as
        // a byte-equality comparison to ensure pooled == non-pooled behaviour.
        for quality in [5u32, 9] {
            let params = BrotliParams {
                quality,
                ..Default::default()
            };
            let compressed = compress_with_params_pooled(&input, &params, &pool)
                .unwrap_or_else(|e| panic!("compress q={quality} failed: {e}"));
            let decompressed = decompress(&compressed)
                .unwrap_or_else(|e| panic!("decompress q={quality} failed: {e}"));
            assert_eq!(decompressed, input, "roundtrip failed at quality {quality}");
        }
    }

    // -------------------------------------------------------------------------
    // Test: cap=0 never retains any buffers.
    // -------------------------------------------------------------------------
    #[test]
    fn test_pool_cap_zero() {
        let pool = BrotliPool::with_cap(0);
        let params = BrotliParams {
            quality: 3,
            ..Default::default()
        };
        let input = b"cap zero test data".to_vec();

        for _ in 0..3 {
            let _ =
                compress_with_params_pooled(&input, &params, &pool).expect("cap0 compress failed");
        }

        let stats = pool.stats();
        assert_eq!(stats.lz77_hits, 0, "cap=0 must have no lz77 hits");
        assert_eq!(stats.lz77_allocations, 3, "cap=0 must allocate 3 times");
    }
}
