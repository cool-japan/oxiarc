//! Ring buffer (sliding window) for LZ77/LZSS decompression.
//!
//! This module provides a circular buffer that maintains a history of recently
//! output bytes, allowing back-references to previously seen data during
//! decompression.
//!
//! # Sizes
//!
//! Different compression methods use different window sizes:
//! - DEFLATE: 32 KB (32768 bytes)
//! - LZH lh4: 4 KB (4096 bytes)
//! - LZH lh5: 8 KB (8192 bytes)
//! - LZH lh6: 32 KB (32768 bytes)
//! - LZH lh7: 64 KB (65536 bytes)

use crate::error::{OxiArcError, Result};

/// Common window sizes for different compression methods.
pub mod sizes {
    /// Window size for DEFLATE (32 KB).
    pub const DEFLATE: usize = 32768;
    /// Window size for LZH lh4 (4 KB).
    pub const LZH_LH4: usize = 4096;
    /// Window size for LZH lh5 (8 KB).
    pub const LZH_LH5: usize = 8192;
    /// Window size for LZH lh6 (32 KB).
    pub const LZH_LH6: usize = 32768;
    /// Window size for LZH lh7 (64 KB).
    pub const LZH_LH7: usize = 65536;
}

/// A ring buffer (circular buffer) for maintaining decompression history.
///
/// The buffer stores the most recent `capacity` bytes of output data,
/// wrapping around when full. This allows efficient back-reference copying
/// during LZ77/LZSS decompression.
#[derive(Debug, Clone)]
pub struct RingBuffer {
    /// The underlying buffer.
    buffer: Vec<u8>,
    /// Current write position (next byte will be written here).
    position: usize,
    /// Number of bytes written (up to capacity).
    size: usize,
    /// Capacity (must be power of 2).
    capacity: usize,
    /// Mask for efficient modulo (capacity - 1).
    mask: usize,
}

impl RingBuffer {
    /// Create a new ring buffer with the specified capacity.
    ///
    /// # Arguments
    ///
    /// * `capacity` - Must be a power of 2 (e.g., 4096, 8192, 32768, 65536)
    ///
    /// # Panics
    ///
    /// Panics if `capacity` is not a power of 2 or is zero.
    pub fn new(capacity: usize) -> Self {
        assert!(capacity > 0, "Capacity must be greater than 0");
        assert!(
            capacity.is_power_of_two(),
            "Capacity must be a power of 2, got {}",
            capacity
        );

        Self {
            buffer: vec![0; capacity],
            position: 0,
            size: 0,
            capacity,
            mask: capacity - 1,
        }
    }

    /// Create a new ring buffer for DEFLATE decompression (32 KB).
    pub fn deflate() -> Self {
        Self::new(sizes::DEFLATE)
    }

    /// Create a new ring buffer for LZH lh5 decompression (8 KB).
    pub fn lzh_lh5() -> Self {
        Self::new(sizes::LZH_LH5)
    }

    /// Get the capacity of the buffer.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Get the number of bytes currently in the buffer.
    pub fn len(&self) -> usize {
        self.size
    }

    /// Check if the buffer is empty.
    pub fn is_empty(&self) -> bool {
        self.size == 0
    }

    /// Get the current write position.
    pub fn position(&self) -> usize {
        self.position
    }

    /// Clear the buffer.
    pub fn clear(&mut self) {
        self.position = 0;
        self.size = 0;
        // Optionally zero the buffer for security
        self.buffer.fill(0);
    }

    /// Write a single byte to the buffer.
    pub fn write_byte(&mut self, byte: u8) {
        self.buffer[self.position] = byte;
        self.position = (self.position + 1) & self.mask;
        if self.size < self.capacity {
            self.size += 1;
        }
    }

    /// Read a byte at the given distance from the current position.
    ///
    /// Distance 1 means the most recently written byte.
    /// Distance `capacity` means the oldest byte in the buffer.
    ///
    /// # Arguments
    ///
    /// * `distance` - The distance back from current position (1-based)
    ///
    /// # Returns
    ///
    /// The byte at that position, or an error if distance is invalid.
    pub fn read_at_distance(&self, distance: usize) -> Result<u8> {
        if distance == 0 || distance > self.size {
            return Err(OxiArcError::invalid_distance(distance, self.size));
        }

        let index = (self.position.wrapping_sub(distance)) & self.mask;
        Ok(self.buffer[index])
    }

