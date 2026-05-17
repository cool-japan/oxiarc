//! Thread-safe memory pool for DEFLATE buffer allocations.
//!
//! Amortises the repeated allocation cost of per-encode buffers (sliding window,
//! hash head array, hash chain/prev array) across many compression calls.
//!
//! # Design
//!
//! There are three distinct buffer types needed for each DEFLATE encode session:
//!
//! - **Window** (`Vec<u8>`, 64 KiB = `WINDOW_SIZE * 2`): the LZ77 sliding window.
//! - **HashHead** (`Vec<u16>`, 32768 entries): hash table mapping hash → window position.
//! - **HashPrev** (`Vec<u16>`, 32768 entries): hash chain (previous position with same hash).
//!
//! Each type has its own `Mutex`-guarded bucket.  Acquiring a buffer either pops a
//! cached one from the bucket (a "hit") or allocates fresh memory (a "miss").  On
//! drop, `PooledBuf` / `PooledU16Buf` return their buffer to the correct bucket,
//! subject to the per-bucket capacity cap.  Excess buffers (over cap) are silently
//! dropped (freed).
//!
//! The pool is `Clone` because it wraps an `Arc<PoolInner>`; all clones share the
//! same underlying buckets.
//!
//! # Example
//!
//! ```rust
//! use oxiarc_deflate::{Deflater, pool::DeflatePool};
//!
//! let pool = DeflatePool::new();
//! // First call allocates fresh buffers.
//! let compressed1 = Deflater::new(6).with_pool(&pool).compress_to_vec(b"hello world").unwrap();
//! // Second call reuses the buffers returned from the first.
//! let compressed2 = Deflater::new(6).with_pool(&pool).compress_to_vec(b"hello world").unwrap();
//! assert_eq!(compressed1, compressed2);
//! assert!(pool.stats().window_hits >= 1);
//! ```

use crate::lz77::{HASH_SIZE, WINDOW_SIZE};

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

// ─────────────────────────────────────────────────────────────────────────────
// Internal state
// ─────────────────────────────────────────────────────────────────────────────

/// Sizes (in element counts) of each pool bucket.
const WINDOW_BUF_LEN: usize = WINDOW_SIZE * 2; // bytes
const HASH_HEAD_LEN: usize = HASH_SIZE; // u16 entries
const HASH_PREV_LEN: usize = WINDOW_SIZE; // u16 entries

#[derive(Debug)]
struct PoolInner {
    /// Cached `Vec<u8>` window buffers (each of length `WINDOW_BUF_LEN`).
    window: Mutex<Vec<Vec<u8>>>,
    /// Cached `Vec<u16>` hash-head buffers (each of length `HASH_HEAD_LEN`).
    hash_head: Mutex<Vec<Vec<u16>>>,
    /// Cached `Vec<u16>` hash-prev/chain buffers (each of length `HASH_PREV_LEN`).
    hash_prev: Mutex<Vec<Vec<u16>>>,
    /// Per-bucket cap (maximum retained buffers per bucket).
    cap: usize,
    /// Number of fresh window allocations (pool miss).
    window_allocs: AtomicUsize,
    /// Number of window buffers reused from the pool (pool hit).
    window_hits: AtomicUsize,
}

// ─────────────────────────────────────────────────────────────────────────────
// Public API: DeflatePool
// ─────────────────────────────────────────────────────────────────────────────

/// Thread-safe pool of reusable buffers for DEFLATE encoding.
///
/// Amortises the cost of allocating a 64 KiB sliding window and two 64 KiB
/// hash arrays each time a [`Deflater`](crate::Deflater) encodes a block.  The pool is
/// `Clone`: all clones share the same internal buckets.
///
/// Enable pooling on a `Deflater` via [`Deflater::with_pool`](crate::Deflater::with_pool).
///
/// See [module-level documentation](self) for a worked example.
#[derive(Clone, Debug)]
pub struct DeflatePool {
    inner: Arc<PoolInner>,
}

