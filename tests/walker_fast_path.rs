//! Block N — walker fast-path smoke test.
//!
//! Runs end-to-end against a fixture directory and asserts that the
//! walker returns the right files with the right sizes. On Windows
//! this exercises `walk_fast_path` (via FileIdBothDirectoryInfo); on
//! Linux it exercises the read_dir fallback. Both produce equivalent
//! FileEntry vecs — that's the contract.
//!
//! On Windows specifically, additionally asserts that `file_ref` is
//! populated (the fast path's whole point — Stage 2b skip).

use std::path::PathBuf;

use superdeduper::config::ScanConfig;
use superdeduper::{inventory, pipeline};

fn tmpdir() -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "sd-walker-fast-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn cfg_for(roots: Vec<PathBuf>) -> ScanConfig {
    ScanConfig {
        roots,
        reference_roots: vec![],
        min_size: 1,
        max_size: None,
        tier1_bytes: pipeline::hash::TIER1_BYTES,
        include: None,
        exclude: None,
        format: superdeduper::cli::OutputFormat::Text,
        use_cache: false,
        use_format_aware: false,
        threads: 2,
        output: None,
        follow_links: false,
        allow_system_paths: false,
        allow_recall_on_read: false,
        io_threads: 4,
        hash_algo: pipeline::hash::HashAlgo::Blake3,
        exclusion_policy: superdeduper::exclusions::ExclusionPolicy::disabled(),
        exclusion_counters: superdeduper::exclusions::ExclusionCounters::new(),
    }
}

#[test]
fn walker_finds_basic_files() {
    let d = tmpdir();
    std::fs::write(d.join("a.txt"), b"hello").unwrap();
    std::fs::write(d.join("b.txt"), b"world").unwrap();
    std::fs::write(d.join("c.bin"), vec![0u8; 64]).unwrap();

    let cfg = cfg_for(vec![d.clone()]);
    let (files, _skipped) = inventory::enumerate_with_skipped(&cfg, None).unwrap();

    let names: Vec<String> = files
        .iter()
        .map(|f| f.path.file_name().unwrap().to_string_lossy().to_string())
        .collect();
    assert_eq!(files.len(), 3, "expected 3 files, got {:?}", names);
    assert!(names.contains(&"a.txt".to_string()));
    assert!(names.contains(&"b.txt".to_string()));
    assert!(names.contains(&"c.bin".to_string()));

    // Sizes must match what we wrote.
    for f in &files {
        let expected = match f.path.file_name().unwrap().to_string_lossy().as_ref() {
            "a.txt" => 5,
            "b.txt" => 5,
            "c.bin" => 64,
            other => panic!("unexpected file {other}"),
        };
        assert_eq!(f.size, expected, "size for {:?}", f.path);
    }

    std::fs::remove_dir_all(&d).ok();
}

#[test]
fn walker_recurses_into_subdirs() {
    let d = tmpdir();
    std::fs::write(d.join("top.txt"), b"abc").unwrap();
    let sub = d.join("subdir");
    std::fs::create_dir(&sub).unwrap();
    std::fs::write(sub.join("nested.txt"), b"defg").unwrap();
    let deeper = sub.join("deeper");
    std::fs::create_dir(&deeper).unwrap();
    std::fs::write(deeper.join("deep.txt"), b"hijkl").unwrap();

    let cfg = cfg_for(vec![d.clone()]);
    let (files, _skipped) = inventory::enumerate_with_skipped(&cfg, None).unwrap();

    let mut names: Vec<String> = files
        .iter()
        .map(|f| f.path.file_name().unwrap().to_string_lossy().to_string())
        .collect();
    names.sort();
    assert_eq!(
        names,
        vec!["deep.txt", "nested.txt", "top.txt"],
        "walker should recurse into subdirs and find all 3 files"
    );

    std::fs::remove_dir_all(&d).ok();
}

#[test]
fn walker_applies_min_size_filter() {
    let d = tmpdir();
    std::fs::write(d.join("tiny.txt"), b"x").unwrap(); // 1 byte
    std::fs::write(d.join("small.txt"), vec![0u8; 100]).unwrap(); // 100 bytes

    let mut cfg = cfg_for(vec![d.clone()]);
    cfg.min_size = 50;

    let (files, _) = inventory::enumerate_with_skipped(&cfg, None).unwrap();
    let names: Vec<String> = files
        .iter()
        .map(|f| f.path.file_name().unwrap().to_string_lossy().to_string())
        .collect();
    assert_eq!(
        files.len(),
        1,
        "only one file above min_size=50, got {names:?}"
    );
    assert_eq!(names, vec!["small.txt"]);

    std::fs::remove_dir_all(&d).ok();
}

/// Windows-only: when the fast path fires, every FileEntry should
/// have a non-zero file_ref AND a populated volume_guid. That's the
/// whole point of Block N — pre-populating these so Stage 2b
/// short-circuits per-file.
#[cfg(windows)]
#[test]
fn walker_fast_path_populates_file_ref_on_windows() {
    let d = tmpdir();
    std::fs::write(d.join("a.txt"), b"hello").unwrap();
    std::fs::write(d.join("b.txt"), b"world").unwrap();

    let cfg = cfg_for(vec![d.clone()]);
    let (files, _) = inventory::enumerate_with_skipped(&cfg, None).unwrap();
    assert_eq!(files.len(), 2);

    for f in &files {
        assert_ne!(
            f.file_ref, 0,
            "fast path should populate file_ref; got 0 for {:?}",
            f.path
        );
        assert!(
            f.volume_guid.is_some(),
            "fast path should populate volume_guid; got None for {:?}",
            f.path
        );
        assert!(
            f.attributes != 0,
            "fast path should populate file attributes; got 0 for {:?}",
            f.path
        );
    }

    std::fs::remove_dir_all(&d).ok();
}
