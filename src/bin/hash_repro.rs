//! Standalone hash-cost reproducer.
//!
//! Generates a synthetic corpus that matches a file-size histogram
//! approximating real superdeduper workloads (AppData / C:\Users),
//! then drives the exact tier 1 / tier 2 / tier 3 sequence Stage 4
//! uses against each file. Reports per-tier wallclock and per-file
//! mean cost. Lets us measure the per-call hash overhead without
//! the noise of MFT walks, duplicate grouping, or layout work.
//!
//! Use case: hand to river5 so they can iterate on `river5_hash()`
//! changes against the exact workload superdeduper produces.
//!
//! Subcommands:
//!   gen-corpus  build N synthetic files into a directory
//!   bench       run the tier sequence over an existing corpus

use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use clap::{Parser, Subcommand};
use rayon::prelude::*;

use superdeduper::pipeline::hash::algo::{hash_oneshot, ContentHasher, HashAlgo};

// Same constants as src/pipeline/hash.rs. Kept in sync by hand —
// this binary exists specifically to mirror Stage 4 timing without
// depending on the private items.
const TIER1_BYTES: u64 = 4 * 1024;
const TIER2_REGION: u64 = 64 * 1024;
const TIER2_MIN_FILE: u64 = 256 * 1024;
const TIER3_ONESHOT_THRESHOLD: u64 = 1 << 20;
const TIER3_BUF: usize = 1 << 20;

#[derive(Parser)]
#[command(version, about = "superdeduper hash-cost reproducer", long_about = None)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Generate a synthetic corpus matching a file-size histogram.
    GenCorpus {
        /// Directory to create. Will be created if missing.
        #[arg(long)]
        dir: PathBuf,
        /// Target total corpus size in bytes (drives how many files
        /// to generate so the histogram tail doesn't blow up disk).
        #[arg(long, default_value_t = 1u64 << 30)]
        total_bytes: u64,
        /// Optional JSON histogram override. If absent, uses the
        /// built-in AppData-style distribution.
        #[arg(long)]
        histogram: Option<PathBuf>,
        /// Fraction of files that are members of a duplicate group
        /// (0.0 = every file unique; 1.0 = every file is part of
        /// some duplicate group). Files in the same group share
        /// size + content. Default 0.0 (back-compatible: no dups).
        #[arg(long, default_value_t = 0.0)]
        dup_ratio: f64,
        /// Average member count per duplicate group. Only meaningful
        /// when `--dup-ratio > 0`. Default 3 — produces "keeper +
        /// 2 dups" shape which is what most real workloads show.
        #[arg(long, default_value_t = 3)]
        avg_group_size: usize,
    },
    /// Run the Stage-4 tier sequence against an existing corpus.
    Bench {
        /// Directory containing the corpus (from `gen-corpus`).
        #[arg(long)]
        dir: PathBuf,
        /// Hash algorithm to bench.
        #[arg(long, value_enum, default_value_t = HashAlgoArg::River5)]
        hash_algo: HashAlgoArg,
        /// Rayon worker count (matches superdeduper's io_threads).
        #[arg(long, default_value_t = 8)]
        threads: usize,
        /// Number of full passes over the corpus (timings averaged).
        #[arg(long, default_value_t = 1)]
        repetitions: usize,
    },
}

#[derive(Copy, Clone, Debug, clap::ValueEnum)]
enum HashAlgoArg {
    Blake3,
    River5,
}
impl From<HashAlgoArg> for HashAlgo {
    fn from(a: HashAlgoArg) -> Self {
        match a {
            HashAlgoArg::Blake3 => HashAlgo::Blake3,
            HashAlgoArg::River5 => HashAlgo::River5,
        }
    }
}

/// Log-bucketed file-size histogram. `weight` is relative count;
/// `(lo, hi)` is the size range in bytes (inclusive lo, exclusive hi).
/// A representative file is drawn from each bucket using a uniform
/// distribution over `[lo, hi)`.
#[derive(Clone, Debug, serde::Deserialize)]
struct Bucket {
    lo: u64,
    hi: u64,
    weight: f64,
}