    /// Copy bytes from a back-reference and write them to the buffer.
    ///
    /// This handles the case where the copy length exceeds the distance,
    /// which is valid in LZ77/LZSS and creates a repeating pattern.
    ///
    /// # Arguments
    ///
    /// * `distance` - Distance back from current position (1-based)
    /// * `length` - Number of bytes to copy
    /// * `output` - Optional output slice to also receive the copied bytes
    ///
    /// # Returns
    ///
    /// The number of bytes written to output (if provided).
    pub fn copy_from_history(
        &mut self,
        distance: usize,
        length: usize,
        mut output: Option<&mut [u8]>,
    ) -> Result<usize> {
        if distance == 0 || distance > self.size {
            return Err(OxiArcError::invalid_distance(distance, self.size));
        }

        let mut written = 0;

        // Start position in history
        let mut src_pos = (self.position.wrapping_sub(distance)) & self.mask;

        for _ in 0..length {
            let byte = self.buffer[src_pos];

            // Write to output if provided
            if let Some(ref mut out) = output {
                if written < out.len() {
                    out[written] = byte;
                    written += 1;
                }
            }

            // Write to ring buffer
            self.buffer[self.position] = byte;
            self.position = (self.position + 1) & self.mask;
            if self.size < self.capacity {
                self.size += 1;
            }

            // Advance source position (may wrap around)
            src_pos = (src_pos + 1) & self.mask;
        }

        Ok(written)
    }

    /// Write multiple bytes to the buffer.
    pub fn write_bytes(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.write_byte(byte);
        }
    }

    /// Get the last N bytes written (for debugging/testing).
    pub fn last_bytes(&self, count: usize) -> Vec<u8> {
        let count = count.min(self.size);
        let mut result = Vec::with_capacity(count);

        for i in 0..count {
            let index = (self.position.wrapping_sub(count - i)) & self.mask;
            result.push(self.buffer[index]);
        }

        result
    }

    /// Preload the ring buffer with dictionary data.
    ///
    /// This is used for custom dictionary support in DEFLATE/zlib.
    /// The dictionary is loaded into the history buffer, allowing
    /// back-references to dictionary content during decompression.
    ///
    /// If the dictionary is larger than the buffer capacity, only
    /// the last `capacity` bytes are used (as per zlib specification).
    ///
    /// # Arguments
    ///
    /// * `dictionary` - Dictionary data (typically up to 32KB)
    pub fn preload_dictionary(&mut self, dictionary: &[u8]) {
        // If dictionary is larger than capacity, use only the last capacity bytes
        let dict_to_use = if dictionary.len() > self.capacity {
            &dictionary[dictionary.len() - self.capacity..]
        } else {
            dictionary
        };

        // Copy dictionary to buffer
        for &byte in dict_to_use {
            self.buffer[self.position] = byte;
            self.position = (self.position + 1) & self.mask;
        }

        // Set size to the amount of dictionary data loaded
        self.size = dict_to_use.len().min(self.capacity);
    }
}

/// Snapshot of ring buffer state for partial-delivery rollback.
///
/// Created by [`RingBuffer::snapshot`] and restored via [`RingBuffer::restore_from`].
#[derive(Clone, Debug)]
pub struct RingSnapshot {
    buffer: Vec<u8>,
    position: usize,
    size: usize,
}

impl RingBuffer {
    /// Take a snapshot of current state for rollback on partial-input failure.
    pub fn snapshot(&self) -> RingSnapshot {
        RingSnapshot {
            buffer: self.buffer.clone(),
            position: self.position,
            size: self.size,
        }
    }

    /// Restore from a previously taken snapshot.
    pub fn restore_from(&mut self, snap: &RingSnapshot) {
        debug_assert_eq!(
            snap.buffer.len(),
            self.capacity,
            "snapshot capacity mismatch"
        );
        self.buffer.copy_from_slice(&snap.buffer);
        self.position = snap.position;
        self.size = snap.size;
    }
}

/// A ring buffer that also accumulates output data.
///
/// This is useful when you need both the sliding window for back-references
/// and a growable output buffer for the decompressed data.
#[derive(Debug)]
pub struct OutputRingBuffer {
    /// The ring buffer for history.
    ring: RingBuffer,
    /// Accumulated output.
    output: Vec<u8>,
}

