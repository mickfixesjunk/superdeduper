// AKP GUI-path NTFS keeper-identity cells (#153 Tier C, sdd-testwin).
//
// Drives the REAL GUI per-file dispatch seam `gui::app::run_one_dedupe_action`
// (the exact path the GUI "Go" button flows through) with NTFS keeper-ALIASES as
// the action target. The action-layer keeper-identity gate must REFUSE (Err) when
// the target resolves to the same file-id as the keeper, and the keeper must SURVIVE.
//
// Only file-id survives the string gate — these aliases (\\?\ verbatim, 8.3 short
// name, junction, case-variant) all resolve to the keeper's FILE_ID_INFO, so the
// gate refuses each. Run: cargo test --features gui --test akp_gui_ntfs -- --nocapture
#![cfg(all(windows, feature = "gui"))]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

use superdeduper::cli::DedupeAction;
use superdeduper::gui::app::run_one_dedupe_action;

static COUNTER: AtomicU32 = AtomicU32::new(0);
const CONTENT: &[u8] = b"keeper-payload-do-not-destroy";

fn fresh_dir(tag: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let mut d = std::env::temp_dir(); // C:\...\Temp = real NTFS
    d.push(format!("sdd-akpgui-{}-{}-{}", tag, std::process::id(), n));
    fs::create_dir_all(&d).unwrap();
    d
}

fn make_keeper(dir: &Path) -> PathBuf {
    let k = dir.join("keeper.bin");
    fs::write(&k, CONTENT).unwrap();
    k
}

fn keeper_intact(keeper: &Path) -> bool {
    keeper.exists() && fs::read(keeper).map(|b| b == CONTENT).unwrap_or(false)
}

/// 8.3 short name via `cmd /c for %I in ("path") do @echo %~sI`. None if 8.3 is off.
fn short_name(p: &Path) -> Option<PathBuf> {
    let out = Command::new("cmd")
        .args(["/c", &format!("for %I in (\"{}\") do @echo %~sI", p.display())])
        .output()
        .ok()?;
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() || !s.contains('~') {
        return None;
    }
    let pb = PathBuf::from(s);
    if pb == p { None } else { Some(pb) }
}

const ACTIONS: &[(DedupeAction, &str)] = &[
    (DedupeAction::Remove, "Remove"),
    (DedupeAction::Recycle, "Recycle"),
    (DedupeAction::SafeRename, "SafeRename"),
    (DedupeAction::Hardlink, "Hardlink"),
];

/// For one alias variant: build a fresh keeper + alias per action, drive the GUI
/// seam, assert Err + keeper survives.
fn assert_variant<F>(variant: &str, build_alias: F)
where
    F: Fn(&Path, &Path) -> Option<PathBuf>, // (dir, keeper) -> alias, or None to SKIP
{
    for (action, aname) in ACTIONS {
        let dir = fresh_dir(variant);
        let keeper = make_keeper(&dir);
        let alias = match build_alias(&dir, &keeper) {
            Some(a) => a,
            None => {
                eprintln!("[{variant} / {aname}] SKIP (alias unconstructable on this box)");
                continue;
            }
        };
        let r = run_one_dedupe_action(*action, &alias, &keeper);
        assert!(
            r.is_err(),
            "[{variant} / {aname}] gate DID NOT refuse keeper-alias (target={}, keeper={})",
            alias.display(),
            keeper.display()
        );
        assert!(
            keeper_intact(&keeper),
            "[{variant} / {aname}] KEEPER DESTROYED via alias (target={})",
            alias.display()
        );
        eprintln!("[{variant} / {aname}] GREEN: refused + keeper survives");
    }
}

#[test]
fn akp_gui_verbatim_prefix() {
    // \\?\ verbatim alias = canonicalize() (returns the \\?\ form on Windows).
    assert_variant("AKP-2-verbatim", |_dir, keeper| fs::canonicalize(keeper).ok());
}

#[test]
fn akp_gui_short_name_8dot3() {
    assert_variant("AKP-3-8.3", |_dir, keeper| short_name(keeper));
}

#[test]
fn akp_gui_junction() {
    // junction L -> dir; alias = L\keeper.bin (same file via the junction).
    assert_variant("AKP-4-junction", |dir, keeper| {
        let link = dir.join("jx");
        let st = Command::new("cmd")
            .args(["/c", "mklink", "/J", &link.display().to_string(), &dir.display().to_string()])
            .status()
            .ok()?;
        if !st.success() {
            return None;
        }
        Some(link.join(keeper.file_name().unwrap()))
    });
}

#[test]
fn akp_gui_case_variant() {
    // case-insensitive collision: flip the filename case in the same dir.
    assert_variant("AKP-6-case", |dir, _keeper| Some(dir.join("KEEPER.BIN")));
}