/// Default histogram approximating AppData / C:\Users by file count.
/// Skewed heavily to small files, with a long thin tail of bigger
/// ones. Tuned by hand from the benchmarker's prior corpus shapes
/// (530k files / 14 GiB on AppData; 1.15M files / 40 GiB on
/// C:\Users — both have median file under 4 KiB).
fn default_histogram() -> Vec<Bucket> {
    vec![
        Bucket {
            lo: 0,
            hi: 1024,
            weight: 35.0,
        },
        Bucket {
            lo: 1024,
            hi: 4 * 1024,
            weight: 20.0,
        },
        Bucket {
            lo: 4 * 1024,
            hi: 16 * 1024,
            weight: 15.0,
        },
        Bucket {
            lo: 16 * 1024,
            hi: 64 * 1024,
            weight: 10.0,
        },
        Bucket {
            lo: 64 * 1024,
            hi: 256 * 1024,
            weight: 8.0,
        },
        Bucket {
            lo: 256 * 1024,
            hi: 1024 * 1024,
            weight: 6.0,
        },
        Bucket {
            lo: 1024 * 1024,
            hi: 4 * 1024 * 1024,
            weight: 4.0,
        },
        Bucket {
            lo: 4 * 1024 * 1024,
            hi: 16 * 1024 * 1024,
            weight: 1.5,
        },
        Bucket {
            lo: 16 * 1024 * 1024,
            hi: 64 * 1024 * 1024,
            weight: 0.5,
        },
    ]
}

fn load_histogram(path: Option<&Path>) -> anyhow::Result<Vec<Bucket>> {
    if let Some(p) = path {
        let text = std::fs::read_to_string(p)?;
        Ok(serde_json::from_str(&text)?)
    } else {
        Ok(default_histogram())
    }
}

/// Cheap deterministic PRNG (xorshift64). The corpus content is
/// random bytes — we don't need cryptographic quality, but we DO
/// want reproducibility so the same `--total-bytes` produces the
/// same corpus across runs and machines. Seeding with file index
/// makes each generated file unique without ever colliding.
fn xorshift_fill(buf: &mut [u8], mut seed: u64) {
    for chunk in buf.chunks_mut(8) {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        let bytes = seed.to_le_bytes();
        for (i, b) in chunk.iter_mut().enumerate() {
            *b = bytes[i];
        }
    }
}

/// Pick a bucket using stable round-robin weighting. Sums of weights
/// drive how many files end up in each bucket; we floor() the count
/// from the weighted share to keep the math reproducible.
fn plan_file_sizes(hist: &[Bucket], total_bytes: u64) -> Vec<u64> {
    // Mean size per bucket (midpoint), weighted average gives the
    // expected per-file size — divide total_bytes by that to land
    // a per-bucket file count that hits the target total.
    let weight_sum: f64 = hist.iter().map(|b| b.weight).sum();
    let mean_size: f64 = hist
        .iter()
        .map(|b| ((b.lo + b.hi) as f64 / 2.0) * (b.weight / weight_sum))
        .sum();
    let target_files = (total_bytes as f64 / mean_size).max(1.0) as u64;

    let mut sizes = Vec::with_capacity(target_files as usize);
    for b in hist {
        let n = ((b.weight / weight_sum) * target_files as f64).round() as u64;
        for i in 0..n {
            // Spread uniformly within bucket to avoid every file
            // being identical; deterministic from the bucket lo +
            // index so the corpus stays reproducible.
            let span = (b.hi - b.lo).max(1);
            let off = i.wrapping_mul(2654435761) % span;
            sizes.push(b.lo + off);
        }
    }
    sizes
}

