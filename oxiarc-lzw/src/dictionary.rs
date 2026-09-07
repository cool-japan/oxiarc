//! LZW dictionary (code table) management.
//!
//! The dictionary is stored in the classical *prefix/suffix* form used by
//! libtiff, `compress(1)` and every other production LZW implementation:
//! every code `c` above the single-byte range denotes the string
//!
//! ```text
//! string(c) = string(prefix[c]) ++ suffix[c]
//! ```
//!
//! so an entry costs a fixed five bytes (`prefix`, `suffix`, `first`,
//! `length`) instead of an owned `Vec<u8>`, and expanding a code writes its
//! bytes straight into the caller's output buffer *backwards* from the end
//! of the string. The previous representation (`Vec<Vec<u8>>` plus a
//! `HashMap<Vec<u8>, u16>`) allocated once per emitted code on decode and
//! once per input byte on encode; both hot loops are now allocation-free.
//!
//! The same table backs the encoder and the decoder. The encoder
//! additionally keeps an [`LzwCodeIndex`] — an open-addressed
//! `(prefix, byte) -> code` map — which replaces the old string-keyed
//! `HashMap` lookup.

use crate::config::LzwConfig;
use crate::error::{LzwError, Result};

/// LZW dictionary (code table) shared by the encoder and the decoder.
///
/// Codes below [`LzwConfig::clear_code`] are the single-byte roots; the two
/// codes at `clear_code` / `eoi_code` are reserved placeholders (length 0)
/// and are never emitted; codes from [`LzwConfig::first_code`] upwards are
/// the learned entries.
#[derive(Debug)]
pub struct LzwDictionary {
    /// Parent code of each entry (unused for roots and reserved codes).
    prefix: Vec<u16>,
    /// Last byte of each entry's string.
    suffix: Vec<u8>,
    /// First byte of each entry's string (propagated from the prefix).
    first: Vec<u8>,
    /// Length in bytes of each entry's string (0 for reserved codes).
    length: Vec<u16>,
    /// Offset in the current output buffer at which each learned entry was
    /// last written in full, or [`Self::NO_OUTPUT_OFFSET`].
    ///
    /// Expanding a code by walking its prefix chain costs one dependent
    /// load per byte; when the same string is already present earlier in the
    /// output (which is the common case for the long runs LZW builds out of
    /// flat image regions) copying it back is a single `memcpy` instead.
    /// Only learned codes are tracked: roots are one byte long, and their
    /// slots would otherwise survive a table reset with a stale offset.
    output_offset: Vec<u32>,
    /// Configuration.
    config: LzwConfig,
    /// Next available code.
    ///
    /// Held as a `u32` rather than a `u16` because a 16-bit configuration's
    /// table is exhausted at `next_code == 65536`, which a `u16` cannot
    /// represent: `is_full()` would never fire and `next_code += 1` would
    /// overflow.
    next_code: u32,
    /// Current code bit width.
    current_bits: u8,
}

impl LzwDictionary {
    /// Sentinel meaning "this code's string is not in the output buffer".
    pub const NO_OUTPUT_OFFSET: u32 = u32::MAX;

    /// Create a new LZW dictionary with the given configuration.
    ///
    /// # Errors
    ///
    /// Returns [`LzwError::InvalidBitWidth`] when `config` fails
    /// [`LzwConfig::validate`].
    pub fn new(config: LzwConfig) -> Result<Self> {
        config.validate()?;

        let capacity = config.max_code() as usize + 1;
        let mut dict = Self {
            prefix: vec![0; capacity],
            suffix: vec![0; capacity],
            first: vec![0; capacity],
            length: vec![0; capacity],
            output_offset: vec![Self::NO_OUTPUT_OFFSET; capacity],
            config,
            next_code: 0,
            current_bits: config.min_bits,
        };

        // The root entries never change, so they are written once here and
        // left alone by `reset()` — that is what makes a table reset O(1).
        let clear_code = config.clear_code();
        for code in 0..clear_code {
            let index = code as usize;
            dict.prefix[index] = 0;
            dict.suffix[index] = code as u8;
            dict.first[index] = code as u8;
            dict.length[index] = 1;
        }

        dict.reset();
        Ok(dict)
    }

