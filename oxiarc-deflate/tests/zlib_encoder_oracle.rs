//! Encoder differential tests against CPython's `zlib`.
//!
//! The encoder is a faithful port of zlib's `deflate.c`/`trees.c`, so the gate
//! here is much stronger than a size band: for every corpus and every level
//! 1..=9 the produced stream must be **byte-identical** to
//! `zlib.compress(data, level)`. A byte-identity failure localises instantly
//! (which level, which corpus) where a 3 % size band would not.
//!
//! The contractual gate — `oxiarc <= python * 1.03` — is asserted separately so
//! it keeps meaning if a future zlib release changes a tie-break.
//!
//! Gated behind the `zlib-oracle` feature; every test self-skips when `python3`
//! is unavailable, so enabling the feature is always safe.

#![cfg(feature = "zlib-oracle")]

#[path = "common/blocks.rs"]
mod blocks;

#[path = "common/corpus.rs"]
mod corpus;

use oxiarc_deflate::{Deflater, Strategy, deflate, inflate, zlib_compress};

/// Every zlib strategy must reproduce CPython's bytes.
///
/// `Deflater::with_strategy` is new public surface, and the strategies change
/// *both* the match search (`Z_HUFFMAN_ONLY` disables it, `Z_RLE` limits it to
/// distance 1, `Z_FILTERED` discards short matches) and the block-type
/// decision (`Z_FIXED`). The corpora are chosen so those paths actually
/// diverge from each other: `anchored-random` and `nine-bit-alphabet` are the
/// shapes where `dynamic < stored + 4 <= static`, which is the *only* place
/// `Z_FIXED`'s block-type rule is observable — zlib folds `Z_FIXED` into the
/// `opt_lenb` narrowing, so the dynamic cost drops out and both corpora come
/// back as stored blocks; applying `Z_FIXED` only after the stored test would
/// emit a fixed block there instead.
///
/// That makes this gate version-sensitive: it pins zlib >= 1.2.12 behaviour,
/// the release that added `|| s->strategy == Z_FIXED` to the narrowing. Linked
/// against zlib <= 1.2.11 (macOS's system zlib) the reference emits fixed
/// blocks for these two corpora and the byte comparison fails.
#[test]
fn every_strategy_is_byte_identical_to_python() {
    if !corpus::python3_available() {
        skip("python3 with zlib not available");
        return;
    }
    // zlib: Z_FILTERED = 1, Z_HUFFMAN_ONLY = 2, Z_RLE = 3, Z_FIXED = 4.
    let mapping = [
        (Strategy::Filtered, 1u8),
        (Strategy::HuffmanOnly, 2),
        (Strategy::Rle, 3),
        (Strategy::Fixed, 4),
    ];
    let mut samples = corpus::strategy_samples();
    samples.extend(corpus::base_samples());
    let mut mismatches: Vec<String> = Vec::new();
    for s in &samples {
        for (strategy, id) in mapping {
            for level in [1u8, 4, 6, 9] {
                let mut d = Deflater::new(level).with_strategy(strategy);
                let ours = d.compress_to_vec(&s.data).expect("compress_to_vec");
                assert_eq!(
                    inflate(&ours).expect("inflate"),
                    s.data,
                    "{} {strategy:?} level {level}: round trip",
                    s.name
                );
                let Some(theirs) = corpus::python_raw_deflate_strategy(&s.data, level, id) else {
                    skip("python3 reference failed");
                    return;
                };
                if ours != theirs {
                    mismatches.push(format!(
                        "{} {strategy:?} level {level}: {} bytes vs python {}",
                        s.name,
                        ours.len(),
                        theirs.len()
                    ));
                }
            }
        }
    }
    assert!(
        mismatches.is_empty(),
        "{} strategy mismatches:\n{}",
        mismatches.len(),
        mismatches.join("\n")
    );
}