fn gen_corpus(
    dir: &Path,
    total_bytes: u64,
    histogram: Option<PathBuf>,
    dup_ratio: f64,
    avg_group_size: usize,
) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir)?;
    let hist = load_histogram(histogram.as_deref())?;
    let sizes = plan_file_sizes(&hist, total_bytes);

    let dup_ratio = dup_ratio.clamp(0.0, 1.0);
    let avg_group_size = avg_group_size.max(2);
    let n_files = sizes.len();
    // Number of files that should end up in dup groups (rest stay
    // unique). Round-down so we never accidentally promote a unique
    // to a dup-of-one.
    let n_dup_files = ((n_files as f64) * dup_ratio).floor() as usize;
    // Number of distinct dup groups, each averaging `avg_group_size`
    // members. Clamp to at least 1 when we have any dup files at all.
    let n_groups = if n_dup_files == 0 {
        0
    } else {
        (n_dup_files / avg_group_size).max(1)
    };

    // Per-file "content group id": indices [0, n_dup_files) round-
    // robin into dup groups [0, n_groups); indices [n_dup_files,
    // n_files) get unique group ids offset past `n_groups` so they
    // never collide with dup-group ids and stay byte-distinct.
    // Files in the SAME group must share size (otherwise their
    // contents would differ in length and they wouldn't actually be
    // byte-identical), so we lock each group's size to whatever the
    // histogram sampled for its first file index.
    let mut group_size_by_gid: hashbrown::HashMap<u64, u64> =
        hashbrown::HashMap::with_capacity(n_groups + (n_files - n_dup_files));
    let mut group_assignments = Vec::with_capacity(n_files);
    let mut actual_sizes = Vec::with_capacity(n_files);
    for i in 0..n_files {
        let gid: u64 = if i < n_dup_files {
            (i % n_groups.max(1)) as u64
        } else {
            (n_groups + (i - n_dup_files)) as u64
        };
        let sz = *group_size_by_gid.entry(gid).or_insert(sizes[i]);
        group_assignments.push(gid);
        actual_sizes.push(sz);
    }

    println!(
        "generating {} files into {} (target ~{} bytes, {} dup groups, {} dup-member files)",
        n_files,
        dir.display(),
        total_bytes,
        n_groups,
        n_dup_files
    );

    let started = Instant::now();
    let actual: u64 = (0..n_files)
        .into_par_iter()
        .map(|idx| -> anyhow::Result<u64> {
            let sz = actual_sizes[idx];
            let gid = group_assignments[idx];
            let path = dir.join(format!("f{:09}.bin", idx));
            let mut f = File::create(&path)?;
            // Stream out in 1 MiB chunks so a 64 MiB file doesn't
            // hold its whole content in memory.
            let mut remaining = sz as usize;
            let chunk = (TIER3_BUF).min(remaining.max(1));
            let mut buf = vec![0u8; chunk];
            // Seed derives from the CONTENT GROUP id, not the file
            // index. Two files in the same gid get the same initial
            // seed and the same per-chunk seed-advancement, so they
            // produce byte-identical content.
            let mut seed = gid.wrapping_add(0x9E37_79B9_7F4A_7C15);
            while remaining > 0 {
                let n = remaining.min(buf.len());
                xorshift_fill(&mut buf[..n], seed);
                seed = seed.wrapping_add(0xBF58_476D_1CE4_E5B9);
                f.write_all(&buf[..n])?;
                remaining -= n;
            }
            Ok(sz)
        })
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .sum();

    println!(
        "corpus done: {} files, {:.2} GiB written in {:.1}s ({} dup groups, {} unique files)",
        n_files,
        actual as f64 / (1024.0 * 1024.0 * 1024.0),
        started.elapsed().as_secs_f64(),
        n_groups,
        n_files - n_dup_files
    );
    Ok(())
}

/// Enumerate `dir` non-recursively and return the synthetic files
/// in the order they were emitted by the OS. Synthetic corpora are
/// flat (one directory) by construction — keeps the enumerator
/// cost predictable so it doesn't pollute the hash-time numbers.
fn list_corpus(dir: &Path) -> anyhow::Result<Vec<(PathBuf, u64)>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let e = entry?;
        let meta = e.metadata()?;
        if meta.is_file() {
            out.push((e.path(), meta.len()));
        }
    }
    Ok(out)
}

