//! Pure-Rust FIPS 180-4 SHA-256 implementation.
//!
//! Supports incremental ([`Sha256::update`]) and one-shot
//! ([`Sha256::compute`]) hashing. No external dependencies, no `unsafe`, no
//! `unwrap`.
//!
//! # Where this is used
//!
//! This module started as a private helper inside `oxiarc-lzma`'s `.xz`
//! reader/writer (the XZ container format's `CheckType::Sha256` block
//! check, xz spec §2.1.2). It moved here so it can be shared: `oxiarc-lzma`
//! still uses it for that check, and `oxiarc-http` can use it for the
//! dictionary-hash matching RFC 9842 (Compression Dictionary Transport)
//! defines for the `dcb`/`dcz` content codings, without either crate
//! depending on the other.
//!
//! # Example
//!
//! ```rust
//! use oxiarc_core::sha256::Sha256;
//!
//! // One-shot:
//! let digest = Sha256::compute(b"abc");
//! assert_eq!(
//!     oxiarc_core::sha256::hex32(&digest),
//!     "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
//! );
//!
//! // Incremental, in arbitrary chunk sizes -- the result does not depend on
//! // how the input was split:
//! let mut hasher = Sha256::new();
//! hasher.update(b"a");
//! hasher.update(b"b");
//! hasher.update(b"c");
//! assert_eq!(hasher.finalize(), digest);
//! ```

// ────────────────────────────────────────────────────────────
// FIPS 180-4 §5.3.3 — initial hash values H[0..8]:
// First 32 bits of the fractional parts of sqrt(first 8 primes).
const H0: [u32; 8] = [
    0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
];

// ────────────────────────────────────────────────────────────
// FIPS 180-4 §4.2.2 — round constants K[0..64]:
// First 32 bits of the fractional parts of cbrt(first 64 primes).
const K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

// ────────────────────────────────────────────────────────────
// FIPS 180-4 §4.1.2 auxiliary functions

/// Ch(x,y,z) = (x ∧ y) ⊕ (¬x ∧ z)
#[inline(always)]
const fn ch(x: u32, y: u32, z: u32) -> u32 {
    (x & y) ^ (!x & z)
}

/// Maj(x,y,z) = (x ∧ y) ⊕ (x ∧ z) ⊕ (y ∧ z)
#[inline(always)]
const fn maj(x: u32, y: u32, z: u32) -> u32 {
    (x & y) ^ (x & z) ^ (y & z)
}

/// Σ₀(x) = ROTR²(x) ⊕ ROTR¹³(x) ⊕ ROTR²²(x)
#[inline(always)]
const fn big_sigma0(x: u32) -> u32 {
    x.rotate_right(2) ^ x.rotate_right(13) ^ x.rotate_right(22)
}

/// Σ₁(x) = ROTR⁶(x) ⊕ ROTR¹¹(x) ⊕ ROTR²⁵(x)
#[inline(always)]
const fn big_sigma1(x: u32) -> u32 {
    x.rotate_right(6) ^ x.rotate_right(11) ^ x.rotate_right(25)
}

/// σ₀(x) = ROTR⁷(x) ⊕ ROTR¹⁸(x) ⊕ SHR³(x)
#[inline(always)]
const fn small_sigma0(x: u32) -> u32 {
    x.rotate_right(7) ^ x.rotate_right(18) ^ (x >> 3)
}

/// σ₁(x) = ROTR¹⁷(x) ⊕ ROTR¹⁹(x) ⊕ SHR¹⁰(x)
#[inline(always)]
const fn small_sigma1(x: u32) -> u32 {
    x.rotate_right(17) ^ x.rotate_right(19) ^ (x >> 10)
}

// ────────────────────────────────────────────────────────────
// Core 64-round block compression

