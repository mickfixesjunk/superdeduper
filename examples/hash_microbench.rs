//! Microbench: river5-aes-ni vs BLAKE3 ns/byte on a 100 MB pseudorandom
//! buffer in memory. Removes filesystem + page-cache + tier-dispatch
//! variables — pure hash-kernel throughput.
//!
//! Run: `cargo run --release --example hash_microbench`

use std::time::Instant;

use superdeduper::pipeline::hash::algo::{hash_oneshot, HashAlgo};

fn fill_pseudo(buf: &mut [u8]) {
    // Cheap deterministic fill — xorshift64. Not for crypto; for
    // microbench input only.
    let mut s: u64 = 0x1234_5678_9abc_def0;
    let mut i = 0;
    while i + 8 <= buf.len() {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        buf[i..i + 8].copy_from_slice(&s.to_le_bytes());
        i += 8;
    }
    for j in i..buf.len() {
        buf[j] = (s as u8).wrapping_add(j as u8);
    }
}

fn bench(label: &str, algo: HashAlgo, buf: &[u8], trials: usize) {
    let mut best_ns_per_byte = f64::MAX;
    let mut worst_ns_per_byte: f64 = 0.0;
    let mut total_ns = 0u128;
    let bytes = buf.len() as u128;

    // Warm up — one untimed pass.
    let warm = hash_oneshot(algo, buf);
    std::hint::black_box(&warm);

    for t in 0..trials {
        let start = Instant::now();
        let out = hash_oneshot(algo, buf);
        let elapsed_ns = start.elapsed().as_nanos();
        std::hint::black_box(&out);
        total_ns += elapsed_ns;

        let ns_per_byte = elapsed_ns as f64 / buf.len() as f64;
        best_ns_per_byte = best_ns_per_byte.min(ns_per_byte);
        worst_ns_per_byte = worst_ns_per_byte.max(ns_per_byte);

        let mbps = (bytes as f64) / (elapsed_ns as f64) * 1e9 / (1024.0 * 1024.0);
        eprintln!(
            "  trial {:2}: {:9.2} ms  {:6.3} ns/byte  {:8.1} MiB/s",
            t + 1,
            elapsed_ns as f64 / 1e6,
            ns_per_byte,
            mbps,
        );
    }

    let median_ns_per_byte = total_ns as f64 / (trials as f64 * buf.len() as f64);
    let median_mbps = (bytes * trials as u128) as f64 / total_ns as f64 * 1e9 / (1024.0 * 1024.0);
    eprintln!(
        "  {label}: median {:6.3} ns/byte ({:8.1} MiB/s), best {:6.3}, worst {:6.3}",
        median_ns_per_byte, median_mbps, best_ns_per_byte, worst_ns_per_byte,
    );
}

fn main() {
    const SIZE: usize = 100 * 1024 * 1024;
    const TRIALS: usize = 7;

    eprintln!("Allocating + filling 100 MiB pseudorandom buffer");
    let mut buf = vec![0u8; SIZE];
    fill_pseudo(&mut buf);

    eprintln!("\n=== river5-aes-ni ({} trials, 100 MiB) ===", TRIALS);
    bench("river5", HashAlgo::River5, &buf, TRIALS);

    eprintln!("\n=== blake3 ({} trials, 100 MiB) ===", TRIALS);
    bench("blake3", HashAlgo::Blake3, &buf, TRIALS);
}