struct TierTimings {
    tier1_ns: u64,
    tier2_ns: u64,
    tier3_ns: u64,
    files: u64,
    bytes_hashed: u64,
}
impl TierTimings {
    fn zero() -> Self {
        Self {
            tier1_ns: 0,
            tier2_ns: 0,
            tier3_ns: 0,
            files: 0,
            bytes_hashed: 0,
        }
    }
}

/// Mirror of `src/pipeline/hash.rs::tier1_hash`. Open, read first
/// TIER1_BYTES, single hash_oneshot call. Times the open+read+hash
/// together — that's what Stage 4 measures.
fn tier1(path: &Path, size: u64, algo: HashAlgo) -> std::io::Result<u64> {
    let to_read = size.min(TIER1_BYTES) as usize;
    let mut buf = vec![0u8; to_read];
    let mut file = File::open(path)?;
    let n = file.read(&mut buf)?;
    buf.truncate(n);
    let _ = hash_oneshot(algo, &buf);
    Ok(n as u64)
}

/// Mirror of `tier2_hash`: 3 × 64 KiB regions (head/mid/tail) into
/// one contiguous buffer, single hash_oneshot.
fn tier2(path: &Path, size: u64, algo: HashAlgo) -> std::io::Result<u64> {
    let region = TIER2_REGION as usize;
    let mut combined = vec![0u8; 3 * region];

    let mut file = File::open(path)?;
    let h = file.read(&mut combined[..region])?;

    let mid_off = size.saturating_sub(TIER2_REGION) / 2;
    file.seek(SeekFrom::Start(mid_off))?;
    let m = file.read(&mut combined[region..2 * region])?;

    let tail_off = size.saturating_sub(TIER2_REGION);
    file.seek(SeekFrom::Start(tail_off))?;
    let t = file.read(&mut combined[2 * region..])?;

    let _ = hash_oneshot(algo, &combined);
    Ok((h + m + t) as u64)
}

/// Mirror of `tier3_hash_cancellable` (sans cancel — this is a
/// bench, we let it run). Small files take the slurp+oneshot path;
/// large files take the chunked-BufReader streaming path.
fn tier3(path: &Path, size: u64, algo: HashAlgo) -> std::io::Result<u64> {
    if size <= TIER3_ONESHOT_THRESHOLD {
        let mut file = File::open(path)?;
        let mut buf = Vec::with_capacity(size as usize);
        file.read_to_end(&mut buf)?;
        let n = buf.len() as u64;
        let _ = hash_oneshot(algo, &buf);
        return Ok(n);
    }
    let file = File::open(path)?;
    let mut reader = BufReader::with_capacity(TIER3_BUF, file);
    let mut hasher = ContentHasher::new(algo);
    let mut buf = vec![0u8; TIER3_BUF];
    let mut total = 0u64;
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        total += n as u64;
    }
    let _ = hasher.finalize();
    Ok(total)
}

fn bench_one_pass(
    files: &[(PathBuf, u64)],
    algo: HashAlgo,
    pool: &rayon::ThreadPool,
) -> TierTimings {
    let started = Instant::now();
    let totals = pool.install(|| {
        files
            .par_iter()
            .map(|(path, size)| {
                let mut t = TierTimings::zero();
                // Tier 1 always runs.
                let t1 = Instant::now();
                let b1 = tier1(path, *size, algo).unwrap_or(0);
                t.tier1_ns += t1.elapsed().as_nanos() as u64;

                // Tier 2 only for files ≥ TIER2_MIN_FILE — matches
                // the dispatcher in pipeline/hash.rs (line 281).
                let mut b_t2 = 0u64;
                if *size >= TIER2_MIN_FILE {
                    let t2 = Instant::now();
                    b_t2 = tier2(path, *size, algo).unwrap_or(0);
                    t.tier2_ns += t2.elapsed().as_nanos() as u64;
                }

                // Tier 3 always runs — that's the full-content
                // hash that confirms the duplicate.
                let t3 = Instant::now();
                let b3 = tier3(path, *size, algo).unwrap_or(0);
                t.tier3_ns += t3.elapsed().as_nanos() as u64;

                t.files += 1;
                t.bytes_hashed += b1 + b_t2 + b3;
                t
            })
            .reduce(TierTimings::zero, |mut a, b| {
                a.tier1_ns += b.tier1_ns;
                a.tier2_ns += b.tier2_ns;
                a.tier3_ns += b.tier3_ns;
                a.files += b.files;
                a.bytes_hashed += b.bytes_hashed;
                a
            })
    });
    let wall = started.elapsed();
    println!("    pass wallclock: {:.2}s", wall.as_secs_f64());
    totals
}