    /// Reset the dictionary to its initial state.
    ///
    /// Only the allocation cursor and the code width are rolled back: the
    /// root entries are immutable and stale entries above `next_code` are
    /// unreachable, so no memory has to be touched.
    pub fn reset(&mut self) {
        self.current_bits = self.config.min_bits;
        self.next_code = u32::from(self.config.first_code());
    }

    /// Number of code slots in the table (`max_code + 1`).
    pub fn capacity(&self) -> usize {
        self.length.len()
    }

    /// Add an entry for the encoder: `string(prefix) ++ byte`.
    ///
    /// Returns the assigned code.
    ///
    /// # Errors
    ///
    /// Returns [`LzwError::TableFull`] when the table has no free slot.
    pub fn add_entry_encode(&mut self, prefix: u16, byte: u8) -> Result<u16> {
        let code = self.store(prefix, byte)?;
        // Encoder rule: the width grows one entry later than the decoder's.
        self.update_bit_width();
        Ok(code)
    }

    /// Add an entry for the decoder: `string(prefix) ++ byte`.
    ///
    /// Uses the decoder-specific bit-width update, which accounts for the
    /// one-entry lag between encoder and decoder.
    ///
    /// # Errors
    ///
    /// Returns [`LzwError::TableFull`] when the table has no free slot.
    pub fn add_entry_decode(&mut self, prefix: u16, byte: u8) -> Result<u16> {
        let code = self.store(prefix, byte)?;
        self.update_bit_width_decode();
        Ok(code)
    }

    /// Store a new `(prefix, byte)` entry without touching the code width.
    fn store(&mut self, prefix: u16, byte: u8) -> Result<u16> {
        if self.next_code > u32::from(self.config.max_code()) {
            return Err(LzwError::TableFull {
                max_codes: self.config.max_code(),
            });
        }
        let code = self.next_code as u16;
        let index = code as usize;
        let parent = prefix as usize;
        if parent >= self.length.len() || index >= self.length.len() {
            return Err(LzwError::InvalidCode(prefix));
        }
        self.prefix[index] = prefix;
        self.suffix[index] = byte;
        // A freshly assigned code has not been written to the output yet.
        self.output_offset[index] = Self::NO_OUTPUT_OFFSET;
        self.first[index] = self.first[parent];
        // A string can never be longer than the number of table slots, so
        // the saturating add is unreachable in practice and only exists so
        // that a hypothetical overflow degrades into a short string instead
        // of a panic.
        self.length[index] = self.length[parent].saturating_add(1);
        self.next_code += 1;
        Ok(code)
    }

    /// Account for the phantom table entry the decoder creates while
    /// processing the encoder's final data code.
    ///
    /// The decoder adds one dictionary entry for every code it reads after
    /// the first, including the *last* data code — but the encoder has no
    /// following input byte at that point, so it never performs a matching
    /// `add_entry_encode`. Without compensation the decoder can cross a
    /// bit-width threshold just before reading EOI while the encoder writes
    /// EOI at the old width, desynchronizing the stream at exact boundary
    /// sizes.
    ///
    /// libtiff's `LZWPostEncode` increments `free_ent` (without storing an
    /// entry) for exactly this reason; this method mirrors it. Call it after
    /// emitting the final data code and before emitting EOI.
    pub fn note_final_code(&mut self) {
        if !self.is_full() {
            self.next_code += 1;
            self.update_bit_width();
        }
    }

    /// Update bit width based on next_code.
    ///
    /// TIFF uses "early change": bit width increases when the next code
    /// equals 2^current_bits (one code earlier than standard LZW).
    fn update_bit_width(&mut self) {
        if self.current_bits < self.config.max_bits {
            let threshold: u32 = if self.config.early_change {
                // Early change: increase when next_code == 2^current_bits
                1 << self.current_bits
            } else {
                // Standard: increase when next_code == 2^current_bits + 1
                (1 << self.current_bits) + 1
            };

            if self.next_code >= threshold {
                self.current_bits += 1;
            }
        }
    }