impl DeflatePool {
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
                window: Mutex::new(Vec::new()),
                hash_head: Mutex::new(Vec::new()),
                hash_prev: Mutex::new(Vec::new()),
                cap,
                window_allocs: AtomicUsize::new(0),
                window_hits: AtomicUsize::new(0),
            }),
        }
    }

    /// Acquire a zeroed window buffer (`Vec<u8>`, length `WINDOW_SIZE * 2`).
    ///
    /// Pops from the window bucket if available (pool hit), otherwise allocates
    /// a fresh buffer (pool miss).  The returned buffer is always zeroed.
    pub(crate) fn get_window(&self) -> PooledBuf {
        let mut guard = self.inner.window.lock().unwrap_or_else(|e| e.into_inner());

        if let Some(mut buf) = guard.pop() {
            // Pool hit – zero and reuse.
            self.inner.window_hits.fetch_add(1, Ordering::Relaxed);
            buf.fill(0);
            buf.resize(WINDOW_BUF_LEN, 0);
            PooledBuf {
                buf,
                pool: Arc::clone(&self.inner),
                kind: BufKind::Window,
            }
        } else {
            // Pool miss – fresh allocation.
            self.inner.window_allocs.fetch_add(1, Ordering::Relaxed);
            let buf = vec![0u8; WINDOW_BUF_LEN];
            PooledBuf {
                buf,
                pool: Arc::clone(&self.inner),
                kind: BufKind::Window,
            }
        }
    }

    /// Acquire a zeroed hash-head buffer (`Vec<u16>`, length `HASH_SIZE`).
    pub(crate) fn get_hash_head(&self) -> PooledU16Buf {
        let mut guard = self
            .inner
            .hash_head
            .lock()
            .unwrap_or_else(|e| e.into_inner());

        if let Some(mut buf) = guard.pop() {
            buf.fill(0);
            buf.resize(HASH_HEAD_LEN, 0);
            PooledU16Buf {
                buf,
                pool: Arc::clone(&self.inner),
                kind: U16BufKind::HashHead,
            }
        } else {
            let buf = vec![0u16; HASH_HEAD_LEN];
            PooledU16Buf {
                buf,
                pool: Arc::clone(&self.inner),
                kind: U16BufKind::HashHead,
            }
        }
    }

    /// Acquire a zeroed hash-prev/chain buffer (`Vec<u16>`, length `WINDOW_SIZE`).
    pub(crate) fn get_hash_prev(&self) -> PooledU16Buf {
        let mut guard = self
            .inner
            .hash_prev
            .lock()
            .unwrap_or_else(|e| e.into_inner());

        if let Some(mut buf) = guard.pop() {
            buf.fill(0);
            buf.resize(HASH_PREV_LEN, 0);
            PooledU16Buf {
                buf,
                pool: Arc::clone(&self.inner),
                kind: U16BufKind::HashPrev,
            }
        } else {
            let buf = vec![0u16; HASH_PREV_LEN];
            PooledU16Buf {
                buf,
                pool: Arc::clone(&self.inner),
                kind: U16BufKind::HashPrev,
            }
        }
    }

    /// Return a window buffer to the pool (called by `Deflater::drop`).
    pub(crate) fn return_window(&self, buf: Vec<u8>) {
        let mut guard = self.inner.window.lock().unwrap_or_else(|e| e.into_inner());
        if guard.len() < self.inner.cap {
            guard.push(buf);
        }
    }

    /// Return a hash-head buffer to the pool (called by `Deflater::drop`).
    pub(crate) fn return_hash_head(&self, buf: Vec<u16>) {
        let mut guard = self
            .inner
            .hash_head
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if guard.len() < self.inner.cap {
            guard.push(buf);
        }
    }

    /// Return a hash-prev buffer to the pool (called by `Deflater::drop`).
    pub(crate) fn return_hash_prev(&self, buf: Vec<u16>) {
        let mut guard = self
            .inner
            .hash_prev
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if guard.len() < self.inner.cap {
            guard.push(buf);
        }
    }

    /// Return allocation and hit statistics for the window bucket.
    ///
    /// Useful for testing and performance monitoring.  See [`PoolStats`].
    pub fn stats(&self) -> PoolStats {
        PoolStats {
            window_allocations: self.inner.window_allocs.load(Ordering::Relaxed),
            window_hits: self.inner.window_hits.load(Ordering::Relaxed),
        }
    }
}

impl Default for DeflatePool {
    fn default() -> Self {
        Self::new()
    }
}

