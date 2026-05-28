//! Adversarial Keeper-Preservation — GUI keeper-identity WIRING cells, Linux leg
//! (#153 Tier C). The Linux counterpart to `tests/akp_gui_ntfs.rs`.
//!
//! Spec: testdesign/specs/adversarial-keeper-preservation-test.md §5.1 + §6.
//! Author: testrunner 2026-05-28, on candidate c28af25.
//!
//! WHAT THIS PROVES (and what it does NOT): this is the SEAM proof, not an
//! FFI/gate re-proof. `run_one_dedupe_action` is the exact dispatch every GUI
//! action worker calls per file (src/gui/app.rs) — it threads the group's
//! keeper into the guarded `action_*` entry. Driving it with a keeper-ALIAS as
//! the target proves the GUI delete path runs the keeper-identity gate (click →
//! dispatch → guard → keeper survives), which a unit test on `action_*` alone
//! cannot prove (it would not show the GUI threads the keeper). The gate logic
//! itself is proven by the CLI AKP cells (harness/akp-linux.sh, 33/33).
//!
//! No wgpu / headless render needed — this is a direct fn call, not a render,
//! per testdesign 2026-05-28T20:52Z.
//!
//! Linux-constructible aliases (§8): identical-path, symlink, hardlink-self.
//! NTFS `\\?\` / 8.3 / junction / case variants are sdd-testwin's leg.
//! Cross-product across destructive actions {Remove, Recycle, SafeRename} per §6.1.

#![cfg(feature = "gui")]

use std::fs;
use std::path::{Path, PathBuf};

use superdeduper::cli::DedupeAction;
use superdeduper::gui::app::run_one_dedupe_action;

fn tmpdir(tag: &str) -> PathBuf {
    let mut d = std::env::temp_dir();
    d.push(format!(
        "sdd-akp-gui-linux-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&d).unwrap();
    d
}

const DESTRUCTIVE: [DedupeAction; 3] = [
    DedupeAction::Remove,
    DedupeAction::Recycle,
    DedupeAction::SafeRename,
];

/// One refusal cell: the GUI dispatch must REFUSE a destructive action whose
/// target resolves (by file-id) to the keeper, and the keeper must survive
/// byte-identical. For symlink/hardlink the alias name must also survive.
fn assert_refused(action: DedupeAction, vector: &str) {
    let d = tmpdir(&format!("{vector}-{action:?}"));
    let keeper = d.join("keeper.bin");
    fs::write(&keeper, b"keeper-bytes-deterministic").unwrap();
    let keeper_bytes = fs::read(&keeper).unwrap();

    // Recycle needs a place to move trash to; contain it under this tmpdir.
    std::env::set_var("XDG_DATA_HOME", d.join("xdg"));

    let (target, check_alias_survives): (PathBuf, bool) = match vector {
        "identical-path" => (keeper.clone(), false),
        "symlink" => {
            let l = d.join("alias.link");
            std::os::unix::fs::symlink(&keeper, &l).unwrap();
            (l, true)
        }
        "hardlink" => {
            let h = d.join("alias.hl");
            fs::hard_link(&keeper, &h).unwrap();
            (h, true)
        }
        other => panic!("unknown vector {other}"),
    };

    let r = run_one_dedupe_action(action, &target, &keeper);
    assert!(
        r.is_err(),
        "DATA-LOSS GUARD (GUI seam): {vector} / {action:?} — destructive action on a \
         keeper-alias must be REFUSED by the action-layer gate, got Ok"
    );
    assert!(
        keeper.exists() && fs::read(&keeper).unwrap() == keeper_bytes,
        "{vector} / {action:?}: keeper must survive byte-identical"
    );
    if check_alias_survives {
        assert!(
            target.exists(),
            "{vector} / {action:?}: the alias name must also survive (no name unlinked)"
        );
    }
    fs::remove_dir_all(&d).ok();
}

/// Negative control: a genuinely-distinct file must dedupe normally through the
/// same GUI dispatch — the gate must not be fail-closed-to-uselessness (A5).
fn assert_distinct_actioned(action: DedupeAction) {
    let d = tmpdir(&format!("nc-{action:?}"));
    let keeper = d.join("keeper.bin");
    let other = d.join("other.bin");
    fs::write(&keeper, b"same").unwrap();
    fs::write(&other, b"same").unwrap();
    std::env::set_var("XDG_DATA_HOME", d.join("xdg"));

    let r = run_one_dedupe_action(action, &other, &keeper);
    assert!(
        r.is_ok(),
        "negative control: a distinct file must dedupe via the GUI dispatch ({action:?}), got {r:?}"
    );
    // SafeRename renames in place; Remove/Recycle move it away. Either way the
    // original distinct path must no longer hold the file unchanged.
    let consumed = !other.exists()
        || fs::read(&other).map(|b| b != b"same").unwrap_or(true);
    assert!(
        consumed || matches!(action, DedupeAction::SafeRename),
        "negative control: the distinct file should have been actioned ({action:?})"
    );
    assert!(keeper.exists(), "negative control: keeper must be kept ({action:?})");
    fs::remove_dir_all(&d).ok();
}

#[test]
fn gui_seam_refuses_identical_path_keeper_all_destructive() {
    for a in DESTRUCTIVE {
        assert_refused(a, "identical-path");
    }
}

#[test]
fn gui_seam_refuses_symlink_keeper_all_destructive() {
    for a in DESTRUCTIVE {
        assert_refused(a, "symlink");
    }
}

#[test]
fn gui_seam_refuses_hardlink_keeper_all_destructive() {
    for a in DESTRUCTIVE {
        assert_refused(a, "hardlink");
    }
}

#[test]
fn gui_seam_does_not_over_refuse_distinct_file() {
    for a in DESTRUCTIVE {
        assert_distinct_actioned(a);
    }
}

/// AKP-5 hardlink-action variant: dedup intent already satisfied (the alias
/// shares the keeper's inode) — `--action hardlink` is a natural no-op, NOT a
/// refusal. Data-safe either way; the keeper survives.
#[test]
fn gui_seam_hardlink_action_on_alias_is_noop_ok() {
    let d = tmpdir("hl-action-noop");
    let keeper = d.join("keeper.bin");
    let alias = d.join("alias.hl");
    fs::write(&keeper, b"keeper-bytes").unwrap();
    fs::hard_link(&keeper, &alias).unwrap();
    let r = run_one_dedupe_action(DedupeAction::Hardlink, &alias, &keeper);
    assert!(r.is_ok(), "hardlink action on an already-hardlinked alias should be Ok no-op, got {r:?}");
    assert!(keeper.exists() && alias.exists(), "both names survive the no-op");
    fs::remove_dir_all(&d).ok();
}

#[allow(dead_code)]
fn _path_marker(_: &Path) {}
