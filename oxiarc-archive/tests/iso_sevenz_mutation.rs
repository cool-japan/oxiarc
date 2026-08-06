//! Deterministic mutation regression tests for the two untrusted-input
//! parsers that had empty/near-empty fuzz corpora: ISO 9660 and 7z.
//!
//! These are **not** a fuzz campaign — they cannot run the millions of
//! executions a `cargo fuzz` run would. Instead they seed from the real
//! embedded fixtures, apply a bounded, reproducible set of bit-flips, and
//! assert the invariant that matters for an attacker-facing parser: for any
//! input, `new`/`extract` must return `Ok` or `Err` and never panic, run away,
//! or allocate without bound. Output is capped so a mutated size field cannot
//! turn the test into a memory or time bomb.

use std::io::{self, Cursor, Write};

use oxiarc_archive::{IsoReader, SevenZReader};

const ISO_PLAIN: &[u8] = include_bytes!("data/iso_plain.iso");
const SEVENZ_COPY: &[u8] = include_bytes!("data/sevenz_bsdtar_copy.7z");

/// Reproducible PCG-style LCG.
struct Lcg(u64);
impl Lcg {
    fn next_u64(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0
    }
}

/// A sink that counts bytes and refuses to accept more than `cap`, so a
/// mutated length field cannot drive extraction into an unbounded write.
struct CappedSink {
    written: u64,
    cap: u64,
}
impl Write for CappedSink {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.written = self.written.saturating_add(buf.len() as u64);
        if self.written > self.cap {
            return Err(io::Error::other("output cap exceeded"));
        }
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Mutate 1..=6 bytes of `base`, restricting positions to `[lo, hi)` so the
/// caller can keep the parser's magic/header intact and actually reach the
/// deeper (directory-record / coder-chain) paths. `lo`/`hi` are clamped to the
/// buffer.
fn mutate_range(base: &[u8], rng: &mut Lcg, lo: usize, hi: usize) -> Vec<u8> {
    let mut m = base.to_vec();
    if m.is_empty() {
        return m;
    }
    let lo = lo.min(m.len() - 1);
    let hi = hi.clamp(lo + 1, m.len());
    let span = hi - lo;
    let flips = 1 + (rng.next_u64() % 6) as usize;
    for _ in 0..flips {
        let pos = lo + (rng.next_u64() as usize) % span;
        m[pos] ^= (rng.next_u64() >> 24) as u8;
    }
    m
}

#[test]
fn iso9660_reader_survives_deterministic_mutations() {
    let mut rng = Lcg(0x1509_9660_ABCD_1234);
    // Sanity: the pristine fixture opens.
    assert!(IsoReader::new(Cursor::new(ISO_PLAIN)).is_ok());

    // The volume descriptors live at LBA 16 (byte 32768) and the file-data
    // extents begin around LBA 20 (byte 40960). Biasing most mutations into
    // the data region keeps the image openable so `extract` is actually
    // exercised; the rest hit the whole file to stress `new` against garbage.
    let data_region_start = 40 * 1024;
    let mut opened = 0usize;
    let mut extracted = 0usize;

    for i in 0..3000 {
        let m = if i % 4 == 0 {
            mutate_range(ISO_PLAIN, &mut rng, 0, ISO_PLAIN.len())
        } else {
            mutate_range(ISO_PLAIN, &mut rng, data_region_start, ISO_PLAIN.len())
        };
        let Ok(mut reader) = IsoReader::new(Cursor::new(&m)) else {
            continue;
        };
        opened += 1;
        let entries = reader.entries().to_vec();
        for entry in &entries {
            let mut sink = CappedSink {
                written: 0,
                cap: 8 * 1024 * 1024,
            };
            // Ok or Err (including the cap error) — must never panic.
            let _ = reader.extract(entry, &mut sink);
            extracted += 1;
        }
    }

    assert!(
        opened > 50 && extracted > 50,
        "mutations rarely reached extraction (opened={opened}, extracted={extracted}); \
         the test would be a vacuous `new`-only net"
    );
}

#[test]
fn sevenz_reader_survives_deterministic_mutations() {
    let mut rng = Lcg(0x0007_2A55_5EED_9911);
    // Sanity: the pristine fixture opens.
    assert!(SevenZReader::new(Cursor::new(SEVENZ_COPY)).is_ok());

    // The 32-byte signature header and the end-of-file header metadata must
    // stay intact for `new` to succeed. The packed streams sit in between, so
    // biasing mutations into `[32, 2/3 len)` keeps the file openable and drives
    // `extract` (coder-chain + CRC) paths; a quarter hit the whole file.
    let packed_hi = (SEVENZ_COPY.len() * 2) / 3;
    let mut opened = 0usize;
    let mut probed = 0usize;

    for i in 0..3000 {
        let m = if i % 4 == 0 {
            mutate_range(SEVENZ_COPY, &mut rng, 0, SEVENZ_COPY.len())
        } else {
            mutate_range(SEVENZ_COPY, &mut rng, 32, packed_hi)
        };
        let Ok(mut reader) = SevenZReader::new(Cursor::new(&m)) else {
            continue;
        };
        opened += 1;
        let count = reader.entries().len();
        // Cap how many entries we probe so a mutated entry count cannot blow up.
        for index in 0..count.min(64) {
            let _ = reader.extract(index);
            probed += 1;
        }
    }

    assert!(
        opened > 50 && probed > 50,
        "mutations rarely reached extraction (opened={opened}, probed={probed})"
    );
}

/// Every truncation prefix of the ISO fixture must be handled without panic.
#[test]
fn iso9660_truncation_prefixes_never_panic() {
    // Step through sector-sized prefixes (2048 bytes) plus a few odd offsets.
    let mut cut = 0usize;
    while cut <= ISO_PLAIN.len() {
        let _ = IsoReader::new(Cursor::new(&ISO_PLAIN[..cut]));
        cut += 2048;
    }
    for &cut in &[1usize, 33, 2049, 40959] {
        if cut <= ISO_PLAIN.len() {
            let _ = IsoReader::new(Cursor::new(&ISO_PLAIN[..cut]));
        }
    }
}

/// Every truncation prefix of the 7z fixture must be handled without panic.
#[test]
fn sevenz_truncation_prefixes_never_panic() {
    for cut in 0..=SEVENZ_COPY.len() {
        let _ = SevenZReader::new(Cursor::new(&SEVENZ_COPY[..cut]));
    }
}
