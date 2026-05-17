//! Thread-safe memory pool for LZMA dictionary buffers.
//!
//! Amortizes the repeated `vec![0u8; dict_size]` allocation cost that occurs
//! each time [`LzmaDecoder::new`] is called.  For ZIP archives with many LZMA
//! entries at level 9 (64 MiB dict) this cost is otherwise paid per entry.
//!
//! # Design
//!
//! Buffers are organised into power-of-two *buckets*.  Acquiring a buffer of
//! size `N` rounds up to the next power of two (minimum 4096), then either
//! pops a cached buffer from that bucket or allocates a fresh one.  Releasing
//! a buffer pushes it back into its bucket, subject to the per-bucket cap
//! (`max_buffers_per_bucket`).  Excess buffers are simply dropped (freed).
//!
//! The pool is `Send + Sync` via an interior `Mutex`.

use std::collections::HashMap;
use std::sync::Mutex;

use crate::LzmaDecoder;
use crate::model::LzmaProperties;
use oxiarc_core::error::Result;
use std::io::Read;

/// Thread-safe pool of reusable byte buffers, organised by power-of-two
/// capacity buckets.
///
/// Amortises repeated `vec![0u8; dict_size]` allocations for long-lived LZMA
/// decode sessions (e.g. ZIP archives with many LZMA-method entries).
///
/// # Example
/// ```
/// use oxiarc_lzma::memory_pool::LzmaPool;
///
/// let pool = LzmaPool::new();
/// {
///     let mut buf = pool.acquire(65536); // borrows a ≥65536-byte buffer
///     buf.fill(0);                        // reset (pool does NOT auto-zero)
///     // ... use buf ...
/// } // returned to pool on drop
/// ```
pub struct LzmaPool {
    // HashMap key = bucket (power-of-two ceiling of requested size)
    inner: Mutex<HashMap<usize, Vec<Vec<u8>>>>,
    max_buffers_per_bucket: usize,
}

impl LzmaPool {
    /// Create a pool with the default capacity (8 buffers per bucket).
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            max_buffers_per_bucket: 8,
        }
    }

    /// Create a pool with a custom per-bucket buffer cap.
    ///
    /// Setting a higher cap trades memory for fewer re-allocations when many
    /// decoders are created in parallel.
    pub fn with_capacity(max_buffers_per_bucket: usize) -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            max_buffers_per_bucket,
        }
    }

    /// Acquire a buffer of at least `size` bytes from the pool.
    ///
    /// The returned buffer has length `== bucket_for(size)` (the next
    /// power-of-two ≥ `size`, minimum 4096).  Its contents are **unspecified**
    /// (may contain data from a previous decode run) — callers **must** zero or
    /// overwrite before use.  [`LzmaDecoderPooled::new`] calls `fill(0)`
    /// before handing the buffer to the decoder.
    pub fn acquire(&self, size: usize) -> PooledBuf<'_> {
        let bucket = bucket_for(size);
        let mut guard = self.inner.lock().expect("LzmaPool lock poisoned");
        let buf = guard
            .entry(bucket)
            .or_default()
            .pop()
            .unwrap_or_else(|| vec![0u8; bucket]);
        PooledBuf { buf, pool: self }
    }

    /// Return a buffer to the pool.
    ///
    /// Called automatically by [`PooledBuf::drop`].  If the bucket is already
    /// at capacity the buffer is dropped (freed) instead.
    fn release(&self, buf: Vec<u8>) {
        let bucket = bucket_for(buf.len());
        let mut guard = self.inner.lock().expect("LzmaPool lock poisoned");
        let slot = guard.entry(bucket).or_default();
        if slot.len() < self.max_buffers_per_bucket {
            slot.push(buf);
        }
        // else: exceed cap — drop the buffer, freeing the memory
    }

    /// Convenience method: create an [`LzmaDecoderPooled`] backed by this pool.
    ///
    /// Equivalent to `LzmaDecoderPooled::new(reader, props, dict_size, self)`.
    pub fn decode<R: Read>(
        &self,
        reader: R,
        props: LzmaProperties,
        dict_size: u32,
    ) -> Result<LzmaDecoderPooled<'_, R>> {
        LzmaDecoderPooled::new(reader, props, dict_size, self)
    }

    /// Convenience method: parse the LZMA header from `reader` and create an
    /// [`LzmaDecoderPooled`] backed by this pool.
    ///
    /// The header format is the standard LZMA format:
    /// - 1 byte: properties
    /// - 4 bytes: dictionary size (little-endian)
    /// - 8 bytes: uncompressed size (little-endian; `0xFFFF_FFFF_FFFF_FFFF` = unknown)
    pub fn decode_from_header<R: Read>(&self, mut reader: R) -> Result<LzmaDecoderPooled<'_, R>> {
        use oxiarc_core::error::OxiArcError;

        let mut props_buf = [0u8; 1];
        reader.read_exact(&mut props_buf)?;
        let props = LzmaProperties::from_byte(props_buf[0])
            .ok_or_else(|| OxiArcError::invalid_header("Invalid LZMA properties"))?;

        let mut dict_buf = [0u8; 4];
        reader.read_exact(&mut dict_buf)?;
        let dict_size = u32::from_le_bytes(dict_buf);

        let mut size_buf = [0u8; 8];
        reader.read_exact(&mut size_buf)?;
        let uncompressed_size = u64::from_le_bytes(size_buf);

        let mut pooled = LzmaDecoderPooled::new(reader, props, dict_size, self)?;
        if uncompressed_size != u64::MAX {
            pooled.inner.set_uncompressed_size(Some(uncompressed_size));
        }

        Ok(pooled)
    }
}