// SAFETY: All mutable state is guarded by `Mutex`.
unsafe impl Send for DeflatePool {}
unsafe impl Sync for DeflatePool {}

// ─────────────────────────────────────────────────────────────────────────────
// Public API: PoolStats
// ─────────────────────────────────────────────────────────────────────────────

/// Statistics returned by [`DeflatePool::stats`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PoolStats {
    /// Number of fresh (non-pooled) window allocations.
    pub window_allocations: usize,
    /// Number of window buffers reused from the pool.
    pub window_hits: usize,
}

// ─────────────────────────────────────────────────────────────────────────────
// Internal: bucket discriminants
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug)]
enum BufKind {
    Window,
}

#[derive(Clone, Copy, Debug)]
enum U16BufKind {
    HashHead,
    HashPrev,
}

// ─────────────────────────────────────────────────────────────────────────────
// RAII guards
// ─────────────────────────────────────────────────────────────────────────────

/// A `u8` buffer borrowed from a [`DeflatePool`] window bucket.
///
/// On drop the inner `Vec<u8>` is returned to the pool (if the bucket is not
/// already at capacity).
pub(crate) struct PooledBuf {
    pub buf: Vec<u8>,
    pool: Arc<PoolInner>,
    kind: BufKind,
}

impl Drop for PooledBuf {
    fn drop(&mut self) {
        let buf = std::mem::take(&mut self.buf);
        match self.kind {
            BufKind::Window => {
                let mut guard = self.pool.window.lock().unwrap_or_else(|e| e.into_inner());
                if guard.len() < self.pool.cap {
                    guard.push(buf);
                }
                // else: over cap — buffer is dropped (freed)
            }
        }
    }
}

/// A `u16` buffer borrowed from a [`DeflatePool`] hash bucket.
///
/// On drop the inner `Vec<u16>` is returned to the pool (if the bucket is not
/// already at capacity).
pub(crate) struct PooledU16Buf {
    pub buf: Vec<u16>,
    pool: Arc<PoolInner>,
    kind: U16BufKind,
}

