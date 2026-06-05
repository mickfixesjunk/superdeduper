//! Tier-A GUI operation driver — Linux headless leg (#153 §3 a/b/c).
//!
//! Spec: testdesign/specs/gui-e2e-coverage-plan.md §3 (CORE-action UI-layer
//! cells, SHARED backend) — G-RECYCLE/REMOVE/HARDLINK/SAFERENAME + G-SUBMIT.
//! Author: testrunner 2026-05-28 (testdesign owns the cell spec; I build the
//! harness + run; testdesign reviews against the spec).
//!
//! Approach (testdesign-confirmed, no wgpu): drive the REAL SuperdeduperApp via
//! egui_kittest's build_eframe(SuperdeduperApp::new) — frame-step + input
//! simulation, NOT rasterization. The keeper-safety WIRING half is already done
//! via the run_one_dedupe_action seam (tests/akp_gui_linux.rs); THIS file is the
//! operation-burn (present / triggers / effect).
//!
//! STEP 1 (this commit): keystone capability — the full app instantiates +
//! steps frames headless (no GPU/window). Each operation cell builds on this.
//! XDG dirs are isolated to a tempdir so the launch-time Resume modal stays
//! absent (empty checkpoint) and the cache/scan-history are hermetic.

#![cfg(feature = "gui")]

use std::path::PathBuf;
use std::time::Duration;

use accesskit::Role;
use egui_kittest::kittest::Queryable;
use superdeduper::gui::app::SuperdeduperApp;