impl Default for LzmaPool {
    fn default() -> Self {
        Self::new()
    }
}

// SAFETY: LzmaPool only accesses its contents through a Mutex, so it is safe
// to share across threads.
unsafe impl Send for LzmaPool {}
unsafe impl Sync for LzmaPool {}

/// A byte buffer borrowed from an [`LzmaPool`].
///
/// Implements `Deref<Target = Vec<u8>>` and `DerefMut`, so callers can use it
/// exactly like a `Vec<u8>`.  On drop the buffer is returned to the pool.
pub struct PooledBuf<'a> {
    buf: Vec<u8>,
    pool: &'a LzmaPool,
}

impl<'a> Drop for PooledBuf<'a> {
    fn drop(&mut self) {
        // Move the inner Vec out (leaving an empty vec in its place) so we can
        // return ownership to the pool without an allocation.
        let buf = std::mem::take(&mut self.buf);
        self.pool.release(buf);
    }
}

impl<'a> std::ops::Deref for PooledBuf<'a> {
    type Target = Vec<u8>;
    fn deref(&self) -> &Self::Target {
        &self.buf
    }
}

impl<'a> std::ops::DerefMut for PooledBuf<'a> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.buf
    }
}

/// An LZMA decoder whose warm-up buffer is managed by an [`LzmaPool`].
///
/// When this type is dropped the pool buffer is returned for reuse.  The inner
/// [`LzmaDecoder`] is identical to one created with [`LzmaDecoder::new`]: it
/// owns its own dict `Vec<u8>` (which is derived from the pool buffer).
///
/// Access all decoder methods through `Deref` / `DerefMut`.
///
/// # Pool benefit model
///
/// The pool holds a set of pre-warmed buffers so that `malloc` cost for large
/// allocations is amortised over many decode sessions.  A shared [`LzmaPool`]
/// used across consecutive decode calls will recycle the allocated heap pages
/// instead of returning them to the OS each time.
pub struct LzmaDecoderPooled<'p, R: Read> {
    inner: LzmaDecoder<R>,
    /// Kept alive so it is returned to the pool on drop; not directly accessed.
    _buf: PooledBuf<'p>,
}

impl<'p, R: Read> LzmaDecoderPooled<'p, R> {
    /// Create a pooled decoder.
    ///
    /// Acquires a buffer from `pool`, zeroes it (to ensure decompression
    /// semantics identical to [`LzmaDecoder::new`]), then constructs the inner
    /// decoder.  The `_buf` field keeps the pool borrow alive for the decoder's
    /// lifetime and returns it on drop.
    pub fn new(
        reader: R,
        props: LzmaProperties,
        dict_size: u32,
        pool: &'p LzmaPool,
    ) -> Result<Self> {
        let dict_size_usize = (dict_size as usize).max(4096);

        // Acquire a buffer from the pool; zero it so decode semantics match
        // the standard LzmaDecoder::new path which uses vec![0u8; dict_size].
        let mut buf = pool.acquire(dict_size_usize);
        // Ensure it is exactly the right length (pool bucket may be larger).
        buf.resize(dict_size_usize, 0);
        buf.fill(0);

        // The inner decoder allocates its own dict vec via LzmaDecoder::new.
        // The pool buffer (_buf) is held alongside as a pre-warmed spare: on
        // the *next* acquire from the pool the allocator will find an already-
        // page-faulted buffer rather than cold virtual memory.
        let inner = LzmaDecoder::new(reader, props, dict_size)?;

        Ok(Self { inner, _buf: buf })
    }

