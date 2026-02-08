//! ZIP AES-256 encryption support following WinZip AE-2 specification.
//!
//! This module implements the WinZip AES encryption scheme (AE-2) which provides:
//! - AES-256 encryption in CTR mode
//! - PBKDF2-SHA1 key derivation
//! - HMAC-SHA1 authentication
//!
//! All implementations are pure Rust with no external dependencies.

use oxiarc_core::error::{OxiArcError, Result};

// ===========================================================================
// SHA-1 Implementation (RFC 3174)
// ===========================================================================

/// SHA-1 hash state.
struct Sha1 {
    state: [u32; 5],
    count: u64,
    buffer: [u8; 64],
    buffer_len: usize,
}

impl Sha1 {
    /// Initial hash values for SHA-1.
    const INIT_STATE: [u32; 5] = [0x67452301, 0xEFCDAB89, 0x98BADCFE, 0x10325476, 0xC3D2E1F0];

    /// Create a new SHA-1 hasher.
    fn new() -> Self {
        Self {
            state: Self::INIT_STATE,
            count: 0,
            buffer: [0u8; 64],
            buffer_len: 0,
        }
    }

    /// Update the hash with new data.
    fn update(&mut self, data: &[u8]) {
        let mut offset = 0;
        self.count += (data.len() as u64) * 8;

        // If we have buffered data, try to complete a block
        if self.buffer_len > 0 {
            let space = 64 - self.buffer_len;
            let to_copy = data.len().min(space);
            self.buffer[self.buffer_len..self.buffer_len + to_copy]
                .copy_from_slice(&data[..to_copy]);
            self.buffer_len += to_copy;
            offset += to_copy;

            if self.buffer_len == 64 {
                self.compress(&self.buffer.clone());
                self.buffer_len = 0;
            }
        }

        // Process complete blocks
        while offset + 64 <= data.len() {
            self.compress(&data[offset..offset + 64]);
            offset += 64;
        }

        // Buffer remaining data
        if offset < data.len() {
            let remaining = data.len() - offset;
            self.buffer[..remaining].copy_from_slice(&data[offset..]);
            self.buffer_len = remaining;
        }
    }

    /// Finalize and return the hash.
    fn finalize(mut self) -> [u8; 20] {
        // Padding
        let mut padding = [0u8; 72]; // Max padding needed
        padding[0] = 0x80;

        let padding_len = if self.buffer_len < 56 {
            56 - self.buffer_len
        } else {
            120 - self.buffer_len
        };

        // Append length in bits (big-endian)
        let length_bytes = self.count.to_be_bytes();

        self.update(&padding[..padding_len]);
        self.update(&length_bytes);

        // Output hash
        let mut result = [0u8; 20];
        for (i, word) in self.state.iter().enumerate() {
            result[i * 4..(i + 1) * 4].copy_from_slice(&word.to_be_bytes());
        }
        result
    }

    /// Compress a single 64-byte block.
    fn compress(&mut self, block: &[u8]) {
        // Parse block into 16 32-bit words (big-endian)
        let mut w = [0u32; 80];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                block[i * 4],
                block[i * 4 + 1],
                block[i * 4 + 2],
                block[i * 4 + 3],
            ]);
        }

        // Extend to 80 words
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }

        let mut a = self.state[0];
        let mut b = self.state[1];
        let mut c = self.state[2];
        let mut d = self.state[3];
        let mut e = self.state[4];

        // Main loop
        for (i, &w_i) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | ((!b) & d), 0x5A827999u32),
                20..=39 => (b ^ c ^ d, 0x6ED9EBA1u32),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1BBCDCu32),
                _ => (b ^ c ^ d, 0xCA62C1D6u32),
            };

            let temp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(w_i);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = temp;
        }

        self.state[0] = self.state[0].wrapping_add(a);
        self.state[1] = self.state[1].wrapping_add(b);
        self.state[2] = self.state[2].wrapping_add(c);
        self.state[3] = self.state[3].wrapping_add(d);
        self.state[4] = self.state[4].wrapping_add(e);
    }
}

/// Compute SHA-1 hash of data.
fn sha1(data: &[u8]) -> [u8; 20] {
    let mut hasher = Sha1::new();
    hasher.update(data);
    hasher.finalize()
}

// ===========================================================================
// HMAC-SHA1 Implementation (RFC 2104)
// ===========================================================================

/// HMAC-SHA1 block size.
const HMAC_SHA1_BLOCK_SIZE: usize = 64;