impl OutputRingBuffer {
    /// Create a new output ring buffer.
    pub fn new(window_size: usize) -> Self {
        Self {
            ring: RingBuffer::new(window_size),
            output: Vec::new(),
        }
    }

    /// Create with an initial output capacity hint.
    pub fn with_capacity(window_size: usize, output_capacity: usize) -> Self {
        Self {
            ring: RingBuffer::new(window_size),
            output: Vec::with_capacity(output_capacity),
        }
    }

    /// Write a literal byte.
    pub fn write_literal(&mut self, byte: u8) {
        self.ring.write_byte(byte);
        self.output.push(byte);
    }

    /// Write multiple literal bytes.
    pub fn write_literals(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.write_literal(byte);
        }
    }

    /// Copy from back-reference.
    pub fn copy_match(&mut self, distance: usize, length: usize) -> Result<()> {
        if distance == 0 || distance > self.ring.len() {
            return Err(OxiArcError::invalid_distance(distance, self.ring.len()));
        }

        // Reserve space for efficiency
        self.output.reserve(length);

        let mut src_pos =
            (self.ring.position().wrapping_sub(distance)) & (self.ring.capacity() - 1);

        for _ in 0..length {
            let byte = self.ring.buffer[src_pos];
            self.ring.write_byte(byte);
            self.output.push(byte);
            src_pos = (src_pos + 1) & (self.ring.capacity() - 1);
        }

        Ok(())
    }

    /// Get the total output length.
    pub fn output_len(&self) -> usize {
        self.output.len()
    }

    /// Get the output data.
    pub fn output(&self) -> &[u8] {
        &self.output
    }

    /// Consume and return the output data.
    pub fn into_output(self) -> Vec<u8> {
        self.output
    }

    /// Clear both the ring buffer and output.
    pub fn clear(&mut self) {
        self.ring.clear();
        self.output.clear();
    }

    /// Get the ring buffer for direct access.
    pub fn ring(&self) -> &RingBuffer {
        &self.ring
    }

    /// Preload the ring buffer with dictionary data.
    ///
    /// This is used for custom dictionary support in DEFLATE/zlib.
    /// The dictionary is loaded into the history (ring buffer) but NOT
    /// included in the output. This allows back-references into the
    /// dictionary during decompression.
    ///
    /// # Arguments
    ///
    /// * `dictionary` - Dictionary data (up to window_size bytes, typically 32KB max)
    pub fn preload_dictionary(&mut self, dictionary: &[u8]) {
        self.ring.preload_dictionary(dictionary);
        // Note: Dictionary is NOT added to output, only to history
    }

    /// Drain all accumulated output while PRESERVING the LZ77 sliding window.
    ///
    /// This is the streaming primitive: call it after each sync-flush unit to
    /// collect decompressed bytes, then continue feeding the next unit.  The
    /// ring-buffer history is untouched so distance back-references across flush
    /// boundaries remain valid (RFC 4978 §3 requires this).
    pub fn drain_output(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.output)
    }

    /// Take a ring-state snapshot for rollback on partial-input failure.
    pub fn ring_snapshot(&self) -> RingSnapshot {
        self.ring.snapshot()
    }

    /// Restore ring state and truncate output to `output_len`.
    ///
    /// Used after a failed partial-block attempt to undo all side-effects of
    /// the failed `inflate_block` call before retrying with more input.
    pub fn restore_ring(&mut self, snap: &RingSnapshot, output_len: usize) {
        self.ring.restore_from(snap);
        self.output.truncate(output_len);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ringbuffer_basic() {
        let mut ring = RingBuffer::new(8);

        ring.write_byte(b'H');
        ring.write_byte(b'e');
        ring.write_byte(b'l');
        ring.write_byte(b'l');
        ring.write_byte(b'o');

        assert_eq!(ring.len(), 5);
        assert_eq!(
            ring.read_at_distance(1)
                .expect("read at distance 1 should return last byte"),
            b'o'
        );
        assert_eq!(
            ring.read_at_distance(2)
                .expect("read at distance 2 should return second-to-last byte"),
            b'l'
        );
        assert_eq!(
            ring.read_at_distance(5)
                .expect("read at distance 5 should return first byte"),
            b'H'
        );
    }

    #[test]
    fn test_ringbuffer_wrap() {
        let mut ring = RingBuffer::new(4);

        ring.write_bytes(b"ABCDEF"); // Wraps around

        assert_eq!(ring.len(), 4); // Max is capacity
        assert_eq!(
            ring.read_at_distance(1)
                .expect("read at distance 1 after wrap should return last byte"),
            b'F'
        );
        assert_eq!(
            ring.read_at_distance(2)
                .expect("read at distance 2 after wrap should return second-to-last byte"),
            b'E'
        );
        assert_eq!(
            ring.read_at_distance(3)
                .expect("read at distance 3 after wrap should return third-to-last byte"),
            b'D'
        );
        assert_eq!(
            ring.read_at_distance(4)
                .expect("read at distance 4 after wrap should return fourth-to-last byte"),
            b'C'
        );
    }

    #[test]
    fn test_ringbuffer_copy_match() {
        let mut ring = RingBuffer::new(32);
        let mut output = [0u8; 10];

        // Write "ABCD"
        ring.write_bytes(b"ABCD");

        // Copy distance=4, length=4 -> "ABCD"
        let written = ring
            .copy_from_history(4, 4, Some(&mut output))
            .expect("copy from history should succeed for valid distance and length");
        assert_eq!(written, 4);
        assert_eq!(&output[..4], b"ABCD");
    }

    #[test]
    fn test_ringbuffer_copy_overlap() {
        // This tests the case where length > distance
        // e.g., "AB" -> copy distance=2, length=6 -> "ABABAB"
        let mut ring = RingBuffer::new(32);
        let mut output = [0u8; 10];

        ring.write_bytes(b"AB");

        let written = ring
            .copy_from_history(2, 6, Some(&mut output))
            .expect("copy from history should succeed for overlapping back-reference");
        assert_eq!(written, 6);
        assert_eq!(&output[..6], b"ABABAB");
    }

    #[test]
    fn test_ringbuffer_single_byte_repeat() {
        // distance=1, length=5 -> repeat last byte 5 times
        let mut ring = RingBuffer::new(32);
        let mut output = [0u8; 10];

        ring.write_byte(b'X');

        let written = ring
            .copy_from_history(1, 5, Some(&mut output))
            .expect("copy from history should succeed for single-byte repeat");
        assert_eq!(written, 5);
        assert_eq!(&output[..5], b"XXXXX");
    }

    #[test]
    fn test_ringbuffer_invalid_distance() {
        let ring = RingBuffer::new(32);

        assert!(ring.read_at_distance(0).is_err());
        assert!(ring.read_at_distance(1).is_err()); // Empty buffer
    }

    #[test]
    fn test_output_ringbuffer() {
        let mut orb = OutputRingBuffer::new(32);

        orb.write_literals(b"Hello");
        orb.copy_match(5, 5)
            .expect("copy match should succeed when history contains enough data"); // Copy "Hello" again

        assert_eq!(orb.output(), b"HelloHello");
    }

    #[test]
    fn test_last_bytes() {
        let mut ring = RingBuffer::new(8);
        ring.write_bytes(b"Hello, World!");

        let last_5 = ring.last_bytes(5);
        assert_eq!(last_5, b"orld!");
    }

    #[test]
    #[should_panic(expected = "power of 2")]
    fn test_non_power_of_two_panics() {
        let _ = RingBuffer::new(100);
    }

    #[test]
    fn test_drain_output_preserves_ring() {
        let mut orb = OutputRingBuffer::new(32);
        orb.write_literals(b"Hello");
        let drained = orb.drain_output();
        assert_eq!(drained, b"Hello");
        assert_eq!(orb.output_len(), 0);
        // Ring must still contain the history
        orb.copy_match(5, 5)
            .expect("copy match should succeed after drain since ring still holds history"); // back-ref to "Hello"
        assert_eq!(orb.output(), b"Hello");
    }

    #[test]
    fn test_restore_ring_rollback() {
        let mut orb = OutputRingBuffer::new(32);
        orb.write_literals(b"ABC");
        let snap = orb.ring_snapshot();
        let out_len = orb.output_len();
        // Simulate partial work
        orb.write_literals(b"XYZ");
        assert_eq!(orb.output(), b"ABCXYZ");
        // Rollback
        orb.restore_ring(&snap, out_len);
        assert_eq!(orb.output(), b"ABC");
        // Ring should be back to ABC state — write a literal that would have
        // conflicted with XYZ to confirm ring is rolled back
        orb.write_literals(b"DEF");
        assert_eq!(orb.output(), b"ABCDEF");
    }
}