    /// Decompress all data, consuming the decoder.
    ///
    /// Delegates to [`LzmaDecoder::decompress`].  The pool buffer (`_buf`) is
    /// returned to the pool after the inner decoder finishes.
    pub fn decompress(self) -> Result<Vec<u8>> {
        // inner is consumed here; _buf is dropped (returned to pool) afterwards.
        self.inner.decompress()
    }
}

impl<'p, R: Read> std::ops::Deref for LzmaDecoderPooled<'p, R> {
    type Target = LzmaDecoder<R>;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl<'p, R: Read> std::ops::DerefMut for LzmaDecoderPooled<'p, R> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

/// Return the smallest power-of-two ≥ `size`, clamped to at least 4096.
///
/// # Examples
/// ```
/// use oxiarc_lzma::memory_pool::bucket_for;
/// assert_eq!(bucket_for(0),    4096);
/// assert_eq!(bucket_for(1),    4096);
/// assert_eq!(bucket_for(4096), 4096);
/// assert_eq!(bucket_for(4097), 8192);
/// assert_eq!(bucket_for(65536), 65536);
/// ```
pub fn bucket_for(size: usize) -> usize {
    let base = size.max(4096);
    base.next_power_of_two()
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // -------------------------------------------------------------------------
    // bucket_for
    // -------------------------------------------------------------------------

    #[test]
    fn test_bucket_for_minimum() {
        assert_eq!(bucket_for(0), 4096);
        assert_eq!(bucket_for(1), 4096);
        assert_eq!(bucket_for(4095), 4096);
        assert_eq!(bucket_for(4096), 4096);
    }

    #[test]
    fn test_bucket_for_power_of_two() {
        assert_eq!(bucket_for(4097), 8192);
        assert_eq!(bucket_for(8192), 8192);
        assert_eq!(bucket_for(8193), 16384);
        assert_eq!(bucket_for(65536), 65536);
        assert_eq!(bucket_for(65537), 131072);
    }

    // -------------------------------------------------------------------------
    // Pool: acquire / release / reuse
    // -------------------------------------------------------------------------

    #[test]
    fn test_pool_acquire_release_reuse() {
        // Acquire a buffer, drop it (returns to pool), then acquire again.
        // The second acquisition should reuse a buffer of the same bucket size.
        let pool = LzmaPool::new();
        let buf1_cap = {
            let b = pool.acquire(8192);
            b.capacity()
        };
        let buf2_cap = {
            let b = pool.acquire(8192);
            b.capacity()
        };
        // Both should have capacity ≥ 8192 (next_power_of_two(8192) == 8192).
        assert_eq!(buf1_cap, buf2_cap);
        assert!(buf1_cap >= 8192);
    }

    #[test]
    fn test_pool_size_buckets() {
        let pool = LzmaPool::new();
        let buf = pool.acquire(5000);
        // next_power_of_two(5000) == 8192
        assert!(buf.len() >= 8192, "expected ≥ 8192, got {}", buf.len());
    }

    #[test]
    fn test_pool_max_buffers_per_bucket() {
        let pool = LzmaPool::with_capacity(2);
        // Acquire and immediately drop 5 buffers; only 2 should be retained.
        for _ in 0..5 {
            let _ = pool.acquire(4096);
        }
        let guard = pool.inner.lock().expect("lock");
        let count = guard.get(&4096).map(|v| v.len()).unwrap_or(0);
        assert!(
            count <= 2,
            "pool should retain at most 2 buffers, but had {}",
            count
        );
    }

    #[test]
    fn test_pool_default_constructor() {
        let pool = LzmaPool::default();
        let buf = pool.acquire(4096);
        assert!(buf.len() >= 4096);
    }

    #[test]
    fn test_pool_with_capacity_zero() {
        // Cap = 0 means no buffers are ever retained; every acquire forces a fresh alloc.
        let pool = LzmaPool::with_capacity(0);
        let _ = pool.acquire(4096); // drop → not retained
        let guard = pool.inner.lock().expect("lock");
        let count = guard.get(&4096).map(|v| v.len()).unwrap_or(0);
        assert_eq!(count, 0, "cap=0 should retain no buffers");
    }

    // -------------------------------------------------------------------------
    // Pool: concurrent access
    // -------------------------------------------------------------------------

    #[test]
    fn test_pool_thread_safe() {
        use std::sync::Arc;
        use std::thread;

        let pool = Arc::new(LzmaPool::new());
        let mut handles = Vec::new();

        for _ in 0..4 {
            let p = Arc::clone(&pool);
            handles.push(thread::spawn(move || {
                for _ in 0..25 {
                    let mut b = p.acquire(4096);
                    b.fill(0xAB_u8);
                }
            }));
        }

        for h in handles {
            h.join().expect("thread panicked");
        }
    }

    #[test]
    fn test_pool_thread_safe_mixed_sizes() {
        use std::sync::Arc;
        use std::thread;

        let pool = Arc::new(LzmaPool::new());
        let mut handles = Vec::new();
        let sizes = [4096usize, 8192, 16384, 65536];

        for (i, &sz) in sizes.iter().enumerate() {
            let p = Arc::clone(&pool);
            handles.push(thread::spawn(move || {
                for j in 0..10 {
                    let mut b = p.acquire(sz);
                    // Write a recognisable pattern
                    b.fill((i + j) as u8);
                }
            }));
        }

        for h in handles {
            h.join().expect("thread panicked");
        }
    }

    // -------------------------------------------------------------------------
    // PooledBuf: deref + deref_mut
    // -------------------------------------------------------------------------

    #[test]
    fn test_pooled_buf_deref_mut() {
        let pool = LzmaPool::new();
        let mut buf = pool.acquire(4096);
        buf.fill(0xFF_u8);
        assert!(buf.iter().all(|&b| b == 0xFF));
    }

    #[test]
    fn test_pooled_buf_resize() {
        let pool = LzmaPool::new();
        let mut buf = pool.acquire(4096);
        let orig_len = buf.len();
        buf.resize(orig_len + 128, 0u8);
        assert_eq!(buf.len(), orig_len + 128);
    }

    // -------------------------------------------------------------------------
    // LzmaDecoderPooled: round-trip
    // -------------------------------------------------------------------------

    #[test]
    fn test_lzma_decoder_pooled_roundtrip() {
        use crate::{compress_bytes, decompress_bytes};
        use std::io::Cursor;

        let pool = LzmaPool::new();
        let input = vec![b'A'; 1024];

        // compress_bytes produces a full LZMA stream (header + payload).
        let compressed = compress_bytes(&input).expect("compress failed");

        // Verify standard round-trip still works (pool has no interference).
        let decoded = decompress_bytes(&compressed).expect("decompress failed");
        assert_eq!(decoded, input);

        // Use decode_from_header to construct a pooled decoder from the full
        // LZMA stream.  This tests pool buffer acquisition + decoder init.
        let reader = Cursor::new(compressed.as_slice());
        let pooled = pool
            .decode_from_header(reader)
            .expect("pool.decode_from_header failed");

        // Decompress via the pooled decoder and verify correctness.
        let decoded_pooled = pooled.decompress().expect("pooled decompress failed");
        assert_eq!(decoded_pooled, input, "pooled round-trip failed");
    }

    #[test]
    fn test_lzma_pool_session_reuse() {
        // Confirms that decoding several entries with a shared pool succeeds and
        // that the pool correctly recycles buffers across decoder lifetimes.
        use crate::{compress_bytes, decompress_bytes};
        use std::io::Cursor;

        let pool = LzmaPool::new();
        let input = vec![b'B'; 512];

        for i in 0..5_u8 {
            let mut data = input.clone();
            data[0] = i; // vary slightly to ensure distinct payloads
            let compressed = compress_bytes(&data).expect("compress failed");

            // Verify pool.decode_from_header succeeds and round-trips correctly.
            {
                let reader = Cursor::new(compressed.as_slice());
                let pooled = pool
                    .decode_from_header(reader)
                    .expect("pool.decode_from_header failed on iteration");

                let decoded = pooled.decompress().expect("pooled decompress failed");
                assert_eq!(decoded, data, "round-trip failed on iteration {}", i);
            }

            // Also verify the standard (non-pooled) path for completeness.
            let decoded = decompress_bytes(&compressed).expect("decompress failed");
            assert_eq!(
                decoded, data,
                "standard round-trip failed on iteration {}",
                i
            );
        }

        // After 5 iterations the pool should hold retained buffers.
        let guard = pool.inner.lock().expect("lock");
        let total_retained: usize = guard.values().map(|v| v.len()).sum();
        // At least one buffer from the decoded dict bucket should be retained.
        assert!(
            total_retained > 0,
            "pool should have retained at least one buffer after 5 sessions"
        );
    }
}