/// `Strategy::Fixed` must never emit a *dynamic* block.
///
/// This is the structural half of the gate above: it needs no reference tool,
/// so it keeps holding on a machine with no `python3`. It deliberately asserts
/// nothing about the stored-vs-fixed split — that half is version-sensitive
/// (see above) and belongs to the byte-identity gate; several of these corpora
/// come back as stored blocks.
#[test]
fn fixed_strategy_never_emits_a_dynamic_block() {
    for s in corpus::strategy_samples() {
        for level in [1u8, 6, 9] {
            let mut d = Deflater::new(level).with_strategy(Strategy::Fixed);
            let ours = d.compress_to_vec(&s.data).expect("compress_to_vec");
            let blocks = blocks::walk_blocks(&ours).expect("walkable");
            assert!(
                blocks.iter().all(|b| b.btype != blocks::BlockType::Dynamic),
                "{} level {level}: Strategy::Fixed emitted a dynamic block",
                s.name
            );
            assert_eq!(inflate(&ours).expect("inflate"), s.data, "{}", s.name);
        }
    }
}

/// The one-sided ratio gate from the track contract.
const MAX_RATIO: f64 = 1.03;

fn skip(reason: &str) {
    eprintln!("SKIP: {reason}");
}

#[test]
fn zlib_compress_is_byte_identical_to_python_at_every_level() {
    if !corpus::python3_available() {
        skip("python3 with zlib not available");
        return;
    }
    let samples = corpus::all_samples();
    assert!(
        samples.len() >= 6,
        "the Rust-generated corpora must always be present"
    );
    let mut checked = 0;
    for s in &samples {
        for level in 1u8..=9 {
            let ours = zlib_compress(&s.data, level).expect("zlib_compress");
            let Some(theirs) = corpus::python_zlib_compress(&s.data, level) else {
                skip("python3 reference failed");
                return;
            };
            assert_eq!(
                ours.len(),
                theirs.len(),
                "{} level {level}: {} bytes vs python {}",
                s.name,
                ours.len(),
                theirs.len()
            );
            assert!(
                ours == theirs,
                "{} level {level}: same length but different bytes",
                s.name
            );
            checked += 1;
        }
    }
    assert!(checked >= 54, "only {checked} comparisons ran");
}

#[test]
fn raw_deflate_is_byte_identical_to_python_at_every_level() {
    if !corpus::python3_available() {
        skip("python3 with zlib not available");
        return;
    }
    for s in corpus::base_samples() {
        for level in 1u8..=9 {
            let ours = deflate(&s.data, level).expect("deflate");
            let Some(theirs) = corpus::python_raw_deflate(&s.data, level) else {
                skip("python3 reference failed");
                return;
            };
            assert!(
                ours == theirs,
                "{} level {level}: raw DEFLATE differs ({} vs {} bytes)",
                s.name,
                ours.len(),
                theirs.len()
            );
        }
    }
}

/// Level 0 is the one level whose byte output legitimately depends on the
/// caller, not the codec: zlib's `deflate_stored` sizes each stored block to
/// the room left in `avail_out`, and CPython's `zlib.compress` hands it 32 KiB
/// output blocks. This encoder writes into an unbounded sink, so it emits the
/// maximum 65535-byte stored blocks and is therefore *smaller* than CPython's
/// output (fewer 5-byte block headers) while encoding the same bytes. The gate
/// is that it is never larger, always decodes, and really is all-stored.
#[test]
fn level_0_emits_maximal_stored_blocks_and_is_never_larger_than_python() {
    if !corpus::python3_available() {
        skip("python3 with zlib not available");
        return;
    }
    for s in corpus::base_samples() {
        let ours = zlib_compress(&s.data, 0).expect("zlib_compress");
        let Some(theirs) = corpus::python_zlib_compress(&s.data, 0) else {
            skip("python3 reference failed");
            return;
        };
        assert!(
            ours.len() <= theirs.len(),
            "{}: level 0 produced {} bytes, python {}",
            s.name,
            ours.len(),
            theirs.len()
        );
        assert_eq!(&ours[..2], &theirs[..2], "{}: zlib header differs", s.name);
        assert_eq!(
            oxiarc_deflate::zlib_decompress(&ours).expect("zlib_decompress"),
            s.data,
            "{}: level 0 round trip",
            s.name
        );
        // Every block is stored, and every one but the last is 65535 bytes.
        let raw = &ours[2..ours.len() - 4];
        let mut off = 0usize;
        let mut blocks = 0usize;
        loop {
            let header = raw[off];
            assert_eq!(header & 0x06, 0, "{}: block {blocks} is not stored", s.name);
            let last = header & 1 == 1;
            let len = u16::from_le_bytes([raw[off + 1], raw[off + 2]]) as usize;
            let nlen = u16::from_le_bytes([raw[off + 3], raw[off + 4]]);
            assert_eq!(nlen, !(len as u16), "{}: NLEN mismatch", s.name);
            if !last {
                assert_eq!(len, 65535, "{}: non-final stored block is short", s.name);
            }
            off += 5 + len;
            blocks += 1;
            if last {
                break;
            }
        }
        assert_eq!(
            off,
            raw.len(),
            "{}: trailing bytes after last block",
            s.name
        );
    }
}

