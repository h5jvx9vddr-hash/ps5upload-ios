//! Times reading every file of an exFAT image back through the reader and hashing it, the way
//! an image build's verify stage does, once per chunk size.
//!
//! cargo run --release -p ps5upload-fpkg --example exfat_read_bench -- <image.exfat> [chunk KiB...]
//!
//! `EXFAT_BENCH_MAX_BYTES` caps how much of the image each pass reads.

use ps5upload_fpkg::exfat::ExFatSource;
use ps5upload_fpkg::source::SourceTree;
use std::time::Instant;

fn main() {
    let mut args = std::env::args().skip(1);
    let Some(path) = args.next() else {
        eprintln!("usage: exfat_read_bench <image.exfat> [chunk KiB...]");
        std::process::exit(2);
    };
    let mut chunks: Vec<u64> = args.map(|a| a.parse::<u64>().unwrap() << 10).collect();
    if chunks.is_empty() {
        chunks = vec![64 << 10, 1 << 20, 8 << 20, 32 << 20];
    }
    let cap: u64 = std::env::var("EXFAT_BENCH_MAX_BYTES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(u64::MAX);
    let mode = std::env::var("EXFAT_BENCH_HASH").unwrap_or_else(|_| "single".into());
    for chunk in chunks {
        let mut tree = ExFatSource::open(std::path::Path::new(&path)).expect("open");
        let files = tree.files().to_vec();
        let start = Instant::now();
        let mut total = 0u64;
        let mut n = 0usize;
        'outer: for f in &files {
            let mut h = blake3::Hasher::new();
            let mut at = 0u64;
            while at < f.size {
                if total >= cap {
                    break 'outer;
                }
                let want = (f.size - at).min(chunk) as usize;
                let buf = tree.read_range(&f.path, at, want).expect("read");
                assert!(!buf.is_empty());
                match mode.as_str() {
                    "none" => {}
                    "rayon" => {
                        h.update_rayon(&buf);
                    }
                    _ => {
                        h.update(&buf);
                    }
                }
                at += buf.len() as u64;
                total += buf.len() as u64;
            }
            h.finalize();
            n += 1;
        }
        let secs = start.elapsed().as_secs_f64();
        println!(
            "hash {mode:<6} chunk {:>6} KiB: {} files, {:.2} GiB in {:.2} s = {:.0} MiB/s",
            chunk >> 10,
            n,
            total as f64 / (1u64 << 30) as f64,
            secs,
            total as f64 / (1u64 << 20) as f64 / secs
        );
    }
}