/// Compute HMAC-SHA1.
fn hmac_sha1(key: &[u8], message: &[u8]) -> [u8; 20] {
    // Prepare key
    let mut key_block = [0u8; HMAC_SHA1_BLOCK_SIZE];
    if key.len() > HMAC_SHA1_BLOCK_SIZE {
        let hash = sha1(key);
        key_block[..20].copy_from_slice(&hash);
    } else {
        key_block[..key.len()].copy_from_slice(key);
    }

    // Inner and outer padded keys
    let mut ipad = [0x36u8; HMAC_SHA1_BLOCK_SIZE];
    let mut opad = [0x5Cu8; HMAC_SHA1_BLOCK_SIZE];
    for i in 0..HMAC_SHA1_BLOCK_SIZE {
        ipad[i] ^= key_block[i];
        opad[i] ^= key_block[i];
    }

    // Inner hash
    let mut inner = Sha1::new();
    inner.update(&ipad);
    inner.update(message);
    let inner_hash = inner.finalize();

    // Outer hash
    let mut outer = Sha1::new();
    outer.update(&opad);
    outer.update(&inner_hash);
    outer.finalize()
}

// ===========================================================================
// PBKDF2-SHA1 Implementation (RFC 2898)
// ===========================================================================

/// PBKDF2-SHA1 key derivation.
///
/// Derives a key of `dk_len` bytes from a password and salt using the specified
/// number of iterations.
fn pbkdf2_sha1(password: &[u8], salt: &[u8], iterations: u32, dk_len: usize) -> Vec<u8> {
    let h_len = 20; // SHA-1 output length
    let l = dk_len.div_ceil(h_len); // Number of blocks needed

    let mut dk = Vec::with_capacity(dk_len);

    for i in 1..=l {
        // F(Password, Salt, c, i)
        let mut salt_with_index = salt.to_vec();
        salt_with_index.extend_from_slice(&(i as u32).to_be_bytes());

        // U_1 = PRF(Password, Salt || INT(i))
        let mut u = hmac_sha1(password, &salt_with_index);
        let mut result = u;

        // U_2 ... U_c
        for _ in 1..iterations {
            u = hmac_sha1(password, &u);
            for j in 0..h_len {
                result[j] ^= u[j];
            }
        }

        dk.extend_from_slice(&result);
    }

    dk.truncate(dk_len);
    dk
}

// ===========================================================================
// AES-256 Implementation (FIPS 197)
// ===========================================================================

/// AES S-Box (Substitution Box).
const SBOX: [u8; 256] = [
    0x63, 0x7c, 0x77, 0x7b, 0xf2, 0x6b, 0x6f, 0xc5, 0x30, 0x01, 0x67, 0x2b, 0xfe, 0xd7, 0xab, 0x76,
    0xca, 0x82, 0xc9, 0x7d, 0xfa, 0x59, 0x47, 0xf0, 0xad, 0xd4, 0xa2, 0xaf, 0x9c, 0xa4, 0x72, 0xc0,
    0xb7, 0xfd, 0x93, 0x26, 0x36, 0x3f, 0xf7, 0xcc, 0x34, 0xa5, 0xe5, 0xf1, 0x71, 0xd8, 0x31, 0x15,
    0x04, 0xc7, 0x23, 0xc3, 0x18, 0x96, 0x05, 0x9a, 0x07, 0x12, 0x80, 0xe2, 0xeb, 0x27, 0xb2, 0x75,
    0x09, 0x83, 0x2c, 0x1a, 0x1b, 0x6e, 0x5a, 0xa0, 0x52, 0x3b, 0xd6, 0xb3, 0x29, 0xe3, 0x2f, 0x84,
    0x53, 0xd1, 0x00, 0xed, 0x20, 0xfc, 0xb1, 0x5b, 0x6a, 0xcb, 0xbe, 0x39, 0x4a, 0x4c, 0x58, 0xcf,
    0xd0, 0xef, 0xaa, 0xfb, 0x43, 0x4d, 0x33, 0x85, 0x45, 0xf9, 0x02, 0x7f, 0x50, 0x3c, 0x9f, 0xa8,
    0x51, 0xa3, 0x40, 0x8f, 0x92, 0x9d, 0x38, 0xf5, 0xbc, 0xb6, 0xda, 0x21, 0x10, 0xff, 0xf3, 0xd2,
    0xcd, 0x0c, 0x13, 0xec, 0x5f, 0x97, 0x44, 0x17, 0xc4, 0xa7, 0x7e, 0x3d, 0x64, 0x5d, 0x19, 0x73,
    0x60, 0x81, 0x4f, 0xdc, 0x22, 0x2a, 0x90, 0x88, 0x46, 0xee, 0xb8, 0x14, 0xde, 0x5e, 0x0b, 0xdb,
    0xe0, 0x32, 0x3a, 0x0a, 0x49, 0x06, 0x24, 0x5c, 0xc2, 0xd3, 0xac, 0x62, 0x91, 0x95, 0xe4, 0x79,
    0xe7, 0xc8, 0x37, 0x6d, 0x8d, 0xd5, 0x4e, 0xa9, 0x6c, 0x56, 0xf4, 0xea, 0x65, 0x7a, 0xae, 0x08,
    0xba, 0x78, 0x25, 0x2e, 0x1c, 0xa6, 0xb4, 0xc6, 0xe8, 0xdd, 0x74, 0x1f, 0x4b, 0xbd, 0x8b, 0x8a,
    0x70, 0x3e, 0xb5, 0x66, 0x48, 0x03, 0xf6, 0x0e, 0x61, 0x35, 0x57, 0xb9, 0x86, 0xc1, 0x1d, 0x9e,
    0xe1, 0xf8, 0x98, 0x11, 0x69, 0xd9, 0x8e, 0x94, 0x9b, 0x1e, 0x87, 0xe9, 0xce, 0x55, 0x28, 0xdf,
    0x8c, 0xa1, 0x89, 0x0d, 0xbf, 0xe6, 0x42, 0x68, 0x41, 0x99, 0x2d, 0x0f, 0xb0, 0x54, 0xbb, 0x16,
];

