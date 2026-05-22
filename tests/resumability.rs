//! Tests for the checkpoint save/load/round-trip used by the GUI's
//! Pause / Resume flow.
//!
//! The Pause → Resume contract is: when the user pauses mid-scan,
//! the GUI writes a checkpoint file containing the roots, settings,
//! the inventory walk (if Stage 1 finished), and any duplicate
//! groups already confirmed. On next launch, the Resume modal reads
//! that checkpoint via `summary()` to show the user what was paused,
//! then `load()`s the full state and resumes the scan from where it
//! stopped. These tests cover the persistence side of that contract;
//! the actual "resume from a partial scan" engine logic is covered
//! by the integration smoke tests.
//!
//! Why not a single big "scan→pause→resume→verify" integration
//! test: that would need a real scan engine running for tens of
//! seconds and a way to fire a Pause event from a test harness.
//! We get most of the safety by testing the checkpoint round-trip
//! exhaustively here, plus the engine's own unit tests for resume
//! handling — that's the boundary where bugs would actually creep
//! in.

#![cfg(feature = "gui")]

use std::path::PathBuf;

use superdeduper::gui::checkpoint::{self, Checkpoint, SavedFileEntry};
use superdeduper::gui::events::DuplicateGroupSummary;
use superdeduper::gui::state::{RootEntry, ScanSettings};

fn sample_checkpoint() -> Checkpoint {
    let roots = vec![
        RootEntry {
            path: PathBuf::from("C:\\Users\\Audio"),
            is_reference: false,
        },
        RootEntry {
            path: PathBuf::from("C:\\Users\\Audio\\Documents"),
            is_reference: true,
        },
    ];
    let settings = ScanSettings::default();
    let mut cp = Checkpoint::new(roots, settings);
    cp.record(&DuplicateGroupSummary {
        size: 1024,
        content_hash: "abcd1234".into(),
        files: vec![
            PathBuf::from("C:\\a\\dup1.bin"),
            PathBuf::from("C:\\a\\dup2.bin"),
        ],
    });
    cp.record(&DuplicateGroupSummary {
        size: 4096,
        content_hash: "deadbeef".into(),
        files: vec![
            PathBuf::from("C:\\b\\dup3.bin"),
            PathBuf::from("C:\\b\\dup4.bin"),
        ],
    });
    cp.saved_inventory = Some(vec![SavedFileEntry {
        path: PathBuf::from("C:\\Users\\Audio\\f.bin"),
        size: 1024,
        mtime: 1_700_000_000,
        file_ref: 42,
        parent_ref: 1,
        usn: 9001,
        attributes: 0x20,
        volume_guid: Some("\\\\?\\Volume{abc}\\".into()),
    }]);
    cp
}

#[test]
fn checkpoint_round_trips_via_save_and_load() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("checkpoint.json");
    let cp = sample_checkpoint();

    checkpoint::save(&path, &cp).expect("save");
    let loaded = checkpoint::load(&path).expect("load").expect("present");

    // Spot-check every field that matters for resume correctness.
    assert_eq!(loaded.schema, cp.schema, "schema string");
    assert_eq!(loaded.roots.len(), cp.roots.len(), "roots count");
    assert_eq!(loaded.roots[0].path, cp.roots[0].path);
    assert_eq!(loaded.roots[1].is_reference, true);
    assert_eq!(
        loaded.completed_hashes, cp.completed_hashes,
        "completed_hashes vec"
    );
    assert_eq!(
        loaded.previous_duplicates.len(),
        2,
        "duplicate groups preserved"
    );
    assert_eq!(loaded.previous_duplicates[0].content_hash, "abcd1234");
    assert_eq!(loaded.previous_duplicates[1].size, 4096);

    let inv = loaded.saved_inventory.expect("inventory persisted");
    assert_eq!(inv.len(), 1);
    assert_eq!(inv[0].file_ref, 42);
    assert_eq!(inv[0].usn, 9001);
    assert_eq!(inv[0].volume_guid.as_deref(), Some("\\\\?\\Volume{abc}\\"));
}

#[test]
fn checkpoint_summary_reports_counts_without_loading_full_state() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("checkpoint.json");
    let cp = sample_checkpoint();
    checkpoint::save(&path, &cp).unwrap();

    let summary = checkpoint::summary(&path).unwrap().expect("present");
    assert_eq!(summary.roots.len(), 2);
    assert_eq!(summary.duplicate_count, 2);
    assert!(summary.has_saved_inventory);
}

#[test]
fn checkpoint_load_returns_none_when_file_missing() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("does-not-exist.json");
    let result = checkpoint::load(&path).expect("load on missing file is OK");
    assert!(result.is_none(), "missing checkpoint should be None, not Err");
}

#[test]
fn checkpoint_delete_removes_the_file() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("checkpoint.json");
    let cp = sample_checkpoint();
    checkpoint::save(&path, &cp).unwrap();
    assert!(path.exists(), "saved file exists");

    checkpoint::delete(&path).unwrap();
    assert!(!path.exists(), "file removed by delete()");

    // Idempotent: deleting an already-gone file shouldn't error.
    checkpoint::delete(&path).unwrap();
}

#[test]
fn checkpoint_round_trip_preserves_settings_default_round_trip() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("checkpoint.json");
    let cp = Checkpoint::new(vec![], ScanSettings::default());
    checkpoint::save(&path, &cp).unwrap();
    let loaded = checkpoint::load(&path).unwrap().unwrap();
    // Default settings should round-trip identically — this catches
    // any future ScanSettings field added without #[serde(default)]
    // breaking older checkpoints.
    assert_eq!(loaded.settings, cp.settings);
}

#[test]
fn checkpoint_corrupt_file_returns_err_or_none() {
    // Two ways "corrupt" can manifest: invalid JSON → load returns
    // Err; we expect the GUI to handle Err by calling mark_corrupt
    // and starting fresh. This test pins the behavior so a regression
    // (e.g. silently returning Ok(None)) gets caught.
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("checkpoint.json");
    std::fs::write(&path, b"{not valid json").unwrap();
    let result = checkpoint::load(&path);
    assert!(
        result.is_err(),
        "corrupt JSON should surface as Err, got: {:?}",
        result
    );

    // mark_corrupt should rename the file rather than deleting it,
    // so a curious user can post-mortem the bad checkpoint.
    let archived = checkpoint::mark_corrupt(&path).unwrap();
    if let Some(archived_path) = archived {
        assert!(archived_path.exists(), "corrupt file moved aside");
        assert!(!path.exists(), "original removed");
    }
}