/// Process a single 64-byte block, updating the hash state in place.
fn compress_block(state: &mut [u32; 8], block: &[u8; 64]) {
    // Build message schedule W[0..64]
    let mut w = [0u32; 64];

    // W[0..16]: big-endian u32 from the 64-byte block
    for (i, chunk) in block.chunks_exact(4).enumerate() {
        w[i] = u32::from_be_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
    }

    // W[16..64]: σ₁(W[t-2]) + W[t-7] + σ₀(W[t-15]) + W[t-16]
    // All additions are wrapping (finite-field arithmetic mod 2³²)
    for t in 16..64 {
        w[t] = small_sigma1(w[t - 2])
            .wrapping_add(w[t - 7])
            .wrapping_add(small_sigma0(w[t - 15]))
            .wrapping_add(w[t - 16]);
    }

    // Initialise working variables from current hash state
    let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = *state;

    // 64-round compression loop
    for t in 0..64 {
        let t1 = h
            .wrapping_add(big_sigma1(e))
            .wrapping_add(ch(e, f, g))
            .wrapping_add(K[t])
            .wrapping_add(w[t]);
        let t2 = big_sigma0(a).wrapping_add(maj(a, b, c));

        h = g;
        g = f;
        f = e;
        e = d.wrapping_add(t1);
        d = c;
        c = b;
        b = a;
        a = t1.wrapping_add(t2);
    }

    // Add compressed chunk to current hash value (wrapping throughout)
    state[0] = state[0].wrapping_add(a);
    state[1] = state[1].wrapping_add(b);
    state[2] = state[2].wrapping_add(c);
    state[3] = state[3].wrapping_add(d);
    state[4] = state[4].wrapping_add(e);
    state[5] = state[5].wrapping_add(f);
    state[6] = state[6].wrapping_add(g);
    state[7] = state[7].wrapping_add(h);
}

// ────────────────────────────────────────────────────────────
// Public API

/// Incremental SHA-256 hasher (FIPS 180-4).
pub struct Sha256 {
    state: [u32; 8],
    /// Partial block buffer, filled as `update` is called.
    buf: [u8; 64],
    /// Number of bytes currently held in `buf`.
    buf_len: usize,
    /// Total number of bytes processed so far (for the length field in padding).
    total_bytes: u64,
}

impl Sha256 {
    /// Create a new hasher with FIPS 180-4 initial values.
    pub const fn new() -> Self {
        Self {
            state: H0,
            buf: [0u8; 64],
            buf_len: 0,
            total_bytes: 0,
        }
    }

    /// Feed bytes into the hasher. May be called any number of times with
    /// chunks of arbitrary size; results are independent of chunk boundaries.
    pub fn update(&mut self, data: &[u8]) {
        let mut src = data;

        // If there are bytes already buffered, top the buffer up to 64 bytes
        // and compress if we reach a full block.
        if self.buf_len > 0 {
            let need = 64 - self.buf_len;
            let take = need.min(src.len());
            self.buf[self.buf_len..self.buf_len + take].copy_from_slice(&src[..take]);
            self.buf_len += take;
            src = &src[take..];

            if self.buf_len == 64 {
                let block: [u8; 64] = self.buf;
                compress_block(&mut self.state, &block);
                self.buf_len = 0;
            } else {
                // Buffer still not full; no more input to process.
                // buf_len was already updated above; just update total_bytes.
                self.total_bytes = self.total_bytes.saturating_add(data.len() as u64);
                return;
            }
        }

        // Process all remaining full 64-byte blocks directly from `src`.
        // (At this point buf_len == 0.)
        let full_blocks = src.len() / 64;
        for chunk in src[..full_blocks * 64].chunks_exact(64) {
            let mut block = [0u8; 64];
            block.copy_from_slice(chunk);
            compress_block(&mut self.state, &block);
        }

        // Keep any remaining tail bytes in the (now-empty) buffer.
        let tail = &src[full_blocks * 64..];
        self.buf[..tail.len()].copy_from_slice(tail);
        self.buf_len = tail.len();

        // Saturating-add is safe here: 2⁶⁴ bytes would take millennia.
        self.total_bytes = self.total_bytes.saturating_add(data.len() as u64);
    }