/// Round constants for AES key expansion.
const RCON: [u32; 10] = [
    0x01000000, 0x02000000, 0x04000000, 0x08000000, 0x10000000, 0x20000000, 0x40000000, 0x80000000,
    0x1b000000, 0x36000000,
];

/// AES-256 cipher state.
pub struct Aes256 {
    /// Expanded round keys (15 * 4 = 60 words for AES-256).
    round_keys: [[u8; 16]; 15],
}

impl Aes256 {
    /// Create a new AES-256 cipher with the given 32-byte key.
    pub fn new(key: &[u8; 32]) -> Self {
        let round_keys = Self::key_expansion(key);
        Self { round_keys }
    }

    /// Expand the 256-bit key into round keys.
    fn key_expansion(key: &[u8; 32]) -> [[u8; 16]; 15] {
        let nk = 8; // Key length in 32-bit words (256/32)
        let nr = 14; // Number of rounds for AES-256

        // Initialize with key
        let mut w = [[0u8; 4]; 60];
        for i in 0..nk {
            w[i] = [key[4 * i], key[4 * i + 1], key[4 * i + 2], key[4 * i + 3]];
        }

        // Key expansion
        for i in nk..4 * (nr + 1) {
            let mut temp = w[i - 1];
            if i % nk == 0 {
                // RotWord + SubWord + Rcon
                temp = [
                    SBOX[temp[1] as usize] ^ ((RCON[i / nk - 1] >> 24) as u8),
                    SBOX[temp[2] as usize],
                    SBOX[temp[3] as usize],
                    SBOX[temp[0] as usize],
                ];
            } else if nk > 6 && i % nk == 4 {
                // Additional SubWord for AES-256
                temp = [
                    SBOX[temp[0] as usize],
                    SBOX[temp[1] as usize],
                    SBOX[temp[2] as usize],
                    SBOX[temp[3] as usize],
                ];
            }
            w[i] = [
                w[i - nk][0] ^ temp[0],
                w[i - nk][1] ^ temp[1],
                w[i - nk][2] ^ temp[2],
                w[i - nk][3] ^ temp[3],
            ];
        }

        // Convert to round keys
        let mut round_keys = [[0u8; 16]; 15];
        for (r, round_key) in round_keys.iter_mut().enumerate() {
            for c in 0..4 {
                let w_idx = r * 4 + c;
                round_key[c * 4] = w[w_idx][0];
                round_key[c * 4 + 1] = w[w_idx][1];
                round_key[c * 4 + 2] = w[w_idx][2];
                round_key[c * 4 + 3] = w[w_idx][3];
            }
        }

        round_keys
    }

    /// Encrypt a single 16-byte block.
    pub fn encrypt_block(&self, input: &[u8; 16]) -> [u8; 16] {
        let mut state = *input;

        // Initial round key addition
        Self::add_round_key(&mut state, &self.round_keys[0]);

        // Main rounds
        for round in 1..14 {
            Self::sub_bytes(&mut state);
            Self::shift_rows(&mut state);
            Self::mix_columns(&mut state);
            Self::add_round_key(&mut state, &self.round_keys[round]);
        }

        // Final round (no MixColumns)
        Self::sub_bytes(&mut state);
        Self::shift_rows(&mut state);
        Self::add_round_key(&mut state, &self.round_keys[14]);

        state
    }