/// Tests in this file mutate PROCESS-GLOBAL env (XDG_DATA_HOME / HOME via
/// set_var) to isolate each app instance. cargo runs tests in parallel threads,
/// so without serialization they clobber each other's XDG/HOME mid-run (observed:
/// recycle + reference flaked only when run together). This process-wide lock
/// serializes them — each test holds it for its full duration. Poison-tolerant
/// (a panicking test shouldn't wedge the rest).
fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// Build a scannable corpus in its OWN tempdir (NOT under the isolated XDG/HOME):
/// 3 byte-identical 8 KiB files (a dup group — 8 KiB clears the 4 KiB GUI
/// min_size floor) + 1 distinct file (cascade-safety negative control).
/// Returns (corpus_dir, [keeper, loser_a, loser_b, distinct]).
fn make_dup_corpus(tag: &str) -> (PathBuf, [PathBuf; 4]) {
    let mut dir = std::env::temp_dir();
    dir.push(format!(
        "sdd-tier-a-corpus-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let mut blob = b"tier-a-dup-content-".to_vec();
    blob.resize(8192, 0);
    let keeper = dir.join("keeper.bin");
    let loser_a = dir.join("loser_a.bin");
    let loser_b = dir.join("loser_b.bin");
    let distinct = dir.join("distinct.bin");
    std::fs::write(&keeper, &blob).unwrap();
    std::fs::write(&loser_a, &blob).unwrap();
    std::fs::write(&loser_b, &blob).unwrap();
    let mut other = b"tier-a-DISTINCT-content-".to_vec();
    other.resize(8192, 1);
    std::fs::write(&distinct, &other).unwrap();
    (dir, [keeper, loser_a, loser_b, distinct])
}

/// Isolate XDG_* + HOME + SUPERDEDUPER_TEST_DATA_DIR to a fresh tempdir so
/// app::new() sees no checkpoint (no Resume modal), a hermetic cache, and a
/// clean scan-history. Returns the root so the caller can inspect/clean it.
///
/// XDG_* covers Linux; SUPERDEDUPER_TEST_DATA_DIR covers cross-platform via
/// install.rs::data_dir + cache.rs::default_cache_path + scan_history.rs::data_dir
/// (gui/checkpoint.rs::default_checkpoint_path inherits transitively). Without
/// the env-var, sdd-testwin's Windows runs hit real %LOCALAPPDATA%\\superdeduper
/// state from prior real runs (stale settings shadow test roots, dismissed
/// alpha modal hides the Continue button, leftover checkpoint races with the
/// boot path) — that's the 8/9-cells-fail-on-Windows finding's likely root.
fn isolated_env(tag: &str) -> PathBuf {
    let mut root = std::env::temp_dir();
    root.push(format!(
        "sdd-tier-a-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(root.join("data")).unwrap();
    std::fs::create_dir_all(root.join("config")).unwrap();
    std::fs::create_dir_all(root.join("cache")).unwrap();
    std::env::set_var("XDG_DATA_HOME", root.join("data"));
    std::env::set_var("XDG_CONFIG_HOME", root.join("config"));
    std::env::set_var("XDG_CACHE_HOME", root.join("cache"));
    std::env::set_var("HOME", &root);
    // Cross-platform engine-side hermetic redirect; the engine's
    // install/cache/scan_history/checkpoint resolvers all honor this first.
    std::env::set_var("SUPERDEDUPER_TEST_DATA_DIR", root.join("data"));
    root
}

#[test]
fn tier_a_app_instantiates_and_steps_frames_headless() {
    let _env = env_lock();
    let root = isolated_env("boot");
    // build_eframe gives us a CreationContext (storage=None) and runs the app's
    // update() each harness.run() — egui layout on CPU, no wgpu adapter.
    let mut harness = egui_kittest::Harness::builder()
        .build_eframe(|cc| SuperdeduperApp::new(cc));
    // Step several frames: proves update() renders the real UI tree headlessly
    // without panicking (theme install, image loaders, no-checkpoint launch path).
    for _ in 0..5 {
        harness.run();
    }
    // The app object exists + is reachable post-frames (state accessor works).
    let _app: &SuperdeduperApp = harness.state();
    std::fs::remove_dir_all(&root).ok();
}

/// Click EVERY node whose label contains `substr`. kittest's first label match
/// for a control can be a non-interactive label/tooltip rather than the Button
/// node (clicking it is a no-op), which silently drops the interaction. Clicking
/// all matches reliably hits the real Button (clicking a label is harmless) as
/// long as the substring is unique to one control (true for our distinctive
/// tokens: "Start scan", "scan →", "Go (", "Confirm", "Continue"). Returns the
/// number of nodes clicked.
fn click_all(harness: &egui_kittest::Harness<'_, SuperdeduperApp>, substr: &str) -> usize {
    let mut n = 0;
    for node in harness.query_all_by_label_contains(substr) {
        node.click();
        n += 1;
    }
    n
}

/// STEP 2 (§6 scan a/b/c foundation + the seed-roots seam end-to-end):
/// seed a scan root via the pub add_root (the exact fn the rfd dialog calls),
/// click the REAL "Start scan" button, and confirm the worker-thread scan
/// completes + populates the clickable groups table — headless, no rfd, no wgpu.
/// This is the prerequisite for every Tier-A action cell; if it works the whole
/// headless GUI-E2E surface (Tier-A + §6 + E2E-10) is unblocked.
#[test]
fn tier_a_seed_root_then_real_scan_populates_table() {
    let _env = env_lock();
    let home = isolated_env("scan");
    let (corpus, _files) = make_dup_corpus("scan");
    let corpus_for_closure = corpus.clone();

    // a (present): boot the real app + seed the scan root (rfd dialog output).
    let mut harness = egui_kittest::Harness::builder()
        .with_size([1400.0_f32, 900.0])
        .build_eframe(move |cc| {
            let mut app = SuperdeduperApp::new(cc);
            app.add_root(corpus_for_closure.clone(), false);
            app
        });
    // NOTE: use harness.step() (single frame) not run() — run() panics
    // ("exceeded max_steps") whenever a spinner / continuous repaint is active
    // (the preflight probe + the live scan both animate).
    for _ in 0..3 {
        harness.step();
    }
    // First-launch alpha-warning modal (app.rs:3045) overlays + early-returns the
    // main UI until dismissed. Click "Continue" (session-dismiss) to reach the app.
    click_all(&harness, "Continue");
    for _ in 0..3 {
        harness.step();
    }
    assert!(
        harness.query_all_by_label_contains("Start scan").next().is_some(),
        "STEP=present: Start-scan control should render once a root is seeded + alpha modal dismissed"
    );

    // b (triggers): click the REAL Scan button (click_all — the first label match
    // can be a non-button node; clicking all reliably hits the Button).
    click_all(&harness, "Start scan");

    // The click opens an async PREFLIGHT modal (Probing + spinner); its proceed is
    // "Start scan →" ("scan →" is unique to it). Advance it each frame, then the
    // worker scan runs and the bulk "Go (N files)" button appears (real-state:
    // gated on visible_dupe_count > 0 — NOT the "Got it" banner).
    let mut populated = false;
    for _ in 0..400 {
        harness.step();
        click_all(&harness, "scan →"); // advance preflight proceed when present
        if harness.query_all_by_label_contains("Go (").next().is_some() {
            populated = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        populated,
        "STEP=scan: real scan did not populate the clickable groups table (no 'Go (' bulk button after seed+Start scan+preflight)"
    );

    std::fs::remove_dir_all(&corpus).ok();
    std::fs::remove_dir_all(&home).ok();
}

/// Boot the real app headless with `corpus` seeded as a scan root, dismiss the
/// first-launch alpha modal, click the real Start scan, and wait until the
/// groups table populates. Shared setup for every action cell. Returns the
/// harness + corpus dir + [keeper, loser_a, loser_b, distinct] + isolated HOME.
fn boot_and_scan(
    tag: &str,
) -> (
    egui_kittest::Harness<'static, SuperdeduperApp>,
    PathBuf,
    [PathBuf; 4],
    PathBuf,
) {
    let home = isolated_env(tag);
    let (corpus, files) = make_dup_corpus(tag);
    let corpus_c = corpus.clone();
    let mut harness = egui_kittest::Harness::builder()
        .with_size([1400.0_f32, 900.0])
        .build_eframe(move |cc| {
            let mut app = SuperdeduperApp::new(cc);
            app.add_root(corpus_c.clone(), false);
            app
        });
    // step() not run() — run() panics on the preflight/scan spinner's repaint.
    for _ in 0..3 {
        harness.step();
    }
    click_all(&harness, "Continue"); // dismiss alpha-warning modal
    for _ in 0..3 {
        harness.step();
    }
    click_all(&harness, "Start scan"); // main scan button (click_all reliably hits it)
    let mut ok = false;
    for _ in 0..400 {
        harness.step();
        click_all(&harness, "scan →"); // advance the async preflight proceed
        if harness.query_all_by_label_contains("Go (").next().is_some() {
            ok = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(ok, "boot_and_scan[{tag}]: groups table did not populate after scan");
    dismiss_scan_complete_if_present(&mut harness); // telemetry: clear the overlay so the table is reachable
    (harness, corpus, files, home)
}

/// Step frames until `pred` holds or the budget expires (worker threads + event
/// drain are async). Returns whether `pred` became true.
fn run_until(
    harness: &mut egui_kittest::Harness<'static, SuperdeduperApp>,
    mut pred: impl FnMut() -> bool,
) -> bool {
    for _ in 0..400 {
        harness.step();
        if pred() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    false
}

/// Under --features telemetry, a scan-complete modal (Ready) OVERLAYS the table
/// after a scan finishes — clicking the bulk combo/Go is then blocked. "Skip
/// this time" dismisses it (-> Hidden), revealing the table. No-op under
/// --features gui (the modal is cfg'd out). Cells that need the TABLE call this
/// after the table populates; G-SUBMIT does NOT (it drives the modal's Submit).
fn dismiss_scan_complete_if_present(harness: &mut egui_kittest::Harness<'static, SuperdeduperApp>) {
    for _ in 0..20 {
        if click_all(harness, "Skip this time") > 0 {
            for _ in 0..3 {
                harness.step();
            }
            return;
        }
        harness.step();
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// §3 a/b/c action driver, shared by the destructive bulk cells. On a populated
/// table: (a) assert the bulk Go control is present; select `option_substr` in
/// the bulk combo; (b) click Go; drive the #85 typed-confirm modal (focus the
/// field + type `confirm_word` + Confirm). Leaves the action worker dispatched —
/// the caller polls the FS for the (c) effect. STEP-labeled.
fn drive_bulk_action(
    harness: &mut egui_kittest::Harness<'static, SuperdeduperApp>,
    option_substr: &str,
    confirm_word: &str,
) {
    // a (present): the bulk Go control renders on a populated table.
    assert!(
        harness.query_all_by_label_contains("Go (").next().is_some(),
        "STEP=present: bulk Go control should be present on a populated table"
    );
    // open the bulk-action combo (default selection text "🛡 Safe-rename dupes"),
    // fall back to the ComboBox role node, then pick the target option.
    if click_all(harness, "Safe-rename dupes") == 0 {
        for n in harness.query_all_by_role(Role::ComboBox) {
            n.click();
        }
    }
    for _ in 0..3 {
        harness.step();
    }
    assert!(
        click_all(harness, option_substr) > 0,
        "STEP=select: bulk option {option_substr:?} not found in combo popup"
    );
    for _ in 0..3 {
        harness.step();
    }
    // b (triggers): click Go -> destructive variant routes to the #85 modal.
    assert!(click_all(harness, "Go (") > 0, "STEP=click: Go ( button not found");
    for _ in 0..3 {
        harness.step();
    }
    // #85 typed-confirm: focus the singleline TextEdit (type_text targets the
    // focused widget — without focus the chars are lost + Confirm stays disabled),
    // type the per-action word, then Confirm.
    {
        let mut found = false;
        for ti in harness.query_all_by_role(Role::TextInput) {
            ti.focus();
            ti.type_text(confirm_word);
            found = true;
        }
        assert!(found, "STEP=confirm: confirm-modal TextEdit (Type {confirm_word}) not found");
    }
    for _ in 0..3 {
        harness.step();
    }
    assert!(click_all(harness, "Confirm") > 0, "STEP=confirm: Confirm button not found");
    for _ in 0..3 {
        harness.step();
    }
}

/// G-REMOVE (§3 a/b/c) — permanent delete via bulk "Nuke dupes", through the #85
/// confirm (DELETE). Effect: both losers GONE from disk; keeper + distinct kept.
#[test]
fn tier_a_g_remove_actions_losers_preserves_keeper() {
    let _env = env_lock();
    let (mut harness, corpus, files, home) = boot_and_scan("remove");
    let [keeper, loser_a, loser_b, distinct] = files;
    drive_bulk_action(&mut harness, "Nuke dupes", "DELETE");
    let losers_gone = run_until(&mut harness, || !loser_a.exists() && !loser_b.exists());
    assert!(
        losers_gone,
        "STEP=effect: losers not removed (loser_a exists={}, loser_b exists={})",
        loser_a.exists(),
        loser_b.exists()
    );
    assert!(keeper.exists(), "STEP=effect: keeper must be preserved (DATA-LOSS GUARD)");
    assert!(distinct.exists(), "STEP=effect: distinct non-dup file must be untouched (cascade safety)");
    std::fs::remove_dir_all(&corpus).ok();
    std::fs::remove_dir_all(&home).ok();
}

/// G-RECYCLE (§3 a/b/c) — bulk "Recycle dupes" through the #85 confirm (DELETE).
/// Effect: losers GONE from the corpus (moved to XDG Trash); keeper + distinct kept.
#[test]
fn tier_a_g_recycle_actions_losers_preserves_keeper() {
    let _env = env_lock();
    let (mut harness, corpus, files, home) = boot_and_scan("recycle");
    let [keeper, loser_a, loser_b, distinct] = files;
    drive_bulk_action(&mut harness, "Recycle dupes", "DELETE");
    let losers_gone = run_until(&mut harness, || !loser_a.exists() && !loser_b.exists());
    assert!(
        losers_gone,
        "STEP=effect: losers not recycled out of corpus (loser_a={}, loser_b={})",
        loser_a.exists(),
        loser_b.exists()
    );
    assert!(keeper.exists(), "STEP=effect: keeper must be preserved (DATA-LOSS GUARD)");
    assert!(distinct.exists(), "STEP=effect: distinct non-dup file must be untouched (cascade safety)");
    std::fs::remove_dir_all(&corpus).ok();
    std::fs::remove_dir_all(&home).ok();
}

/// G-SAFERENAME (§3 a/b/c) — bulk "Safe-rename dupes" through the #85 confirm
/// (RENAME). Effect: each loser RENAMED in place ({name}.superdeduper); the
/// original loser paths are gone, the .superdeduper variants exist; keeper kept.
#[test]
fn tier_a_g_saferename_renames_losers_preserves_keeper() {
    let _env = env_lock();
    let (mut harness, corpus, files, home) = boot_and_scan("saferename");
    let [keeper, loser_a, loser_b, distinct] = files;
    let renamed = |p: &PathBuf| {
        let n = format!("{}.superdeduper", p.file_name().unwrap().to_string_lossy());
        p.with_file_name(n)
    };
    let (ra, rb) = (renamed(&loser_a), renamed(&loser_b));
    drive_bulk_action(&mut harness, "Safe-rename dupes", "RENAME");
    let renamed_ok = run_until(&mut harness, || {
        ra.exists() && rb.exists() && !loser_a.exists() && !loser_b.exists()
    });
    assert!(
        renamed_ok,
        "STEP=effect: losers not safe-renamed ({}->{} exists={}, {}->{} exists={})",
        loser_a.display(), ra.display(), ra.exists(),
        loser_b.display(), rb.display(), rb.exists()
    );
    assert!(keeper.exists(), "STEP=effect: keeper must be preserved (DATA-LOSS GUARD)");
    assert!(distinct.exists(), "STEP=effect: distinct non-dup file must be untouched (cascade safety)");
    std::fs::remove_dir_all(&corpus).ok();
    std::fs::remove_dir_all(&home).ok();
}

/// Post-build scan driver shared by multi-root cells: dismiss the alpha modal,
/// click the real Start scan, advance the async preflight, wait until the bulk
/// "Go (N files)" button appears (table populated). Panics if it never populates.
fn drive_scan_to_table(harness: &mut egui_kittest::Harness<'static, SuperdeduperApp>) {
    for _ in 0..3 {
        harness.step();
    }
    click_all(harness, "Continue");
    for _ in 0..3 {
        harness.step();
    }
    click_all(harness, "Start scan");
    let mut ok = false;
    for _ in 0..400 {
        harness.step();
        click_all(harness, "scan →");
        if harness.query_all_by_label_contains("Go (").next().is_some() {
            ok = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(ok, "STEP=scan: groups table did not populate");
    dismiss_scan_complete_if_present(harness); // telemetry: clear the overlay so the table is reachable
}

/// G-REFERENCE-PROTECTION (§4 GUI ref-protection / [[adversarial-keeper...]] §7.4
/// GUI twin) — verifies v0.2.36 relocation #2: references are guarded AT THE GUI
/// ACTION LAYER. Mark a root as a reference, scan a dup group that spans the
/// reference file + 2 plain copies under a scan root, run a bulk destructive
/// action, and assert the REFERENCE-root file is PRESERVED (guard_reference
/// refused it) while a non-reference copy IS still actioned (selective — the
/// gate isn't fail-closed-to-uselessness, the A5 negative-control discipline).
#[test]
fn tier_a_g_reference_protection_preserves_reference_file() {
    let _env = env_lock();
    let home = isolated_env("refguard");

    // Two corpora: a SCAN root (2 plain dup copies) + a REFERENCE root (same
    // content). 8 KiB clears the GUI min_size floor.
    let base = std::env::temp_dir().join(format!(
        "sdd-tier-a-refguard-{}-{}",
        std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
    ));
    let scan_dir = base.join("scan");
    let ref_dir = base.join("reference");
    std::fs::create_dir_all(&scan_dir).unwrap();
    std::fs::create_dir_all(&ref_dir).unwrap();
    let mut blob = b"tier-a-reference-guard-content-".to_vec();
    blob.resize(8192, 0);
    let copy_a = scan_dir.join("copy_a.bin");
    let copy_b = scan_dir.join("copy_b.bin");
    let ref_file = ref_dir.join("source_of_truth.bin");
    std::fs::write(&copy_a, &blob).unwrap();
    std::fs::write(&copy_b, &blob).unwrap();
    std::fs::write(&ref_file, &blob).unwrap();
    let ref_sha = {
        use std::io::Read;
        let mut f = std::fs::File::open(&ref_file).unwrap();
        let mut v = Vec::new();
        f.read_to_end(&mut v).unwrap();
        v
    };

    let (sc, rc) = (scan_dir.clone(), ref_dir.clone());
    let mut harness = egui_kittest::Harness::builder()
        .with_size([1400.0_f32, 900.0])
        .build_eframe(move |cc| {
            let mut app = SuperdeduperApp::new(cc);
            app.add_root(sc.clone(), false); // scan root
            app.add_root(rc.clone(), true); // REFERENCE root (is_reference = true)
            app
        });
    drive_scan_to_table(&mut harness);

    // Bulk permanent-delete across the group (spans the reference + the 2 copies).
    drive_bulk_action(&mut harness, "Nuke dupes", "DELETE");

    // Let the worker run, then assert. The load-bearing claim: the file under the
    // REFERENCE root is preserved byte-identical (guard_reference refused to touch
    // it). Negative control: at least one non-reference copy WAS actioned (so the
    // gate refuses the reference without refusing everything).
    let acted = run_until(&mut harness, || !copy_a.exists() || !copy_b.exists());
    assert!(
        ref_file.exists() && std::fs::read(&ref_file).unwrap() == ref_sha,
        "STEP=effect: REFERENCE file under the reference root must be PRESERVED byte-identical (guard_reference) — exists={}",
        ref_file.exists()
    );
    assert!(
        acted,
        "STEP=effect: a non-reference copy should still be actioned (gate must not be fail-closed-to-uselessness) — copy_a={} copy_b={}",
        copy_a.exists(),
        copy_b.exists()
    );

    std::fs::remove_dir_all(&base).ok();
    std::fs::remove_dir_all(&home).ok();
}

/// G-HARDLINK (§3 a/b/c) — per-ROW "🔗 Hardlink" affordance (NOT the bulk combo).
/// Expand the group row (the "▸ <keeper>" selectable label), click Hardlink,
/// confirm with "HARDLINK", and assert the dupes are replaced with hardlinks to
/// the keeper: all member paths still exist (no path touched) + they collapse to
/// a SINGLE shared inode (the real on-disk effect), content byte-identical.
#[cfg(unix)]
#[test]
fn tier_a_g_hardlink_collapses_dupes_to_one_inode() {
    let _env = env_lock();
    use std::os::unix::fs::MetadataExt;
    let (mut harness, corpus, files, home) = boot_and_scan("hardlink");
    let [keeper, loser_a, loser_b, distinct] = files;

    // pre: the three dup members are distinct inodes (the collision to collapse).
    let ino = |p: &PathBuf| std::fs::metadata(p).map(|m| m.ino()).unwrap_or(0);
    let pre: std::collections::HashSet<u64> = [ino(&keeper), ino(&loser_a), ino(&loser_b)].into_iter().collect();
    assert_eq!(pre.len(), 3, "pre: the 3 dup members should start as distinct inodes, got {pre:?}");
    let dup_sha = std::fs::read(&keeper).unwrap();

    // a (present): expand the group row — the keeper-path column is a "▸ <path>"
    // selectable label; clicking it toggles expansion, revealing the per-row 🔗 Hardlink.
    assert!(click_all(&mut harness, "▸") > 0, "STEP=expand: collapsed group row (▸) not found");
    for _ in 0..3 {
        harness.step();
    }
    // b (triggers): click the per-row 🔗 Hardlink -> #85 modal (word HARDLINK).
    assert!(click_all(&mut harness, "Hardlink") > 0, "STEP=click: per-row 🔗 Hardlink button not found after expand");
    for _ in 0..3 {
        harness.step();
    }
    // #85 confirm: focus + type HARDLINK + Confirm.
    {
        let mut found = false;
        for ti in harness.query_all_by_role(Role::TextInput) {
            ti.focus();
            ti.type_text("HARDLINK");
            found = true;
        }
        assert!(found, "STEP=confirm: confirm-modal TextEdit (Type HARDLINK) not found");
    }
    for _ in 0..3 {
        harness.step();
    }
    assert!(click_all(&mut harness, "Confirm") > 0, "STEP=confirm: Confirm button not found");

    // c (effect): the worker replaces the dupes with hardlinks to the keeper —
    // all 3 member paths still exist + collapse to ONE shared inode, byte-identical.
    let collapsed = run_until(&mut harness, || {
        let s: std::collections::HashSet<u64> = [ino(&keeper), ino(&loser_a), ino(&loser_b)].into_iter().collect();
        keeper.exists() && loser_a.exists() && loser_b.exists() && s.len() == 1 && !s.contains(&0)
    });
    assert!(
        collapsed,
        "STEP=effect: dupes not hardlinked to one inode (keeper={}, a={}, b={})",
        ino(&keeper), ino(&loser_a), ino(&loser_b)
    );
    assert!(std::fs::read(&keeper).unwrap() == dup_sha, "STEP=effect: hardlinked content must be byte-identical");
    assert!(distinct.exists(), "STEP=effect: distinct non-dup file must be untouched (cascade safety)");

    std::fs::remove_dir_all(&corpus).ok();
    std::fs::remove_dir_all(&home).ok();
}

/// G-SUBMIT (§3 a/b/c) — PURE-HEADLESS (testdesign (b)): assert the GUI-unique
/// layer only — the scan-complete modal's Submit control is present (a), and
/// clicking it DISPATCHES the submit worker + transitions OUT of Ready into the
/// pre-POST "Submitting…" state (b/c). The actual POST -> submission_id / ranks /
/// Submitted-pill is the SHARED backend wire-contract, covered by the CLI
/// submit-pending rows + E2E-1 vs the dev server — NOT re-tested here (no network
/// in Tier-A; keeps it offline-deterministic).
#[test]
#[cfg(feature = "telemetry")] // scan-complete modal + leaderboard submit are telemetry-gated
fn tier_a_g_submit_dispatches_from_scan_complete_modal() {
    let _env = env_lock();
    let home = isolated_env("submit");
    let (corpus, _files) = make_dup_corpus("submit");
    let corpus_c = corpus.clone();
    let mut harness = egui_kittest::Harness::builder()
        .with_size([1400.0_f32, 900.0])
        .build_eframe(move |cc| {
            let mut app = SuperdeduperApp::new(cc);
            app.add_root(corpus_c.clone(), false);
            app
        });
    for _ in 0..3 {
        harness.step();
    }
    click_all(&harness, "Continue");
    for _ in 0..3 {
        harness.step();
    }
    click_all(&harness, "Start scan");
    // Scan-complete modal (Ready) shows "Submit to leaderboard" once the scan
    // finishes (reclaimable stats present — our corpus has a real dup group).
    let mut submit_present = false;
    for _ in 0..400 {
        harness.step();
        click_all(&harness, "scan →"); // advance the preflight proceed
        if harness.query_all_by_label_contains("Submit to leaderboard").next().is_some() {
            submit_present = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    // a (present)
    assert!(
        submit_present,
        "STEP=present: scan-complete modal 'Submit to leaderboard' control not found after scan"
    );

    // b (triggers): click Submit -> dispatch submit_recorded_payload + leave Ready.
    assert!(click_all(&harness, "Submit to leaderboard") > 0, "STEP=click: Submit control not clickable");
    // c (pre-POST state): the modal transitions to the "Submitting…" state, OR at
    // minimum leaves Ready (the Submit-to-leaderboard button is gone) — either
    // proves the dispatch fired. We do NOT assert Submitted (needs the network POST).
    let dispatched = {
        let mut ok = false;
        for _ in 0..200 {
            harness.step();
            let submitting = harness.query_all_by_label_contains("Submitting").next().is_some();
            let left_ready = harness.query_all_by_label_contains("Submit to leaderboard").next().is_none();
            if submitting || left_ready {
                ok = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        ok
    };
    assert!(
        dispatched,
        "STEP=dispatch: clicking Submit did not transition out of Ready into Submitting (dispatch not observed)"
    );

    std::fs::remove_dir_all(&corpus).ok();
    std::fs::remove_dir_all(&home).ok();
}

/// §6 scan (c)-ENHANCEMENT — the scan-complete modal renders the real headline
/// stats (group-count + reclaimable). Telemetry-gated (the modal is telemetry-
/// only). Real-state: our corpus is exactly 1 dup group of 3×8 KiB identical
/// files -> duplicate_groups == 1, reclaimable == (3-1)*8192 = 16 KiB. Asserts
/// the modal shows the "Reclaimable" + "Duplicate groups" stat labels AND the
/// correct reclaimable value (a feature break -> wrong/zero stat would fail).
#[test]
#[cfg(feature = "telemetry")] // scan-complete modal is telemetry-gated
fn tier_a_scan_complete_modal_shows_real_stats() {
    let _env = env_lock();
    let home = isolated_env("scanstats");
    let (corpus, _files) = make_dup_corpus("scanstats");
    let corpus_c = corpus.clone();
    let mut harness = egui_kittest::Harness::builder()
        .with_size([1400.0_f32, 900.0])
        .build_eframe(move |cc| {
            let mut app = SuperdeduperApp::new(cc);
            app.add_root(corpus_c.clone(), false);
            app
        });
    for _ in 0..3 {
        harness.step();
    }
    click_all(&harness, "Continue");
    for _ in 0..3 {
        harness.step();
    }
    click_all(&harness, "Start scan");
    // wait for the scan-complete modal (its "Reclaimable" stat label appears).
    let mut modal = false;
    for _ in 0..400 {
        harness.step();
        click_all(&harness, "scan →");
        if harness.query_all_by_label_contains("Reclaimable").next().is_some()
            && harness.query_all_by_label_contains("Duplicate groups").next().is_some()
        {
            modal = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(modal, "STEP=modal: scan-complete modal stat panel (Reclaimable + Duplicate groups) not shown");

    // Real-state: the reclaimable value reflects the actual dedup (16 KiB for our
    // 1 group of 3×8 KiB). humansize uses humansize::BINARY.
    let want = humansize::format_size(16384u64, humansize::BINARY);
    assert!(
        harness.query_all_by_label_contains(&want).next().is_some(),
        "STEP=stats: scan-complete modal should show reclaimable {want:?} (1 group, 2 losers × 8 KiB)"
    );

    std::fs::remove_dir_all(&corpus).ok();
    std::fs::remove_dir_all(&home).ok();
}

/// Action (c) FEEDBACK — the post-action scan-history record's
/// actually_reclaimed_bytes is patched to the EXACT actioned-loser bytes (the
/// #79 local patch via record_local_action_for_latest_scan, app.rs:1934 — fires
/// independent of any submission_id; the server PATCH is separate). Telemetry-
/// gated (scan-history submission surface). Real-state on the PERSISTED record
/// (source of truth, not a UI label): G-REMOVE of 2 losers × 8 KiB -> 16384
/// EXACTLY, EXCLUDING the keeper (would be 24576 if keeper-inflated -> bug).
#[test]
#[cfg(feature = "telemetry")]
fn tier_a_action_patches_scan_history_actually_reclaimed_bytes() {
    let _env = env_lock();
    let (mut harness, corpus, files, home) = boot_and_scan("reclaimpatch");
    let [_keeper, loser_a, loser_b, _distinct] = files;
    drive_bulk_action(&mut harness, "Nuke dupes", "DELETE");
    // ensure the action actually ran (losers gone) before checking the patch.
    let _ = run_until(&mut harness, || !loser_a.exists() && !loser_b.exists());

    // Poll the persisted scan-history record for the EXACT reclaim (16 KiB).
    // Tolerant of JSON whitespace: strip spaces and match the field:value.
    let want = "\"actually_reclaimed_bytes\":16384";
    let inflated = "\"actually_reclaimed_bytes\":24576"; // keeper-included = a bug
    let scan_history_dir = home.join("data").join("superdeduper").join("scan-history");
    let read_field = || -> (bool, bool) {
        let mut exact = false;
        let mut bad = false;
        if let Ok(rd) = std::fs::read_dir(&scan_history_dir) {
            for e in rd.flatten() {
                if let Ok(body) = std::fs::read_to_string(e.path()) {
                    let compact: String = body.chars().filter(|c| !c.is_whitespace()).collect();
                    if compact.contains(want) {
                        exact = true;
                    }
                    if compact.contains(inflated) {
                        bad = true;
                    }
                }
            }
        }
        (exact, bad)
    };
    let patched = run_until(&mut harness, || read_field().0);
    let (exact, keeper_inflated) = read_field();
    assert!(
        !keeper_inflated,
        "STEP=feedback: actually_reclaimed_bytes == 24576 — keeper's 8 KiB was INCLUDED (should EXCLUDE keeper; real bug)"
    );
    assert!(
        patched && exact,
        "STEP=feedback: post-action scan_history.actually_reclaimed_bytes != 16384 (exact actioned-loser bytes, keeper excluded) in {:?}",
        scan_history_dir
    );

    std::fs::remove_dir_all(&corpus).ok();
    std::fs::remove_dir_all(&home).ok();
}

/// G-SYSTEM-PATH-REFUSED (§4 / v0.2.36 gate-2) — verifies relocation #1 +
/// alias-hardening (372f42e + 1dd90ea): the GUI refuses a destructive action on
/// a member that resolves (via symlink — the alias-hardening) to a SYSTEM PATH.
/// guard_system_path checks is_system_path(raw) || is_system_path(canonical_key)
/// at all 5 action entries, so a corpus symlink whose canonical target is under
/// /etc//usr//bin//var/lib is refused. NOT telemetry-gated (core guard) — runs
/// under both profiles. Real-state: the system-aliased member is PRESERVED and
/// EXACTLY ONE plain copy survives (the keeper) ⟺ the guard refused the alias as
/// a LOSER (anti-vacuous: if the alias were merely the keeper, both plains would
/// be actioned -> 0 plains survive -> this FAILS, catching the vacuous case).
#[cfg(unix)]
#[test]
fn tier_a_g_system_path_alias_refused_preserves_target() {
    let _env = env_lock();
    let home = isolated_env("syspath");

    // A readable system file >=4 KiB (clears the GUI min_size) under a system
    // prefix; SKIP-with-reason if none on this box (anti-vacuous: don't fake it).
    let sys_file = ["/etc/services", "/etc/ssl/certs/ca-certificates.crt", "/etc/login.defs"]
        .into_iter()
        .map(std::path::PathBuf::from)
        .find(|p| std::fs::metadata(p).map(|m| m.is_file() && m.len() >= 4096).unwrap_or(false)
            && std::fs::read(p).is_ok()
            && crate_is_system_like(p));
    let sys_file = match sys_file {
        Some(p) => p,
        None => {
            eprintln!("SKIP tier_a_g_system_path: no readable >=4KiB file under a system prefix on this box");
            std::fs::remove_dir_all(&home).ok();
            return;
        }
    };
    let content = std::fs::read(&sys_file).unwrap();

    let base = std::env::temp_dir().join(format!(
        "sdd-tier-a-syspath-{}-{}",
        std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
    ));
    std::fs::create_dir_all(&base).unwrap();
    // Named so a path-ordered keeper pick lands on a plain file, not the alias.
    let a_keep = base.join("a_keep.bin");
    let b_loser = base.join("b_loser.bin");
    let z_alias = base.join("z_sysalias.bin"); // symlink -> system file (canonical = system path)
    std::fs::write(&a_keep, &content).unwrap();
    std::fs::write(&b_loser, &content).unwrap();
    std::os::unix::fs::symlink(&sys_file, &z_alias).unwrap();
    // A1: prove the alias canonicalizes under a system prefix (else the cell is vacuous).
    let canon = std::fs::canonicalize(&z_alias).unwrap();
    assert!(
        canon.starts_with("/etc/") || canon.starts_with("/usr/") || canon.starts_with("/bin/") || canon.starts_with("/var/lib/"),
        "A1: alias canonical {canon:?} is not under a system prefix"
    );

    let base_for_closure = base.clone();
    let mut harness = egui_kittest::Harness::builder()
        .with_size([1400.0_f32, 900.0])
        .build_eframe(move |cc| {
            let mut app = SuperdeduperApp::new(cc);
            app.add_root(base_for_closure.clone(), false);
            app
        });
    drive_scan_to_table(&mut harness);
    drive_bulk_action(&mut harness, "Nuke dupes", "DELETE");

    // Effect: the system-aliased member is PRESERVED (guard refused it). The
    // system target file itself is obviously untouched. Anti-vacuous: exactly one
    // plain survives (the keeper) => the alias was a refused LOSER, not the keeper.
    let preserved = run_until(&mut harness, || {
        z_alias.exists() && ([a_keep.exists(), b_loser.exists()].iter().filter(|x| **x).count() == 1)
    });
    let plains_surviving = [a_keep.exists(), b_loser.exists()].iter().filter(|x| **x).count();
    assert!(
        z_alias.exists(),
        "STEP=effect: system-path-aliased member must be PRESERVED (guard_system_path refused) — alias gone"
    );
    assert!(
        std::fs::metadata(&sys_file).is_ok(),
        "STEP=effect: the system target file must be untouched"
    );
    assert!(
        preserved && plains_surviving == 1,
        "STEP=effect: expected exactly 1 plain survivor (keeper) + alias refused as a LOSER; got {plains_surviving} plains surviving (0 => alias was keeper = vacuous; 2 => nothing actioned)"
    );

    std::fs::remove_dir_all(&base).ok();
    std::fs::remove_dir_all(&home).ok();
}

/// Mirror of dedupe::is_system_path's Linux prefix check (that fn is pub but we
/// avoid a cross-cfg import here) — used only to pre-validate the chosen target.
fn crate_is_system_like(p: &std::path::Path) -> bool {
    let s = p.to_string_lossy();
    s.starts_with("/etc/") || s.starts_with("/usr/") || s.starts_with("/bin/") || s.starts_with("/var/lib/")
}

// =============================================================================
// scan-perf ratio cell -- ship gate for v0.3.36+ GUI perf fixes
// Spec: workdirs/testdesign/specs/egui-kittest-scan-perf-assertion.md
// Author: sdd-testwin 2026-06-04 19:10 PDT post v0.3.37 verify
// =============================================================================

/// Generate the deterministic 1040-file perf corpus per spec §2:
///   500 unique-size files (1KB - 1MB random)
///   250 size-twin pairs = 500 files (4KB - 256KB; tier 1 head matches, tier 3 differs)
///    20 dup pairs       =  40 files (4KB - 1MB; exact duplicates)
/// Total ~1040 files, ~250 MiB, deterministic via fixed seed=0xC0FFEE.
fn generate_perf_test_corpus(tempdir_root: &std::path::Path) -> std::path::PathBuf {
    use std::io::Write;
    let dir = tempdir_root.join("perf-corpus-1040");
    if dir.exists() {
        std::fs::remove_dir_all(&dir).ok();
    }
    std::fs::create_dir_all(&dir).unwrap();

    // Cheap deterministic PRNG (xorshift64; seed=0xC0FFEE) -- avoid pulling in rand crate.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }
        fn fill(&mut self, buf: &mut [u8]) {
            for chunk in buf.chunks_mut(8) {
                let v = self.next().to_le_bytes();
                let n = chunk.len();
                chunk.copy_from_slice(&v[..n]);
            }
        }
        fn range(&mut self, lo: usize, hi: usize) -> usize {
            lo + (self.next() as usize) % (hi - lo)
        }
    }
    let mut rng = Rng(0x00C0FFEE);

    // 500 unique-size files; sizes spread 1KB-1MB
    for i in 0..500 {
        let size = rng.range(1024, 1024 * 1024);
        let mut buf = vec![0u8; size];
        rng.fill(&mut buf);
        let f = std::fs::File::create(dir.join(format!("uniq_{:04}.bin", i))).unwrap();
        std::io::BufWriter::new(f).write_all(&buf).unwrap();
    }

    // 250 size-twin pairs: each pair shares size but has DIFFERENT random content
    for i in 0..250 {
        let size = rng.range(4096, 256 * 1024);
        let mut a = vec![0u8; size];
        let mut b = vec![0u8; size];
        rng.fill(&mut a);
        rng.fill(&mut b);
        let fa = std::fs::File::create(dir.join(format!("twin_{:04}_a.bin", i))).unwrap();
        std::io::BufWriter::new(fa).write_all(&a).unwrap();
        let fb = std::fs::File::create(dir.join(format!("twin_{:04}_b.bin", i))).unwrap();
        std::io::BufWriter::new(fb).write_all(&b).unwrap();
    }

    // 20 dup pairs: each pair byte-identical (same random content written twice)
    for i in 0..20 {
        let size = rng.range(4096, 1024 * 1024);
        let mut buf = vec![0u8; size];
        rng.fill(&mut buf);
        let fa = std::fs::File::create(dir.join(format!("dup_{:04}_a.bin", i))).unwrap();
        std::io::BufWriter::new(fa).write_all(&buf).unwrap();
        let fb = std::fs::File::create(dir.join(format!("dup_{:04}_b.bin", i))).unwrap();
        std::io::BufWriter::new(fb).write_all(&buf).unwrap();
    }

    dir
}

/// Ship-gate cell: assert GUI scan wall <= N x CLI scan wall on the same hardware + corpus.
/// N default 3.0x; configurable via SUPERDEDUPER_TEST_PERF_RATIO env. v0.3.36+ ship gate while
/// engine iterates on GUI perf fixes. See spec for full rationale + tightening plan.
#[test]
fn tier_a_gui_scan_perf_within_cli_ratio() {
    let _env = env_lock();
    let _home = isolated_env("perfratio");
    let corpus_root_tmp = std::env::temp_dir();
    let corpus = generate_perf_test_corpus(&corpus_root_tmp);

    // ---------- Step 1: CLI baseline (with warmup pass) ----------
    // Warmup pass (NOT timed) per testdesign 2026-06-04 19:54 PDT: hot the OS page cache
    // for the corpus + binary's runtime init so the timed run measures steady-state CLI
    // cost, not first-MFT-walk + cold-disk-read overhead. Cold-vs-warm CLI noise was 6x
    // (0.58s cold first run vs 0.09s warm subsequent) which made the ratio gate flaky.
    let cli_exe = env!("CARGO_BIN_EXE_superdeduper");
    let _warmup = std::process::Command::new(cli_exe)
        .args(["scan", &corpus.to_string_lossy(), "--no-cache"])
        .output()
        .expect("CLI warmup scan failed to spawn");
    let t_cli_start = std::time::Instant::now();
    let cli_out = std::process::Command::new(cli_exe)
        .args(["scan", &corpus.to_string_lossy(), "--no-cache"])
        .output()
        .expect("CLI sd scan failed to spawn");
    let t_cli = t_cli_start.elapsed();
    assert!(
        cli_out.status.success(),
        "CLI scan exited non-zero: {} stderr-tail: {}",
        cli_out.status,
        String::from_utf8_lossy(&cli_out.stderr)
    );

    // ---------- Step 2: GUI via egui_kittest in-process ----------
    let corpus_for_closure = corpus.clone();
    let mut harness = egui_kittest::Harness::builder()
        .with_size([1400.0_f32, 900.0])
        .build_eframe(move |cc| {
            let mut app = SuperdeduperApp::new(cc);
            app.add_root(corpus_for_closure.clone(), false);
            app
        });
    for _ in 0..3 {
        harness.step();
    }
    click_all(&harness, "Continue"); // dismiss alpha modal
    for _ in 0..3 {
        harness.step();
    }
    let t_gui_start = std::time::Instant::now();
    click_all(&harness, "Start scan");
    let mut populated = false;
    let timeout_ratio = 6.0_f64;
    let timeout = t_cli.mul_f64(timeout_ratio).max(std::time::Duration::from_secs(30));
    let poll_deadline = std::time::Instant::now() + timeout;
    // Candidate-4 calibration telemetry per testdesign 19:38 PDT + 19:54 PDT (commit-permanent):
    // time each harness.step() to measure egui_kittest frame-pump cost vs engine scan work.
    // Finding 19:50 PDT: 65.6% of GUI walltime is poll-sleep yield (291 polls * 10ms), NOT
    // engine work. true engine work = total_step_time -- that's what O3 ratchet uses below.
    let mut step_count: u64 = 0;
    let mut total_step_time = std::time::Duration::ZERO;
    let mut max_step_time = std::time::Duration::ZERO;
    let mut sleep_count: u64 = 0;
    while std::time::Instant::now() < poll_deadline {
        let t_step = std::time::Instant::now();
        harness.step();
        let step_dur = t_step.elapsed();
        step_count += 1;
        total_step_time += step_dur;
        if step_dur > max_step_time { max_step_time = step_dur; }
        click_all(&harness, "scan \u{2192}"); // advance preflight proceed (the "scan →" button)
        if harness.query_all_by_label_contains("Go (").next().is_some() {
            populated = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
        sleep_count += 1;
    }
    let t_gui = t_gui_start.elapsed();
    let avg_step_ms = if step_count > 0 { total_step_time.as_secs_f64() * 1000.0 / step_count as f64 } else { 0.0 };
    let step_work_frac = if t_gui.as_secs_f64() > 0.0 { total_step_time.as_secs_f64() / t_gui.as_secs_f64() } else { 0.0 };
    let sleep_work_frac = (sleep_count as f64 * 0.010) / t_gui.as_secs_f64().max(1e-9);
    eprintln!(
        "scan-perf-CAL: steps={} total_step_time={:.3}s avg_step={:.2}ms max_step={:.2}ms step_frac={:.3} sleep_frac={:.3}",
        step_count, total_step_time.as_secs_f64(), avg_step_ms, max_step_time.as_secs_f64() * 1000.0, step_work_frac, sleep_work_frac
    );

    if !populated {
        panic!(
            "GUI scan did not populate groups table within {:.1}x t_cli ({:.2}s); see harness diagnostic above",
            timeout_ratio,
            timeout.as_secs_f64()
        );
    }

    // ---------- Step 3: Assert ratio bound (O3 per testdesign 2026-06-04 19:54 PDT) ----------
    // Ratchet on total_step_time (the engine-work-equivalent inside harness.step()) NOT t_gui
    // wall, because harness poll-sleep yields inflate t_gui by 65.6% without measuring engine
    // work. Calibration finding 19:50 PDT: harness yield = ~2.91s out of 4.42s; engine work
    // ~1.3s -> true engine-vs-CLI signal not the wall-derived 50x.
    let ratio_bound: f64 = std::env::var("SUPERDEDUPER_TEST_PERF_RATIO")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(3.0);
    let observed_ratio = total_step_time.as_secs_f64() / t_cli.as_secs_f64();

    eprintln!(
        "scan-perf-ratio: CLI={:.3}s GUI_wall={:.3}s GUI_engine_work={:.3}s ratio={:.2}x bound={:.1}x corpus={}",
        t_cli.as_secs_f64(),
        t_gui.as_secs_f64(),
        total_step_time.as_secs_f64(),
        observed_ratio,
        ratio_bound,
        corpus.display()
    );

    std::fs::remove_dir_all(&corpus).ok();

    if observed_ratio > ratio_bound {
        panic!(
            "GUI engine work {:.3}s > CLI {:.3}s * {:.1}x bound (observed ratio {:.2}x; harness wall was {:.3}s)",
            total_step_time.as_secs_f64(),
            t_cli.as_secs_f64(),
            ratio_bound,
            observed_ratio,
            t_gui.as_secs_f64()
        );
    }
}
