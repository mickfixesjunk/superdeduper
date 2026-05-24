//! End-to-end smoke test: plant some files in a temp directory and verify
//! the pipeline finds the right duplicate groups.
//!
//! This test runs on every platform — it exercises the platform-agnostic
//! pipeline (size grouping + buffered BLAKE3). Once the IOCP fast path
//! lands we'll add a parallel Windows-only test that drives it.

use std::fs;
use std::path::PathBuf;

use superdeduper::cli::OutputFormat;
use superdeduper::config::ScanConfig;
use superdeduper::pipeline;

fn temp_root() -> PathBuf {
    let mut dir = std::env::temp_dir();
    dir.push(format!(
        "superdeduper-smoke-{}-{}",
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
        tier1_bytes: superdeduper::pipeline::hash::TIER1_BYTES,
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
        allow_recall_on_read: false,
        io_threads: 4,
        hash_algo: superdeduper::pipeline::hash::HashAlgo::Blake3,
    };

    let inv = superdeduper::inventory::enumerate(&cfg, None).unwrap();
    assert_eq!(
        inv.len(),
        4,
        "expected 4 files in inventory, got {}",
        inv.len()
    );

    let groups = pipeline::grouping::group_by_size(inv);
    let laid = pipeline::layout::resolve(groups).unwrap();
    let dups = pipeline::hash::run(laid, &cfg).unwrap();

    assert_eq!(dups.len(), 1, "expected exactly one duplicate group");
    let g = &dups[0];
    assert_eq!(g.files.len(), 3);
    assert_eq!(g.size, dup_payload.len() as u64);

    // Compare on basenames + suspect that file's directory rather
    // than exact-path equality. Two cross-platform realities make
    // exact comparison brittle:
    //   * NTFS stores filenames in their on-disk case; %TEMP% can
    //     report the same path in different case. MFT-derived
    //     paths come back with NTFS's case, walker-derived paths
    //     match the input.
    //   * `std::fs::canonicalize` returns a verbatim `\\?\` form
    //     on Windows; neither MFT nor walk paths carry that
    //     prefix, so canonicalizing one side doesn't help either.
    // We do verify that the dup-set is the right three files vs
    // the unique one — that's the actual property we want.
    let names: std::collections::HashSet<String> = g
        .files
        .iter()
        .filter_map(|p| p.file_name().and_then(|n| n.to_str()))
        .map(|s| s.to_lowercase())
        .collect();
    assert!(names.contains("a.bin"), "missing a.bin in {:?}", g.files);
    assert!(names.contains("b.bin"), "missing b.bin in {:?}", g.files);
    assert!(names.contains("c.bin"), "missing c.bin in {:?}", g.files);
    assert!(
        !names.contains("unique.bin"),
        "unique.bin should NOT be in the dup group"
    );

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
        tier1_bytes: superdeduper::pipeline::hash::TIER1_BYTES,
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
        allow_recall_on_read: false,
        io_threads: 4,
        hash_algo: superdeduper::pipeline::hash::HashAlgo::Blake3,
    };
    let inv = superdeduper::inventory::enumerate(&cfg, None).unwrap();
    assert!(inv.is_empty());
    let groups = pipeline::grouping::group_by_size(inv);
    assert!(groups.is_empty());
    fs::remove_dir_all(&root).ok();
}