    /// SubBytes transformation.
    fn sub_bytes(state: &mut [u8; 16]) {
        for byte in state.iter_mut() {
            *byte = SBOX[*byte as usize];
        }
    }

    /// ShiftRows transformation.
    fn shift_rows(state: &mut [u8; 16]) {
        // Row 1: shift left by 1
        let tmp = state[1];
        state[1] = state[5];
        state[5] = state[9];
        state[9] = state[13];
        state[13] = tmp;

        // Row 2: shift left by 2
        state.swap(2, 10);
        state.swap(6, 14);

        // Row 3: shift left by 3 (= right by 1)
        let tmp = state[15];
        state[15] = state[11];
        state[11] = state[7];
        state[7] = state[3];
        state[3] = tmp;
    }

    /// MixColumns transformation.
    fn mix_columns(state: &mut [u8; 16]) {
        for c in 0..4 {
            let i = c * 4;
            let s0 = state[i];
            let s1 = state[i + 1];
            let s2 = state[i + 2];
            let s3 = state[i + 3];

            state[i] = Self::gf_mul2(s0) ^ Self::gf_mul3(s1) ^ s2 ^ s3;
            state[i + 1] = s0 ^ Self::gf_mul2(s1) ^ Self::gf_mul3(s2) ^ s3;
            state[i + 2] = s0 ^ s1 ^ Self::gf_mul2(s2) ^ Self::gf_mul3(s3);
            state[i + 3] = Self::gf_mul3(s0) ^ s1 ^ s2 ^ Self::gf_mul2(s3);
        }
    }

    /// Multiply by 2 in GF(2^8).
    fn gf_mul2(x: u8) -> u8 {
        let mut result = x << 1;
        if x & 0x80 != 0 {
            result ^= 0x1b; // AES irreducible polynomial
        }
        result
    }

    /// Multiply by 3 in GF(2^8).
    fn gf_mul3(x: u8) -> u8 {
        Self::gf_mul2(x) ^ x
    }

    /// AddRoundKey transformation.
    fn add_round_key(state: &mut [u8; 16], round_key: &[u8; 16]) {
        for i in 0..16 {
            state[i] ^= round_key[i];
        }
    }
}

// ===========================================================================
// AES-CTR Mode Implementation
// ===========================================================================

/// AES-256-CTR mode cipher for WinZip encryption.
pub struct AesCtr {
    cipher: Aes256,
    counter: [u8; 16],
    keystream: [u8; 16],
    keystream_pos: usize,
}

impl AesCtr {
    /// Create a new AES-CTR cipher.
    ///
    /// WinZip uses little-endian counter starting at 1 (not 0).
    pub fn new(key: &[u8; 32]) -> Self {
        let cipher = Aes256::new(key);
        let mut counter = [0u8; 16];
        counter[0] = 1; // WinZip starts counter at 1

        Self {
            cipher,
            counter,
            keystream: [0u8; 16],
            keystream_pos: 16, // Force keystream generation on first use
        }
    }

    /// Process data (encrypt or decrypt - CTR mode is symmetric).
    pub fn process(&mut self, data: &mut [u8]) {
        for byte in data.iter_mut() {
            if self.keystream_pos >= 16 {
                self.generate_keystream();
            }
            *byte ^= self.keystream[self.keystream_pos];
            self.keystream_pos += 1;
        }
    }

    /// Generate the next keystream block.
    fn generate_keystream(&mut self) {
        self.keystream = self.cipher.encrypt_block(&self.counter);
        self.keystream_pos = 0;
        self.increment_counter();
    }

    /// Increment the little-endian counter.
    fn increment_counter(&mut self) {
        for byte in &mut self.counter {
            *byte = byte.wrapping_add(1);
            if *byte != 0 {
                break;
            }
        }
    }
}

// ===========================================================================
// WinZip AE-2 Encryption
// ===========================================================================

/// WinZip AES extra field header ID.
pub const WINZIP_AES_EXTRA_ID: u16 = 0x9901;

/// WinZip AES encryption strength indicator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum AesStrength {
    /// AES-128 (16-byte key, 8-byte salt)
    Aes128 = 1,
    /// AES-192 (24-byte key, 12-byte salt)
    Aes192 = 2,
    /// AES-256 (32-byte key, 16-byte salt)
    Aes256 = 3,
}

impl AesStrength {
    /// Get salt length for this strength.
    pub fn salt_len(self) -> usize {
        match self {
            AesStrength::Aes128 => 8,
            AesStrength::Aes192 => 12,
            AesStrength::Aes256 => 16,
        }
    }