    /// Finalise and return the 32-byte digest, consuming `self`.
    ///
    /// FIPS 180-4 padding (§5.1.1):
    ///   1. Append 0x80.
    ///   2. Append zeros until the buffer length ≡ 56 (mod 64).
    ///   3. Append the 8-byte big-endian bit count.
    ///
    /// **Edge case:** if there are > 55 bytes already in the buffer after
    /// appending 0x80, we need an extra (overflow) padding block.
    pub fn finalize(mut self) -> [u8; 32] {
        // Bit count must be computed before we add the padding bytes.
        let bit_count: u64 = self.total_bytes.wrapping_mul(8);

        // Append 0x80
        self.buf[self.buf_len] = 0x80;
        self.buf_len += 1;

        // If the remaining space in the block is < 8 bytes (not enough for the
        // 64-bit length), flush the current block and start a new all-zero one.
        if self.buf_len > 56 {
            // Zero the rest of the current partial block and compress it.
            for byte in &mut self.buf[self.buf_len..] {
                *byte = 0;
            }
            let block: [u8; 64] = self.buf;
            compress_block(&mut self.state, &block);
            self.buf = [0u8; 64];
            self.buf_len = 0;
        }

        // Zero-pad to byte 56, then write big-endian bit count at [56..64].
        for byte in &mut self.buf[self.buf_len..56] {
            *byte = 0;
        }
        self.buf[56..64].copy_from_slice(&bit_count.to_be_bytes());

        // Compress the final block.
        let block: [u8; 64] = self.buf;
        compress_block(&mut self.state, &block);

        // Serialise state as 8 big-endian u32s → 32-byte digest.
        let mut digest = [0u8; 32];
        for (i, word) in self.state.iter().enumerate() {
            digest[i * 4..(i + 1) * 4].copy_from_slice(&word.to_be_bytes());
        }
        digest
    }

    /// One-shot convenience: hash `data` and return the digest.
    pub fn compute(data: &[u8]) -> [u8; 32] {
        let mut h = Self::new();
        h.update(data);
        h.finalize()
    }
}

impl Default for Sha256 {
    fn default() -> Self {
        Self::new()
    }
}

/// Format a 32-byte digest as a lowercase hexadecimal string.
pub fn hex32(digest: &[u8; 32]) -> String {
    digest.iter().fold(String::with_capacity(64), |mut acc, b| {
        use std::fmt::Write as _;
        let _ = write!(acc, "{b:02x}");
        acc
    })
}

// ────────────────────────────────────────────────────────────
#[cfg(test)]
mod tests {
    use super::*;

    // ── NIST FIPS 180-4 / NSRL test vectors ─────────────────

    #[test]
    fn fips_empty() {
        let digest = Sha256::compute(b"");
        assert_eq!(
            hex32(&digest),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn fips_abc() {
        let digest = Sha256::compute(b"abc");
        assert_eq!(
            hex32(&digest),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn fips_448bit() {
        let digest = Sha256::compute(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq");
        assert_eq!(
            hex32(&digest),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
    }

    #[test]
    fn fips_896bit() {
        let digest = Sha256::compute(
            b"abcdefghbcdefghicdefghijdefghijkefghijklfghijklmghijklmnhijklmnoijklmnopjklmnopqklmnopqrlmnopqrsmnopqrstnopqrstu",
        );
        assert_eq!(
            hex32(&digest),
            "cf5b16a778af8380036ce59e7b0492370b249b11e8f07a51afac45037afee9d1"
        );
    }

    #[test]
    fn fips_one_million_a() {
        // Pre-allocate a single large buffer; do NOT loop update() a million
        // times — that is correct but extremely slow under debug builds.
        let data = vec![b'a'; 1_000_000];
        let digest = Sha256::compute(&data);
        assert_eq!(
            hex32(&digest),
            "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
        );
    }

    // ── Chunk-size independence ──────────────────────────────

    fn reference_data() -> Vec<u8> {
        (0u8..=255).cycle().take(1000).collect()
    }

    #[test]
    fn chunk_independence_oneshot() {
        let data = reference_data();
        let expected = Sha256::compute(&data);

        // 1-byte chunks
        let mut h = Sha256::new();
        for byte in &data {
            h.update(std::slice::from_ref(byte));
        }
        assert_eq!(h.finalize(), expected, "1-byte chunks differ from one-shot");

        // 64-byte chunks
        let mut h = Sha256::new();
        for chunk in data.chunks(64) {
            h.update(chunk);
        }
        assert_eq!(
            h.finalize(),
            expected,
            "64-byte chunks differ from one-shot"
        );

        // 100-byte chunks
        let mut h = Sha256::new();
        for chunk in data.chunks(100) {
            h.update(chunk);
        }
        assert_eq!(
            h.finalize(),
            expected,
            "100-byte chunks differ from one-shot"
        );
    }

    #[test]
    fn default_matches_new() {
        assert_eq!(Sha256::default().finalize(), Sha256::new().finalize());
    }

    #[test]
    fn hex32_formats_lowercase() {
        let digest = Sha256::compute(b"");
        let formatted = hex32(&digest);
        assert_eq!(formatted.len(), 64);
        assert!(
            formatted
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
    }
}