    /// Update bit width for decoder (compensates for one-entry lag).
    ///
    /// The decoder adds dictionary entries one iteration later than the
    /// encoder, so the decoder's next_code is always one behind the
    /// encoder's next_code. To maintain bit-width synchronization, the
    /// decoder must increase its bit width one code earlier than the
    /// encoder.
    ///
    /// # Synchronization Analysis
    ///
    /// When encoder adds entry 511:
    /// - Encoder: next_code = 512, bit width increases to 10
    /// - Decoder: next_code = 511 (one behind!)
    ///
    /// Solution: Decoder threshold = encoder threshold - 1
    fn update_bit_width_decode(&mut self) {
        if self.current_bits < self.config.max_bits {
            let threshold: u32 = if self.config.early_change {
                // Decoder threshold is one less than encoder threshold
                (1 << self.current_bits) - 1
            } else {
                // Standard LZW
                1 << self.current_bits
            };

            if self.next_code >= threshold {
                self.current_bits += 1;
            }
        }
    }

    /// Length in bytes of the string denoted by `code`.
    ///
    /// Returns 0 for the reserved ClearCode/EOI slots and for codes that
    /// have not been assigned yet.
    #[inline]
    pub fn entry_len(&self, code: u16) -> u16 {
        self.length.get(code as usize).copied().unwrap_or(0)
    }

    /// First byte of the string denoted by `code`.
    #[inline]
    pub fn first_byte(&self, code: u16) -> u8 {
        self.first.get(code as usize).copied().unwrap_or(0)
    }

    /// Last byte of the string denoted by `code`.
    #[inline]
    pub fn suffix_byte(&self, code: u16) -> u8 {
        self.suffix.get(code as usize).copied().unwrap_or(0)
    }

    /// Parent code of `code` (meaningful only for learned entries, whose
    /// string is `string(parent) ++ suffix_byte(code)`).
    #[inline]
    pub fn prefix_of(&self, code: u16) -> u16 {
        self.prefix.get(code as usize).copied().unwrap_or(0)
    }

    /// Expand `code` into `dst`, writing its bytes backwards from the end.
    ///
    /// `dst.len()` must equal the number of bytes to write, which may be
    /// **fewer** than `entry_len(code)`: the trailing bytes of the string
    /// are then dropped so that `dst` receives the string's first
    /// `dst.len()` bytes. That is what a decoder does when the last code of
    /// a strip expands past the end of the caller's buffer (libtiff's
    /// `LZWDecode` does the same).
    #[inline]
    pub fn expand(&self, code: u16, dst: &mut [u8]) {
        let full = self.entry_len(code) as usize;
        let want = dst.len();
        if want == 0 || full == 0 {
            return;
        }
        let mut current = code as usize;
        // Drop the tail that does not fit by walking up to the ancestor
        // whose string is exactly the prefix we keep.
        let mut skip = full.saturating_sub(want);
        while skip > 0 {
            match self.prefix.get(current) {
                Some(&parent) => current = parent as usize,
                None => return,
            }
            skip -= 1;
        }
        let mut index = want.min(full);
        while index > 0 {
            let (byte, parent) = match (self.suffix.get(current), self.prefix.get(current)) {
                (Some(&byte), Some(&parent)) => (byte, parent as usize),
                _ => return,
            };
            index -= 1;
            dst[index] = byte;
            current = parent;
        }
    }

    /// Offset in the output buffer at which `code`'s full string was last
    /// written, if it is still valid for the decode in progress.
    #[inline]
    pub fn output_offset(&self, code: u16) -> Option<usize> {
        let offset = *self.output_offset.get(code as usize)?;
        if offset == Self::NO_OUTPUT_OFFSET {
            None
        } else {
            Some(offset as usize)
        }
    }

    /// Record that `code`'s full string now lives at `offset` in the output
    /// buffer. Only learned codes (>= `first_code`) are tracked, so a table
    /// reset can never leave a stale offset behind: every learned slot is
    /// re-armed by [`Self::store`] before it can be read again.
    #[inline]
    pub fn set_output_offset(&mut self, code: u16, offset: usize) {
        if code < self.config.first_code() {
            return;
        }
        if let (Ok(offset), Some(slot)) = (
            u32::try_from(offset),
            self.output_offset.get_mut(code as usize),
        ) {
            if offset != Self::NO_OUTPUT_OFFSET {
                *slot = offset;
            }
        }
    }

    /// Materialize the string denoted by `code` (test/diagnostic helper).
    ///
    /// # Errors
    ///
    /// Returns [`LzwError::InvalidCode`] when `code` is outside the table.
    #[cfg(test)]
    pub fn get_string(&self, code: u16) -> Result<Vec<u8>> {
        if code as usize >= self.length.len() {
            return Err(LzwError::InvalidCode(code));
        }
        let mut out = vec![0u8; self.entry_len(code) as usize];
        self.expand(code, &mut out);
        Ok(out)
    }