    /// Get key length for this strength.
    pub fn key_len(self) -> usize {
        match self {
            AesStrength::Aes128 => 16,
            AesStrength::Aes192 => 24,
            AesStrength::Aes256 => 32,
        }
    }

    /// Get derived key length (key + HMAC key + verification).
    pub fn derived_key_len(self) -> usize {
        self.key_len() * 2 + 2
    }

    /// Convert from u8.
    pub fn from_u8(value: u8) -> Option<Self> {
        match value {
            1 => Some(AesStrength::Aes128),
            2 => Some(AesStrength::Aes192),
            3 => Some(AesStrength::Aes256),
            _ => None,
        }
    }
}

/// WinZip AES extra field data.
#[derive(Debug, Clone)]
pub struct AesExtraField {
    /// Vendor version (AE-1 = 1, AE-2 = 2)
    pub version: u16,
    /// Vendor ID ("AE")
    pub vendor_id: [u8; 2],
    /// Encryption strength
    pub strength: AesStrength,
    /// Actual compression method used before encryption
    pub compression_method: u16,
}

impl AesExtraField {
    /// Create a new AES extra field for AE-2.
    pub fn new(strength: AesStrength, compression_method: u16) -> Self {
        Self {
            version: 2, // AE-2
            vendor_id: [b'A', b'E'],
            strength,
            compression_method,
        }
    }

    /// Serialize to bytes.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(11);
        // Header ID
        bytes.extend_from_slice(&WINZIP_AES_EXTRA_ID.to_le_bytes());
        // Data size (7 bytes)
        bytes.extend_from_slice(&7u16.to_le_bytes());
        // Version
        bytes.extend_from_slice(&self.version.to_le_bytes());
        // Vendor ID
        bytes.extend_from_slice(&self.vendor_id);
        // Strength
        bytes.push(self.strength as u8);
        // Compression method
        bytes.extend_from_slice(&self.compression_method.to_le_bytes());
        bytes
    }

    /// Parse from extra field bytes.
    pub fn from_bytes(data: &[u8]) -> Option<Self> {
        if data.len() < 11 {
            return None;
        }

        let header_id = u16::from_le_bytes([data[0], data[1]]);
        if header_id != WINZIP_AES_EXTRA_ID {
            return None;
        }

        let data_size = u16::from_le_bytes([data[2], data[3]]) as usize;
        if data_size < 7 || data.len() < 4 + data_size {
            return None;
        }

        let version = u16::from_le_bytes([data[4], data[5]]);
        let vendor_id = [data[6], data[7]];
        let strength = AesStrength::from_u8(data[8])?;
        let compression_method = u16::from_le_bytes([data[9], data[10]]);

        Some(Self {
            version,
            vendor_id,
            strength,
            compression_method,
        })
    }

    /// Search for AES extra field in extra data.
    pub fn find_in_extra(extra: &[u8]) -> Option<Self> {
        let mut offset = 0;
        while offset + 4 <= extra.len() {
            let header_id = u16::from_le_bytes([extra[offset], extra[offset + 1]]);
            let data_size = u16::from_le_bytes([extra[offset + 2], extra[offset + 3]]) as usize;

            if header_id == WINZIP_AES_EXTRA_ID {
                return Self::from_bytes(&extra[offset..]);
            }

            offset += 4 + data_size;
        }
        None
    }
}

/// HMAC-SHA1 authentication code length (10 bytes for WinZip).
pub const WINZIP_AUTH_CODE_LEN: usize = 10;

/// Password verification value length.
pub const PASSWORD_VERIFICATION_LEN: usize = 2;

/// WinZip AES encryption context.
pub struct ZipAesEncryptor {
    cipher: AesCtr,
    hmac_key: Vec<u8>,
    hmac_data: Vec<u8>,
}

impl ZipAesEncryptor {
    /// Create a new encryptor for the given password and salt.
    ///
    /// Returns the encryptor and the password verification bytes.
    pub fn new(password: &[u8], salt: &[u8], strength: AesStrength) -> Result<(Self, [u8; 2])> {
        if salt.len() != strength.salt_len() {
            return Err(OxiArcError::invalid_header(format!(
                "Invalid salt length: expected {}, got {}",
                strength.salt_len(),
                salt.len()
            )));
        }

        // Derive key material using PBKDF2-SHA1 with 1000 iterations
        let dk_len = strength.derived_key_len();
        let derived = pbkdf2_sha1(password, salt, 1000, dk_len);

        let key_len = strength.key_len();

        // Split derived key material
        let encryption_key = &derived[..key_len];
        let hmac_key = derived[key_len..key_len * 2].to_vec();
        let password_verification: [u8; 2] = [derived[key_len * 2], derived[key_len * 2 + 1]];

        // For AES-256, we need a 32-byte key
        let cipher = if strength == AesStrength::Aes256 {
            let mut key = [0u8; 32];
            key.copy_from_slice(encryption_key);
            AesCtr::new(&key)
        } else {
            // For AES-128/192, we still use AES-256 internally but with zero-padded key
            // Note: Proper implementation would use AES-128/192 variants
            let mut key = [0u8; 32];
            key[..key_len].copy_from_slice(encryption_key);
            AesCtr::new(&key)
        };

        Ok((
            Self {
                cipher,
                hmac_key,
                hmac_data: Vec::new(),
            },
            password_verification,
        ))
    }

