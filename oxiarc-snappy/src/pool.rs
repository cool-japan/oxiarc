//! Thread-safe memory pool for Snappy FrameEncoder/FrameDecoder scratch buffers.
//!
//! Amortises the repeated allocation cost of per-encode/decode chunk buffers
//! across many streaming calls.
//!
//! # Design
//!
//! There are two distinct buffer types:
//!
//! - **Encoder scratch** (`Vec<u8>`, `ENCODER_SCRATCH_CAP` bytes): the input staging
//!   buffer inside [`FrameEncoder`](crate::frame::FrameEncoder) that accumulates up to
//!   64 KiB before being flushed as a chunk.  Sized at `65536 + 256` to leave room
//!   for small overruns during extend-from-slice before flush.
//!
//! - **Decoder scratch** (`Vec<u8>`, `MAX_UNCOMPRESSED_CHUNK_SIZE` bytes): the
//!   temporary read buffer inside [`FrameDecoder`](crate::frame::FrameDecoder) used to
//!   hold raw chunk data read from the stream before decoding.
//!
//! Each bucket is a `Mutex`-guarded `Vec<Vec<u8>>`.  Acquiring a buffer either pops a
//! cached one (a "hit") or allocates fresh memory (a "miss").  On drop, the buffer is
//! returned to the correct bucket, subject to the per-bucket capacity cap.  Excess
//! buffers (over cap) are silently dropped (freed).
//!
//! The pool is `Clone` because it wraps an `Arc<PoolInner>`; all clones share the
//! same underlying buckets.
//!
//! # Example
//!
//! ```rust
//! use oxiarc_snappy::{SnappyPool, FrameEncoder, FrameDecoder};
//! use std::io::{Write, Read};
//!
//! let pool = SnappyPool::new();
//!
//! // First call allocates fresh buffers.
//! let mut compressed = Vec::new();
//! {
//!     let mut enc = FrameEncoder::with_pool(&mut compressed, &pool);
//!     enc.write_all(b"hello world").unwrap();
//!     enc.finish().unwrap();
//! }
//!
//! // Second call reuses the buffers returned from the first.
//! let mut compressed2 = Vec::new();
//! {
//!     let mut enc = FrameEncoder::with_pool(&mut compressed2, &pool);
//!     enc.write_all(b"hello world").unwrap();
//!     enc.finish().unwrap();
//! }
//! assert!(pool.stats().encoder_scratch_hits >= 1);
//! ```

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

// ─────────────────────────────────────────────────────────────────────────────
// Capacity constants
// ─────────────────────────────────────────────────────────────────────────────

/// Maximum uncompressed chunk size matches the framing spec (64 KiB).
pub(crate) const MAX_UNCOMPRESSED_CHUNK_SIZE: usize = 65536;

/// Pre-allocated capacity for encoder scratch buffers: 64 KiB + overhead.
pub(crate) const ENCODER_SCRATCH_CAP: usize = MAX_UNCOMPRESSED_CHUNK_SIZE + 256;

// ─────────────────────────────────────────────────────────────────────────────
// Internal state
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub(crate) struct PoolInner {
    /// Cached encoder staging buffers.
    pub(crate) encoder_scratch: Mutex<Vec<Vec<u8>>>,
    /// Cached decoder chunk read buffers.
    pub(crate) decoder_scratch: Mutex<Vec<Vec<u8>>>,
    /// Per-bucket cap (maximum retained buffers per bucket).
    pub(crate) cap: usize,
    /// Number of fresh encoder scratch allocations (pool miss).
    pub(crate) encoder_scratch_allocs: AtomicUsize,
    /// Number of encoder scratch buffers reused from pool (pool hit).
    pub(crate) encoder_scratch_hits: AtomicUsize,
    /// Number of fresh decoder scratch allocations (pool miss).
    pub(crate) decoder_scratch_allocs: AtomicUsize,
    /// Number of decoder scratch buffers reused from pool (pool hit).
    pub(crate) decoder_scratch_hits: AtomicUsize,
}

// ─────────────────────────────────────────────────────────────────────────────
// Public API: SnappyPool
// ─────────────────────────────────────────────────────────────────────────────

/// Thread-safe pool of reusable buffers for Snappy frame encode/decode.
///
/// Amortises the cost of allocating chunk staging buffers each time a
/// [`FrameEncoder`](crate::frame::FrameEncoder) or
/// [`FrameDecoder`](crate::frame::FrameDecoder) processes a 64 KiB chunk.
///
/// The pool is `Clone`: all clones share the same internal buckets.
///
/// See [module-level documentation](self) for a worked example.
#[derive(Clone, Debug)]
pub struct SnappyPool {
    pub(crate) inner: Arc<PoolInner>,
}

impl SnappyPool {
    /// Create a pool with a default per-bucket capacity of 4.
    pub fn new() -> Self {
        Self::with_cap(4)
    }

    /// Create a pool with a custom per-bucket capacity.
    ///
    /// Each bucket can hold at most `cap` buffers.  When the bucket is full,
    /// buffers returned by finishing encoders/decoders are dropped instead of
    /// cached.  Setting `cap = 0` effectively disables pooling (every operation
    /// allocates fresh memory; no buffers are ever retained).
    pub fn with_cap(cap: usize) -> Self {
        Self {
            inner: Arc::new(PoolInner {
                encoder_scratch: Mutex::new(Vec::new()),
                decoder_scratch: Mutex::new(Vec::new()),
                cap,
                encoder_scratch_allocs: AtomicUsize::new(0),
                encoder_scratch_hits: AtomicUsize::new(0),
                decoder_scratch_allocs: AtomicUsize::new(0),
                decoder_scratch_hits: AtomicUsize::new(0),
            }),
        }
    }

    /// Return allocation and hit statistics for both buckets.
    ///
    /// Useful for testing and performance monitoring.  See [`PoolStats`].
    pub fn stats(&self) -> PoolStats {
        PoolStats {
            encoder_scratch_allocations: self.inner.encoder_scratch_allocs.load(Ordering::Relaxed),
            encoder_scratch_hits: self.inner.encoder_scratch_hits.load(Ordering::Relaxed),
            decoder_scratch_allocations: self.inner.decoder_scratch_allocs.load(Ordering::Relaxed),
            decoder_scratch_hits: self.inner.decoder_scratch_hits.load(Ordering::Relaxed),
        }
    }
}

impl Default for SnappyPool {
    fn default() -> Self {
        Self::new()
    }
}

// SAFETY: All mutable state is guarded by `Mutex`.
unsafe impl Send for SnappyPool {}
unsafe impl Sync for SnappyPool {}

// ─────────────────────────────────────────────────────────────────────────────
// Public API: PoolStats
// ─────────────────────────────────────────────────────────────────────────────

/// Statistics returned by [`SnappyPool::stats`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PoolStats {
    /// Number of fresh (non-pooled) encoder staging allocations.
    pub encoder_scratch_allocations: usize,
    /// Number of encoder staging buffers reused from the pool.
    pub encoder_scratch_hits: usize,
    /// Number of fresh (non-pooled) decoder chunk allocations.
    pub decoder_scratch_allocations: usize,
    /// Number of decoder chunk buffers reused from the pool.
    pub decoder_scratch_hits: usize,
}