#[test]
fn size_stays_within_three_percent_of_python_at_every_level() {
    if !corpus::python3_available() {
        skip("python3 with zlib not available");
        return;
    }
    let mut worst = 0.0f64;
    let mut worst_where = String::new();
    for s in corpus::all_samples() {
        for level in 1u8..=9 {
            let ours = zlib_compress(&s.data, level).expect("zlib_compress");
            let Some(theirs) = corpus::python_zlib_compress(&s.data, level) else {
                skip("python3 reference failed");
                return;
            };
            let ratio = ours.len() as f64 / theirs.len() as f64;
            if ratio > worst {
                worst = ratio;
                worst_where = format!("{} level {level}", s.name);
            }
            assert!(
                ratio <= MAX_RATIO,
                "{} level {level}: {} bytes is {:.2}% larger than python's {}",
                s.name,
                ours.len(),
                (ratio - 1.0) * 100.0,
                theirs.len()
            );
        }
    }
    eprintln!("worst ratio {worst:.4} at {worst_where}");
}

#[test]
fn tiny_and_degenerate_inputs_match_python() {
    if !corpus::python3_available() {
        skip("python3 with zlib not available");
        return;
    }
    let cases: Vec<Vec<u8>> = vec![
        Vec::new(),
        vec![0],
        vec![0, 0],
        vec![0, 0, 0],
        b"a".to_vec(),
        b"ab".to_vec(),
        b"abc".to_vec(),
        b"aaa".to_vec(),
        b"aaaa".to_vec(),
        b"abcabc".to_vec(),
        vec![255u8; 258],
        vec![7u8; 259],
        (0..=255u8).collect(),
        b"hello world hello world hello world".to_vec(),
    ];
    for data in &cases {
        for level in 0u8..=9 {
            let ours = zlib_compress(data, level).expect("zlib_compress");
            let Some(theirs) = corpus::python_zlib_compress(data, level) else {
                skip("python3 reference failed");
                return;
            };
            assert!(
                ours == theirs,
                "len {} level {level}: {:?} vs python {:?}",
                data.len(),
                ours,
                theirs
            );
        }
    }
}

#[test]
fn window_spanning_input_matches_python() {
    if !corpus::python3_available() {
        skip("python3 with zlib not available");
        return;
    }
    // 3 MiB: many window slides, many block flushes, negative `block_start`.
    let mut rng = corpus::Rng::new(0xDEFE_9C01);
    let mut data = Vec::with_capacity(3 << 20);
    let words: Vec<Vec<u8>> = (0..64)
        .map(|i| format!("token{i:03}-payload").into_bytes())
        .collect();
    while data.len() < (3 << 20) {
        data.extend_from_slice(&words[rng.below(64) as usize]);
        if rng.below(16) == 0 {
            data.extend_from_slice(&rng.bytes(37));
        }
    }
    for level in [1u8, 4, 6, 9] {
        let ours = zlib_compress(&data, level).expect("zlib_compress");
        let Some(theirs) = corpus::python_zlib_compress(&data, level) else {
            skip("python3 reference failed");
            return;
        };
        assert!(ours == theirs, "3 MiB level {level} differs");
    }
}

#[test]
fn multi_call_output_is_identical_to_one_shot_and_to_python() {
    if !corpus::python3_available() {
        skip("python3 with zlib not available");
        return;
    }
    let data = corpus::rust_sources(400 * 1024);
    let Some(reference) = corpus::python_raw_deflate(&data, 6) else {
        skip("python3 reference failed");
        return;
    };
    for chunk in [1usize, 3, 1024, 4096, 32768, 100_000] {
        let mut d = Deflater::new(6);
        let mut out = Vec::new();
        let mut i = 0;
        while i < data.len() {
            let end = (i + chunk).min(data.len());
            d.deflate(&data[i..end], &mut out, end == data.len())
                .expect("deflate");
            i = end;
        }
        assert!(
            out == reference,
            "chunk {chunk}: multi-call output differs from python ({} vs {} bytes)",
            out.len(),
            reference.len()
        );
        assert_eq!(inflate(&out).expect("inflate"), data);
    }
}