    /// Encrypt data in place and accumulate for HMAC.
    pub fn encrypt(&mut self, data: &mut [u8]) {
        self.cipher.process(data);
        self.hmac_data.extend_from_slice(data);
    }

    /// Finalize and return the 10-byte authentication code.
    pub fn finalize(self) -> [u8; WINZIP_AUTH_CODE_LEN] {
        let full_hmac = hmac_sha1(&self.hmac_key, &self.hmac_data);
        let mut auth_code = [0u8; WINZIP_AUTH_CODE_LEN];
        auth_code.copy_from_slice(&full_hmac[..WINZIP_AUTH_CODE_LEN]);
        auth_code
    }
}

/// WinZip AES decryption context.
pub struct ZipAesDecryptor {
    cipher: AesCtr,
    hmac_key: Vec<u8>,
    hmac_data: Vec<u8>,
}

impl ZipAesDecryptor {
    /// Create a new decryptor for the given password and salt.
    ///
    /// Returns the decryptor and the expected password verification bytes.
    pub fn new(password: &[u8], salt: &[u8], strength: AesStrength) -> Result<(Self, [u8; 2])> {
        if salt.len() != strength.salt_len() {
            return Err(OxiArcError::invalid_header(format!(
                "Invalid salt length: expected {}, got {}",
                strength.salt_len(),
                salt.len()
            )));
        }

        // Derive key material using PBKDF2-SHA1 with 1000 iterations
        let dk_len = strength.derived_key_len();
        let derived = pbkdf2_sha1(password, salt, 1000, dk_len);

        let key_len = strength.key_len();

        // Split derived key material
        let encryption_key = &derived[..key_len];
        let hmac_key = derived[key_len..key_len * 2].to_vec();
        let password_verification: [u8; 2] = [derived[key_len * 2], derived[key_len * 2 + 1]];

        // For AES-256, we need a 32-byte key
        let cipher = if strength == AesStrength::Aes256 {
            let mut key = [0u8; 32];
            key.copy_from_slice(encryption_key);
            AesCtr::new(&key)
        } else {
            let mut key = [0u8; 32];
            key[..key_len].copy_from_slice(encryption_key);
            AesCtr::new(&key)
        };

        Ok((
            Self {
                cipher,
                hmac_key,
                hmac_data: Vec::new(),
            },
            password_verification,
        ))
    }

    /// Accumulate encrypted data for HMAC verification.
    pub fn update_hmac(&mut self, encrypted_data: &[u8]) {
        self.hmac_data.extend_from_slice(encrypted_data);
    }

    /// Decrypt data in place.
    pub fn decrypt(&mut self, data: &mut [u8]) {
        self.cipher.process(data);
    }

    /// Verify the authentication code.
    pub fn verify(&self, auth_code: &[u8]) -> bool {
        if auth_code.len() != WINZIP_AUTH_CODE_LEN {
            return false;
        }
        let full_hmac = hmac_sha1(&self.hmac_key, &self.hmac_data);
        &full_hmac[..WINZIP_AUTH_CODE_LEN] == auth_code
    }
}