    /// Check if the dictionary is full.
    #[inline]
    pub fn is_full(&self) -> bool {
        self.next_code > u32::from(self.config.max_code())
    }

    /// Get the current bit width.
    #[inline]
    pub fn current_bits(&self) -> u8 {
        self.current_bits
    }

    /// Get the next code that will be assigned.
    ///
    /// Returned as a `u32`: a full 16-bit table's exhausted state is
    /// `next_code == 65536`, one past `u16::MAX`.
    #[inline]
    pub fn next_code(&self) -> u32 {
        self.next_code
    }

    /// Get the clear code.
    #[inline]
    pub fn clear_code(&self) -> u16 {
        self.config.clear_code()
    }

    /// Get the end-of-information code.
    #[inline]
    pub fn eoi_code(&self) -> u16 {
        self.config.eoi_code()
    }

    /// Get the configuration.
    #[inline]
    pub fn config(&self) -> &LzwConfig {
        &self.config
    }
}

/// Open-addressed `(prefix code, byte) -> code` map used by the encoder.
///
/// LZW only ever asks "is `string(prefix) ++ byte` already in the table?",
/// and because every table string is unique that question is answered by the
/// `(prefix, byte)` pair alone — no byte string needs to be built, hashed or
/// stored. Slots hold `key + 1` so that zero means "empty", which makes
/// [`LzwCodeIndex::clear`] a single memset.
#[derive(Debug)]
pub struct LzwCodeIndex {
    /// `key + 1` per slot, 0 when empty.
    keys: Vec<u32>,
    /// Code stored in the matching slot.
    codes: Vec<u16>,
    /// `keys.len() - 1`; `keys.len()` is always a power of two.
    mask: usize,
}

impl LzwCodeIndex {
    /// Create an index able to hold `capacity` entries at a load factor of
    /// at most 0.5 (rounded up to a power of two, minimum 1024 slots).
    ///
    /// The probe loops in [`Self::find`] and [`Self::insert`] terminate
    /// because at least one slot is always free. The largest table this
    /// crate builds is a 16-bit one, whose `capacity` is 65 536 and whose
    /// live entry count therefore never exceeds 65 536 - 258 = 65 278; that
    /// gets 131 072 slots, so the load factor stays below 0.5. (At 12 bits
    /// it is 3 838 entries in 8 192 slots.) The `min(1 << 20)` clamp only
    /// binds for capacities above 524 288, which no valid `LzwConfig` can
    /// reach.
    pub fn with_capacity(capacity: usize) -> Self {
        let slots = capacity
            .saturating_mul(2)
            .max(1024)
            .next_power_of_two()
            .min(1 << 20);
        Self {
            keys: vec![0; slots],
            codes: vec![0; slots],
            mask: slots - 1,
        }
    }

    /// Remove every entry.
    pub fn clear(&mut self) {
        self.keys.fill(0);
    }

    /// Combine a prefix code and a suffix byte into a lookup key.
    #[inline]
    fn key(prefix: u16, byte: u8) -> u32 {
        ((prefix as u32) << 8) | byte as u32
    }

    /// Initial slot for a key (Fibonacci hashing).
    #[inline]
    fn slot(&self, key: u32) -> usize {
        (key.wrapping_mul(0x9E37_79B1) as usize >> 8) & self.mask
    }

    /// Look up the code for `string(prefix) ++ byte`.
    #[inline]
    pub fn find(&self, prefix: u16, byte: u8) -> Option<u16> {
        let key = Self::key(prefix, byte) + 1;
        let mut slot = self.slot(key);
        loop {
            let stored = self.keys[slot];
            if stored == 0 {
                return None;
            }
            if stored == key {
                return Some(self.codes[slot]);
            }
            slot = (slot + 1) & self.mask;
        }
    }

