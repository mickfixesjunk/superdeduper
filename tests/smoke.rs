//! End-to-end smoke test: plant some files in a temp directory and verify
//! the pipeline finds the right duplicate groups.
//!
//! This test runs on every platform — it exercises the platform-agnostic
//! pipeline (size grouping + buffered BLAKE3). Once the IOCP fast path
//! lands we'll add a parallel Windows-only test that drives it.

use std::fs;
use std::path::PathBuf;

use superdupe::cli::OutputFormat;
use superdupe::config::ScanConfig;
use superdupe::pipeline;

fn temp_root() -> PathBuf {
    let mut dir = std::env::temp_dir();
    dir.push(format!(
        "superdupe-smoke-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn finds_planted_duplicates() {
    let root = temp_root();
    let a = root.join("a.bin");
    let b = root.join("nested").join("b.bin");
    let c = root.join("nested").join("c.bin");
    let d = root.join("unique.bin");
    fs::create_dir_all(b.parent().unwrap()).unwrap();

    let dup_payload = vec![0xAB; 32 * 1024];
    let unique_payload = vec![0xCD; 32 * 1024];

    fs::write(&a, &dup_payload).unwrap();
    fs::write(&b, &dup_payload).unwrap();
    fs::write(&c, &dup_payload).unwrap();
    fs::write(&d, &unique_payload).unwrap();

    let cfg = ScanConfig {
        roots: vec![root.clone()],
        reference_roots: vec![],
        min_size: 0,
        max_size: None,
        include: None,
        exclude: None,
        format: OutputFormat::Text,
        paranoid: false,
        use_cache: false,
        use_format_aware: false,
        threads: 2,
        queue_depth: None,
        output: None,
        follow_links: false,
        allow_system_paths: false,
        io_threads: 4,
        hash_algo: superdupe::pipeline::hash::HashAlgo::Blake3,
    };

    let inv = superdupe::inventory::enumerate(&cfg).unwrap();
    assert_eq!(inv.len(), 4, "expected 4 files in inventory, got {}", inv.len());

    let groups = pipeline::grouping::group_by_size(inv);
    let laid = pipeline::layout::resolve(groups).unwrap();
    let dups = pipeline::hash::run(laid, &cfg).unwrap();

    assert_eq!(dups.len(), 1, "expected exactly one duplicate group");
    let g = &dups[0];
    assert_eq!(g.files.len(), 3);
    assert_eq!(g.size, dup_payload.len() as u64);
    assert!(g.files.contains(&a));
    assert!(g.files.contains(&b));
    assert!(g.files.contains(&c));
    assert!(!g.files.contains(&d));

    fs::remove_dir_all(&root).ok();
}

#[test]
fn empty_directory_yields_no_groups() {
    let root = temp_root();
    let cfg = ScanConfig {
        roots: vec![root.clone()],
        reference_roots: vec![],
        min_size: 0,
        max_size: None,
        include: None,
        exclude: None,
        format: OutputFormat::Text,
        paranoid: false,
        use_cache: false,
        use_format_aware: false,
        threads: 1,
        queue_depth: None,
        output: None,
        follow_links: false,
        allow_system_paths: false,
        io_threads: 4,
        hash_algo: superdupe::pipeline::hash::HashAlgo::Blake3,
    };
    let inv = superdupe::inventory::enumerate(&cfg).unwrap();
    assert!(inv.is_empty());
    let groups = pipeline::grouping::group_by_size(inv);
    assert!(groups.is_empty());
    fs::remove_dir_all(&root).ok();
}
