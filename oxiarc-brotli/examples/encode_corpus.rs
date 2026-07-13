//! Development utility: compress a corpus directory with oxiarc-brotli so
//! the outputs can be fed to the reference `brotli -d` for the
//! encode-direction differential check.
//!
//! Usage: `cargo run -p oxiarc-brotli --release --example encode_corpus -- <in_dir> <out_dir>`

use std::fs;
use std::path::PathBuf;

fn main() {
    let mut args = std::env::args().skip(1);
    let (Some(in_dir), Some(out_dir)) = (args.next(), args.next()) else {
        eprintln!("usage: encode_corpus <in_dir> <out_dir>");
        std::process::exit(2);
    };
    let in_dir = PathBuf::from(in_dir);
    let out_dir = PathBuf::from(out_dir);
    fs::create_dir_all(&out_dir).expect("create out dir");

    let mut entries: Vec<_> = fs::read_dir(&in_dir)
        .expect("read in dir")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .collect();
    entries.sort();

    let mut count = 0usize;
    for path in &entries {
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        let data = fs::read(path).expect("read input");
        for quality in [0u32, 1, 2, 5, 6, 9, 11] {
            for lgwin in [10u32, 16, 22, 24] {
                let params = oxiarc_brotli::BrotliParams {
                    quality,
                    lgwin,
                    lgblock: 0,
                };
                let compressed =
                    oxiarc_brotli::compress_with_params(&data, &params).expect("compress");
                let out = out_dir.join(format!("{name}.q{quality}.w{lgwin}.oxi.br"));
                fs::write(out, compressed).expect("write output");
                count += 1;
            }
        }
    }
    println!("wrote {count} oxiarc-compressed streams");
}