fn bench(dir: &Path, algo: HashAlgo, threads: usize, repetitions: usize) -> anyhow::Result<()> {
    let files = list_corpus(dir)?;
    if files.is_empty() {
        anyhow::bail!("no files found in {}", dir.display());
    }
    let total_bytes: u64 = files.iter().map(|(_, s)| *s).sum();
    println!(
        "corpus: {} files, {:.2} GiB, dir={}",
        files.len(),
        total_bytes as f64 / (1024.0 * 1024.0 * 1024.0),
        dir.display()
    );
    println!(
        "algo: {:?}    threads: {}    reps: {}",
        algo, threads, repetitions
    );

    // Build the rayon pool with the requested worker count. Named
    // pool — matches superdeduper's "superdeduper-io-N" convention
    // so a `Get-Process` snapshot during the bench is identifiable.
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(threads.max(1))
        .thread_name(|i| format!("hash-repro-{i}"))
        .build()?;

    let mut sum_t1 = Duration::ZERO;
    let mut sum_t2 = Duration::ZERO;
    let mut sum_t3 = Duration::ZERO;
    let mut sum_files = 0u64;
    let mut sum_bytes = 0u64;
    for rep in 0..repetitions {
        println!("--- rep {} ---", rep + 1);
        let t = bench_one_pass(&files, algo, &pool);
        sum_t1 += Duration::from_nanos(t.tier1_ns);
        sum_t2 += Duration::from_nanos(t.tier2_ns);
        sum_t3 += Duration::from_nanos(t.tier3_ns);
        sum_files += t.files;
        sum_bytes += t.bytes_hashed;
    }

    let total_cpu_ns = sum_t1 + sum_t2 + sum_t3;
    println!();
    println!("=== bench summary ===");
    println!("algo:                  {:?}", algo);
    println!("threads:               {}", threads);
    println!("reps:                  {}", repetitions);
    println!("files per rep:         {}", files.len());
    println!(
        "bytes hashed total:    {} ({:.2} GiB)",
        sum_bytes,
        sum_bytes as f64 / (1024.0 * 1024.0 * 1024.0)
    );
    println!("tier 1 total CPU:      {:.2}s", sum_t1.as_secs_f64());
    println!("tier 2 total CPU:      {:.2}s", sum_t2.as_secs_f64());
    println!("tier 3 total CPU:      {:.2}s", sum_t3.as_secs_f64());
    println!("total CPU across tiers: {:.2}s", total_cpu_ns.as_secs_f64());
    println!(
        "effective MiB/s (bytes/CPU): {:.0}",
        (sum_bytes as f64 / (1024.0 * 1024.0)) / total_cpu_ns.as_secs_f64().max(1e-9)
    );
    // Per-file mean — useful for spotting per-call overhead.
    let mean_per_file_ns = total_cpu_ns.as_nanos() as f64 / sum_files.max(1) as f64;
    println!("mean per file CPU:     {:.0} ns", mean_per_file_ns);

    Ok(())
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::GenCorpus {
            dir,
            total_bytes,
            histogram,
            dup_ratio,
            avg_group_size,
        } => {
            gen_corpus(&dir, total_bytes, histogram, dup_ratio, avg_group_size)?;
        }
        Cmd::Bench {
            dir,
            hash_algo,
            threads,
            repetitions,
        } => {
            bench(&dir, hash_algo.into(), threads, repetitions)?;
        }
    }
    Ok(())
}
