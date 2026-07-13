//! Development utility: differential check against a reference-brotli corpus.
//!
//! Usage: `cargo run -p oxiarc-brotli --release --example refcheck -- <in_dir> <br_dir>`
//!
//! `<br_dir>` contains files named `<input>.q<Q>.w<W>.br`, each produced by
//! the reference `brotli` CLI from `<in_dir>/<input>`. Every stream must
//! decode byte-identically; failures and (worse) silent mismatches are
//! reported individually.

use std::fs;
use std::path::PathBuf;

fn main() {
    let mut args = std::env::args().skip(1);
    let (Some(in_dir), Some(br_dir)) = (args.next(), args.next()) else {
        eprintln!("usage: refcheck <in_dir> <br_dir>");
        std::process::exit(2);
    };
    let in_dir = PathBuf::from(in_dir);
    let br_dir = PathBuf::from(br_dir);

    let mut ok = 0usize;
    let mut errors = 0usize;
    let mut mismatches = 0usize;
    let mut entries: Vec<_> = fs::read_dir(&br_dir)
        .expect("read br dir")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "br"))
        .collect();
    entries.sort();

    for path in &entries {
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        // <input>.q<Q>.w<W>.br -> input name is everything before ".q".
        let Some(base_end) = name.rfind(".q").map(|i| &name[..i]) else {
            continue;
        };
        let original = fs::read(in_dir.join(base_end)).expect("read original");
        let compressed = fs::read(path).expect("read compressed");
        match oxiarc_brotli::decompress(&compressed) {
            Ok(decoded) if decoded == original => ok += 1,
            Ok(decoded) => {
                mismatches += 1;
                println!(
                    "SILENT MISMATCH {name}: got {} bytes, want {} bytes",
                    decoded.len(),
                    original.len()
                );
            }
            Err(e) => {
                errors += 1;
                println!("ERROR {name}: {e}");
            }
        }
    }

    println!(
        "\ntotal {}: ok {ok}, errors {errors}, SILENT MISMATCHES {mismatches}",
        ok + errors + mismatches
    );
    if errors + mismatches > 0 {
        std::process::exit(1);
    }
}