    /// Record that `string(prefix) ++ byte` has code `code`.
    ///
    /// The caller guarantees the index never holds more entries than the
    /// capacity it was built with, so the probe always terminates.
    #[inline]
    pub fn insert(&mut self, prefix: u16, byte: u8, code: u16) {
        let key = Self::key(prefix, byte) + 1;
        let mut slot = self.slot(key);
        loop {
            let stored = self.keys[slot];
            if stored == 0 || stored == key {
                self.keys[slot] = key;
                self.codes[slot] = code;
                return;
            }
            slot = (slot + 1) & self.mask;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dictionary_init() {
        let dict = LzwDictionary::new(LzwConfig::TIFF).expect("create lzw dictionary");

        // Check initial single-byte codes
        for i in 0..256u16 {
            let string = dict.get_string(i).expect("get string for initial code");
            assert_eq!(string, vec![i as u8]);
        }

        // Check special codes
        assert_eq!(dict.clear_code(), 256);
        assert_eq!(dict.eoi_code(), 257);
        assert_eq!(dict.next_code(), 258);
        assert_eq!(dict.current_bits(), 9);
        assert_eq!(dict.entry_len(256), 0);
        assert_eq!(dict.entry_len(257), 0);
    }

    #[test]
    fn test_add_entry() {
        let mut dict =
            LzwDictionary::new(LzwConfig::TIFF).expect("create lzw dictionary for add string");

        let code = dict
            .add_entry_encode(u16::from(b'A'), b'B')
            .expect("add string AB to dictionary");
        assert_eq!(code, 258);

        let retrieved = dict.get_string(code).expect("get string for code 258");
        assert_eq!(retrieved, b"AB");
        assert_eq!(dict.first_byte(code), b'A');
        assert_eq!(dict.entry_len(code), 2);
    }

    #[test]
    fn test_expand_partial_writes_prefix() {
        let mut dict = LzwDictionary::new(LzwConfig::TIFF).expect("create lzw dictionary");
        let ab = dict
            .add_entry_encode(u16::from(b'A'), b'B')
            .expect("add AB");
        let abc = dict.add_entry_encode(ab, b'C').expect("add ABC");
        assert_eq!(dict.entry_len(abc), 3);

        let mut full = [0u8; 3];
        dict.expand(abc, &mut full);
        assert_eq!(&full, b"ABC");

        // Fewer bytes than the string holds: keep the leading prefix.
        let mut partial = [0u8; 2];
        dict.expand(abc, &mut partial);
        assert_eq!(&partial, b"AB");

        let mut one = [0u8; 1];
        dict.expand(abc, &mut one);
        assert_eq!(&one, b"A");
    }

    #[test]
    fn test_bit_width_increase() {
        let mut dict =
            LzwDictionary::new(LzwConfig::TIFF).expect("create lzw dictionary for bit width test");

        // Initially 9 bits
        assert_eq!(dict.current_bits(), 9);

        // Add entries until bit width increases. With early change, the
        // width increases when next_code == 512 (2^9); we start at 258, so
        // 254 entries are needed (258 + 254 = 512).
        for i in 0..254u16 {
            dict.add_entry_encode(i % 256, (i + 1) as u8)
                .expect("add string for bit width increase test");
        }

        assert_eq!(dict.next_code(), 512);
        assert_eq!(dict.current_bits(), 10);
    }

    #[test]
    fn test_table_full_is_reported() {
        let config = LzwConfig::new(9, 9).expect("9/9 config");
        let mut dict = LzwDictionary::new(config).expect("create 9-bit dictionary");
        // first_code 258 .. max_code 511 inclusive = 254 free slots.
        for i in 0..254u16 {
            dict.add_entry_encode(i % 256, 0).expect("fill table");
        }
        assert!(dict.is_full());
        assert!(matches!(
            dict.add_entry_encode(0, 0),
            Err(LzwError::TableFull { max_codes: 511 })
        ));
    }

    #[test]
    fn test_code_index_find_insert_clear() {
        let mut index = LzwCodeIndex::with_capacity(4096);
        assert_eq!(index.find(65, b'B'), None);
        index.insert(65, b'B', 258);
        assert_eq!(index.find(65, b'B'), Some(258));
        assert_eq!(index.find(65, b'C'), None);
        assert_eq!(index.find(66, b'B'), None);

        // Fill it to the documented capacity to exercise probing.
        for code in 0..4000u16 {
            index.insert(code, (code % 251) as u8, code);
        }
        for code in 0..4000u16 {
            assert_eq!(index.find(code, (code % 251) as u8), Some(code));
        }

        index.clear();
        assert_eq!(index.find(65, b'B'), None);
        for code in 0..4000u16 {
            assert_eq!(index.find(code, (code % 251) as u8), None);
        }
    }
}