impl Drop for PooledU16Buf {
    fn drop(&mut self) {
        let buf = std::mem::take(&mut self.buf);
        match self.kind {
            U16BufKind::HashHead => {
                let mut guard = self
                    .pool
                    .hash_head
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                if guard.len() < self.pool.cap {
                    guard.push(buf);
                }
            }
            U16BufKind::HashPrev => {
                let mut guard = self
                    .pool
                    .hash_prev
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
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Deflater, deflate::deflate, inflate::inflate};

    // Helper: compress with pool and verify roundtrip via inflate.
    fn compress_with_pool(pool: &DeflatePool, input: &[u8], level: u8) -> Vec<u8> {
        let mut d = Deflater::new(level).with_pool(pool);
        d.compress_to_vec(input).expect("pool compress failed")
    }

    fn compress_without_pool(input: &[u8], level: u8) -> Vec<u8> {
        deflate(input, level).expect("no-pool compress failed")
    }

    // -------------------------------------------------------------------------
    // Test 1: Pool basic — three sequential encodes reuse the window buffer.
    // -------------------------------------------------------------------------
    #[test]
    fn test_pool_basic_window_reuse() {
        let pool = DeflatePool::new();
        let input: Vec<u8> = b"the quick brown fox jumps over the lazy dog "
            .iter()
            .cycle()
            .take(8_192)
            .copied()
            .collect();

        // First call: must allocate (pool miss).
        let out1 = compress_with_pool(&pool, &input, 6);
        let out2 = compress_with_pool(&pool, &input, 6);
        let out3 = compress_with_pool(&pool, &input, 6);

        let stats = pool.stats();
        // First call = 1 alloc; calls 2 and 3 should be pool hits.
        assert!(
            stats.window_hits >= 2,
            "expected ≥ 2 window hits, got {} (allocs={})",
            stats.window_hits,
            stats.window_allocations,
        );

        // All three outputs must decompress to the original.
        for (i, compressed) in [&out1, &out2, &out3].iter().enumerate() {
            let decompressed = inflate(compressed)
                .unwrap_or_else(|e| panic!("inflate call {} failed: {}", i + 1, e));
            assert_eq!(decompressed, input, "roundtrip failed for call {}", i + 1);
        }
    }

    // -------------------------------------------------------------------------
    // Test 2: Roundtrip equality — pooled and non-pooled produce identical bytes.
    // -------------------------------------------------------------------------
    #[test]
    fn test_pool_roundtrip_equality() {
        let pool = DeflatePool::new();
        let input: Vec<u8> = b"abcdefghijklmnopqrstuvwxyz0123456789"
            .iter()
            .cycle()
            .take(65_536)
            .copied()
            .collect();

        for level in [1u8, 6, 9] {
            let pooled = compress_with_pool(&pool, &input, level);
            let baseline = compress_without_pool(&input, level);
            assert_eq!(
                pooled, baseline,
                "pooled and non-pooled output differ at level {}",
                level
            );
        }
    }

    // -------------------------------------------------------------------------
    // Test 3: Concurrent — 4 threads each compress via the same pool.
    // -------------------------------------------------------------------------
    #[test]
    fn test_pool_concurrent() {
        use std::sync::Arc;
        use std::thread;

        let pool = Arc::new(DeflatePool::new());
        let input: Arc<Vec<u8>> = Arc::new(
            b"concurrent test data "
                .iter()
                .cycle()
                .take(262_144) // 256 KiB
                .copied()
                .collect(),
        );

        let handles: Vec<_> = (0..4)
            .map(|_| {
                let p = Arc::clone(&pool);
                let d = Arc::clone(&input);
                thread::spawn(move || {
                    let mut deflater = Deflater::new(6).with_pool(&p);
                    let compressed = deflater
                        .compress_to_vec(&d)
                        .expect("thread compress failed");
                    let decompressed = inflate(&compressed).expect("thread inflate failed");
                    assert_eq!(&decompressed, d.as_ref(), "thread roundtrip failed");
                })
            })
            .collect();

        for h in handles {
            h.join().expect("thread panicked");
        }
    }

    // -------------------------------------------------------------------------
    // Test 4: Pool boundary — cap=2, compress 3 times, all outputs correct.
    // -------------------------------------------------------------------------
    #[test]
    fn test_pool_boundary_cap_respected() {
        let pool = DeflatePool::with_cap(2);
        let input: Vec<u8> = b"hello boundary test"
            .iter()
            .cycle()
            .take(16_384)
            .copied()
            .collect();

        // Compress 3 times with cap=2.  Third call either allocates fresh or
        // reuses one of the 2 cached, but never exceeds cap=2.
        let mut outputs = Vec::new();
        for _ in 0..3 {
            outputs.push(compress_with_pool(&pool, &input, 6));
        }

        // All three must decompress correctly.
        for (i, compressed) in outputs.iter().enumerate() {
            let decompressed = inflate(compressed)
                .unwrap_or_else(|e| panic!("inflate call {} failed: {}", i + 1, e));
            assert_eq!(decompressed, input, "roundtrip failed at call {}", i + 1);
        }

        // Validate the pool does not exceed cap=2.
        {
            let guard = pool.inner.window.lock().expect("lock");
            assert!(
                guard.len() <= 2,
                "pool window bucket should hold ≤ 2 buffers, holds {}",
                guard.len()
            );
        }
    }

    // -------------------------------------------------------------------------
    // Test 5: Pool with_cap(0) — zero cap, every call allocates fresh.
    // -------------------------------------------------------------------------
    #[test]
    fn test_pool_cap_zero() {
        let pool = DeflatePool::with_cap(0);
        let input = b"cap zero test data".to_vec();

        for _ in 0..3 {
            let _ = compress_with_pool(&pool, &input, 6);
        }

        // Zero-cap pool never retains buffers.
        let stats = pool.stats();
        assert_eq!(stats.window_hits, 0, "cap=0 must have no hits");
        assert_eq!(
            stats.window_allocations, 3,
            "cap=0 must allocate for every call"
        );
    }

    // -------------------------------------------------------------------------
    // Test 6: Default constructor.
    // -------------------------------------------------------------------------
    #[test]
    fn test_pool_default() {
        let pool = DeflatePool::default();
        let out = compress_with_pool(&pool, b"default test", 6);
        let dec = inflate(&out).expect("inflate default test");
        assert_eq!(dec, b"default test");
    }
}