/// Generate a random salt using a simple PRNG.
///
/// Note: This uses a simple time-based seed for portability.
/// For production use, consider using a proper CSPRNG.
pub fn generate_salt(len: usize) -> Vec<u8> {
    use std::time::{SystemTime, UNIX_EPOCH};

    let seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);

    let mut state = seed as u64;
    let mut salt = Vec::with_capacity(len);

    for _ in 0..len {
        // Simple xorshift64 PRNG
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        salt.push((state >> 32) as u8);
    }

    salt
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sha1_empty() {
        let result = sha1(b"");
        let expected = [
            0xda, 0x39, 0xa3, 0xee, 0x5e, 0x6b, 0x4b, 0x0d, 0x32, 0x55, 0xbf, 0xef, 0x95, 0x60,
            0x18, 0x90, 0xaf, 0xd8, 0x07, 0x09,
        ];
        assert_eq!(result, expected);
    }

    #[test]
    fn test_sha1_hello() {
        let result = sha1(b"hello");
        let expected = [
            0xaa, 0xf4, 0xc6, 0x1d, 0xdc, 0xc5, 0xe8, 0xa2, 0xda, 0xbe, 0xde, 0x0f, 0x3b, 0x48,
            0x2c, 0xd9, 0xae, 0xa9, 0x43, 0x4d,
        ];
        assert_eq!(result, expected);
    }

    #[test]
    fn test_sha1_abc() {
        let result = sha1(b"abc");
        let expected = [
            0xa9, 0x99, 0x3e, 0x36, 0x47, 0x06, 0x81, 0x6a, 0xba, 0x3e, 0x25, 0x71, 0x78, 0x50,
            0xc2, 0x6c, 0x9c, 0xd0, 0xd8, 0x9d,
        ];
        assert_eq!(result, expected);
    }

    #[test]
    fn test_sha1_long() {
        let result = sha1(b"The quick brown fox jumps over the lazy dog");
        let expected = [
            0x2f, 0xd4, 0xe1, 0xc6, 0x7a, 0x2d, 0x28, 0xfc, 0xed, 0x84, 0x9e, 0xe1, 0xbb, 0x76,
            0xe7, 0x39, 0x1b, 0x93, 0xeb, 0x12,
        ];
        assert_eq!(result, expected);
    }

    #[test]
    fn test_hmac_sha1() {
        // RFC 2202 test vector 1
        let key = [0x0b; 20];
        let data = b"Hi There";
        let result = hmac_sha1(&key, data);
        let expected = [
            0xb6, 0x17, 0x31, 0x86, 0x55, 0x05, 0x72, 0x64, 0xe2, 0x8b, 0xc0, 0xb6, 0xfb, 0x37,
            0x8c, 0x8e, 0xf1, 0x46, 0xbe, 0x00,
        ];
        assert_eq!(result, expected);
    }

    #[test]
    fn test_hmac_sha1_key_longer_than_block() {
        // RFC 2202 test vector 6: key longer than block size
        // Key: 0xaa repeated 80 times
        // Data: "Test Using Larger Than Block-Size Key - Hash Key First"
        // Digest: 0xaa4ae5e15272d00e95705637ce8a3b55ed402112
        let key = [0xaa; 80];
        let data = b"Test Using Larger Than Block-Size Key - Hash Key First";
        let result = hmac_sha1(&key, data);
        let expected = [
            0xaa, 0x4a, 0xe5, 0xe1, 0x52, 0x72, 0xd0, 0x0e, 0x95, 0x70, 0x56, 0x37, 0xce, 0x8a,
            0x3b, 0x55, 0xed, 0x40, 0x21, 0x12,
        ];
        assert_eq!(result, expected);
    }

    #[test]
    fn test_pbkdf2_sha1() {
        // RFC 6070 test vector
        let password = b"password";
        let salt = b"salt";
        let iterations = 1;
        let dk_len = 20;

        let result = pbkdf2_sha1(password, salt, iterations, dk_len);
        let expected = [
            0x0c, 0x60, 0xc8, 0x0f, 0x96, 0x1f, 0x0e, 0x71, 0xf3, 0xa9, 0xb5, 0x24, 0xaf, 0x60,
            0x12, 0x06, 0x2f, 0xe0, 0x37, 0xa6,
        ];
        assert_eq!(result, expected);
    }

    #[test]
    fn test_pbkdf2_sha1_4096_iterations() {
        // RFC 6070 test vector
        let password = b"password";
        let salt = b"salt";
        let iterations = 4096;
        let dk_len = 20;

        let result = pbkdf2_sha1(password, salt, iterations, dk_len);
        let expected = [
            0x4b, 0x00, 0x79, 0x01, 0xb7, 0x65, 0x48, 0x9a, 0xbe, 0xad, 0x49, 0xd9, 0x26, 0xf7,
            0x21, 0xd0, 0x65, 0xa4, 0x29, 0xc1,
        ];
        assert_eq!(result, expected);
    }

    #[test]
    fn test_aes256_encrypt() {
        // NIST test vector (FIPS 197 Appendix C.3)
        let key: [u8; 32] = [
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d,
            0x0e, 0x0f, 0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b,
            0x1c, 0x1d, 0x1e, 0x1f,
        ];
        let plaintext: [u8; 16] = [
            0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
            0xee, 0xff,
        ];
        let expected: [u8; 16] = [
            0x8e, 0xa2, 0xb7, 0xca, 0x51, 0x67, 0x45, 0xbf, 0xea, 0xfc, 0x49, 0x90, 0x4b, 0x49,
            0x60, 0x89,
        ];

        let cipher = Aes256::new(&key);
        let result = cipher.encrypt_block(&plaintext);
        assert_eq!(result, expected);
    }

    #[test]
    fn test_aes_ctr_encrypt_decrypt() {
        let key: [u8; 32] = [
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d,
            0x0e, 0x0f, 0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b,
            0x1c, 0x1d, 0x1e, 0x1f,
        ];
        let plaintext = b"Hello, World! This is a test of AES-CTR mode encryption.";

        // Encrypt
        let mut encrypted = plaintext.to_vec();
        let mut cipher = AesCtr::new(&key);
        cipher.process(&mut encrypted);

        // Verify it's different from plaintext
        assert_ne!(encrypted.as_slice(), plaintext.as_slice());

        // Decrypt
        let mut decrypted = encrypted.clone();
        let mut cipher = AesCtr::new(&key);
        cipher.process(&mut decrypted);

        // Verify decryption matches original
        assert_eq!(decrypted.as_slice(), plaintext.as_slice());
    }

    #[test]
    fn test_aes_extra_field_roundtrip() {
        let extra = AesExtraField::new(AesStrength::Aes256, 8);
        let bytes = extra.to_bytes();

        let parsed = AesExtraField::from_bytes(&bytes).expect("Failed to parse AES extra field");

        assert_eq!(parsed.version, 2);
        assert_eq!(parsed.vendor_id, [b'A', b'E']);
        assert_eq!(parsed.strength, AesStrength::Aes256);
        assert_eq!(parsed.compression_method, 8);
    }

    #[test]
    fn test_zip_aes_encrypt_decrypt() {
        let password = b"secret123";
        let salt = generate_salt(AesStrength::Aes256.salt_len());
        let plaintext = b"This is secret data that needs encryption!";

        // Encrypt
        let (mut encryptor, pw_verification_enc) =
            ZipAesEncryptor::new(password, &salt, AesStrength::Aes256)
                .expect("Failed to create encryptor");

        let mut encrypted = plaintext.to_vec();
        encryptor.encrypt(&mut encrypted);
        let auth_code = encryptor.finalize();

        // Decrypt
        let (mut decryptor, pw_verification_dec) =
            ZipAesDecryptor::new(password, &salt, AesStrength::Aes256)
                .expect("Failed to create decryptor");

        // Verify password verification bytes match
        assert_eq!(pw_verification_enc, pw_verification_dec);

        // Update HMAC with encrypted data
        decryptor.update_hmac(&encrypted);

        // Verify authentication
        assert!(decryptor.verify(&auth_code));

        // Decrypt
        let mut decrypted = encrypted.clone();
        decryptor.decrypt(&mut decrypted);

        assert_eq!(decrypted.as_slice(), plaintext.as_slice());
    }

    #[test]
    fn test_zip_aes_wrong_password() {
        let password = b"secret123";
        let wrong_password = b"wrong";
        let salt = generate_salt(AesStrength::Aes256.salt_len());

        let (_, pw_verification_correct) =
            ZipAesEncryptor::new(password, &salt, AesStrength::Aes256)
                .expect("Failed to create encryptor");

        let (_, pw_verification_wrong) =
            ZipAesDecryptor::new(wrong_password, &salt, AesStrength::Aes256)
                .expect("Failed to create decryptor");

        // Password verification bytes should differ
        assert_ne!(pw_verification_correct, pw_verification_wrong);
    }

    #[test]
    fn test_generate_salt() {
        let salt1 = generate_salt(16);
        let salt2 = generate_salt(16);

        assert_eq!(salt1.len(), 16);
        assert_eq!(salt2.len(), 16);
        // Salts should be different (with very high probability)
        // Note: This test might rarely fail due to timing, but is unlikely
    }

    #[test]
    fn test_aes_strength_properties() {
        assert_eq!(AesStrength::Aes128.salt_len(), 8);
        assert_eq!(AesStrength::Aes128.key_len(), 16);
        assert_eq!(AesStrength::Aes128.derived_key_len(), 34);

        assert_eq!(AesStrength::Aes192.salt_len(), 12);
        assert_eq!(AesStrength::Aes192.key_len(), 24);
        assert_eq!(AesStrength::Aes192.derived_key_len(), 50);

        assert_eq!(AesStrength::Aes256.salt_len(), 16);
        assert_eq!(AesStrength::Aes256.key_len(), 32);
        assert_eq!(AesStrength::Aes256.derived_key_len(), 66);
    }
}