#[test]
fn dictionary_output_matches_python() {
    if !corpus::python3_available() {
        skip("python3 with zlib not available");
        return;
    }
    let dict = b"the quick brown fox jumps over the lazy dog".repeat(20);
    let data = b"the quick brown fox is quick and brown and jumps".repeat(50);
    for level in [1u8, 6, 9] {
        let mut d = Deflater::with_dictionary(level, &dict);
        let ours = d.compress_to_vec(&data).expect("deflate");
        let dict_path = corpus::temp_path("dict");
        let in_path = corpus::temp_path("din");
        let out_path = corpus::temp_path("dout");
        std::fs::write(&dict_path, &dict).expect("write dict");
        std::fs::write(&in_path, &data).expect("write data");
        let script = "import sys, zlib\n\
            d = open(sys.argv[1],'rb').read()\n\
            x = open(sys.argv[2],'rb').read()\n\
            c = zlib.compressobj(int(sys.argv[4]), zlib.DEFLATED, -15, 8, 0, zdict=d)\n\
            open(sys.argv[3],'wb').write(c.compress(x) + c.flush())\n";
        let status = std::process::Command::new("python3")
            .arg("-c")
            .arg(script)
            .arg(&dict_path)
            .arg(&in_path)
            .arg(&out_path)
            .arg(level.to_string())
            .output()
            .expect("run python3");
        if !status.status.success() {
            skip("python3 dictionary reference failed");
            return;
        }
        let theirs = std::fs::read(&out_path).expect("read reference");
        let _ = std::fs::remove_file(&dict_path);
        let _ = std::fs::remove_file(&in_path);
        let _ = std::fs::remove_file(&out_path);
        assert!(
            ours == theirs,
            "level {level}: dictionary output differs ({} vs {} bytes)",
            ours.len(),
            theirs.len()
        );
    }
}

#[test]
fn sync_flush_stream_matches_python_compressobj() {
    if !corpus::python3_available() {
        skip("python3 with zlib not available");
        return;
    }
    let chunks: Vec<Vec<u8>> = vec![
        b"first chunk of data that repeats data that repeats".to_vec(),
        b"second chunk of data that repeats data that repeats".to_vec(),
        corpus::html_like(40_000),
    ];
    let mut ours = Vec::new();
    let mut d = Deflater::new(6);
    for (i, c) in chunks.iter().enumerate() {
        if i + 1 == chunks.len() {
            d.deflate(c, &mut ours, true).expect("deflate");
        } else {
            d.deflate_sync(c, &mut ours).expect("sync");
        }
    }

    let dir = corpus::temp_path("syncdir");
    std::fs::create_dir_all(&dir).expect("mkdir");
    for (i, c) in chunks.iter().enumerate() {
        std::fs::write(dir.join(format!("c{i}")), c).expect("write chunk");
    }
    let out_path = dir.join("out");
    let script = "import sys, zlib, os\n\
        d = sys.argv[1]\n\
        n = int(sys.argv[2])\n\
        c = zlib.compressobj(6, zlib.DEFLATED, -15)\n\
        out = b''\n\
        for i in range(n):\n\
        \tdata = open(os.path.join(d, 'c%d' % i), 'rb').read()\n\
        \tif i + 1 == n:\n\
        \t\tout += c.compress(data) + c.flush()\n\
        \telse:\n\
        \t\tout += c.compress(data) + c.flush(zlib.Z_SYNC_FLUSH)\n\
        open(os.path.join(d, 'out'), 'wb').write(out)\n";
    let status = std::process::Command::new("python3")
        .arg("-c")
        .arg(script)
        .arg(&dir)
        .arg(chunks.len().to_string())
        .output()
        .expect("run python3");
    if !status.status.success() {
        skip("python3 sync-flush reference failed");
        let _ = std::fs::remove_dir_all(&dir);
        return;
    }
    let theirs = std::fs::read(&out_path).expect("read reference");
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        ours == theirs,
        "sync-flush stream differs ({} vs {} bytes)",
        ours.len(),
        theirs.len()
    );
}
