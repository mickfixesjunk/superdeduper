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
    // %TEMP% (= C:\Users\<u>\AppData\Local\Temp). Post finding-#1 (v0.2.36 @ 5d4226b),
    // is_system_path no longer classifies AppData/%TEMP% as system-critical, so %TEMP%
    // isolates the keeper-identity / reference guards cleanly. (The pre-narrow 0b53e1b
    // wrongly system-refused %TEMP% and masked those guards — fixed; no override needed.)
    let mut d = std::env::temp_dir();
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
        let r = run_one_dedupe_action(*action, &alias, &keeper, &[]);
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
        eprintln!("[{variant} / {aname}] GREEN: refused ({}) + keeper survives", r.err().map(|e| e.to_string()).unwrap_or_default());
    }
}

#[test]
fn akp_gui_verbatim_prefix() {
    // \\?\ verbatim alias = canonicalize() (returns the \\?\ form on Windows).
    assert_variant("AKP-2-verbatim", |_dir, keeper| fs::canonicalize(keeper).ok());
}

// AKP-3 8.3: the 8.3 short name IS a valid keeper-alias (resolves + SAME file-id as the
// long name — verified on-box). On c28af25 it refused via the NAMED refused_keeper_identity
// guard; on 5d4226b it refuses via "I/O error os123 (invalid filename)" — canonicalization
// errors on the 8.3 form BEFORE the file-id check. Keeper-SAFE either way, but the named
// guard isn't REACHED for 8.3 on 5d4226b (engine finding: 8.3 canonicalization). This cell
// is INVARIANT-KEYED — assert_variant asserts (is_err + keeper survives), NOT the mechanism —
// so it passes on the keeper-safety invariant regardless of os123-vs-named-guard. The named
// guard IS proven on the other 3 vectors (\\?\ / junction / case) for remove/recycle/safe-rename.
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

// ───────────────────────── v0.2.36 action-layer relocations ─────────────────────────
// System-path guard + reference guard, driven through the SAME GUI dispatch seam.
// These are the real-NTFS / real-system-path legs the headless Linux harness can't reach.

const ALL5: &[(DedupeAction, &str)] = &[
    (DedupeAction::Remove, "Remove"),
    (DedupeAction::Recycle, "Recycle"),
    (DedupeAction::SafeRename, "SafeRename"),
    (DedupeAction::Hardlink, "Hardlink"),
    (DedupeAction::Reflink, "Reflink"),
];

fn payload(p: &Path, bytes: &[u8]) {
    if let Some(d) = p.parent() { let _ = fs::create_dir_all(d); }
    fs::write(p, bytes).unwrap();
}
fn intact(p: &Path, bytes: &[u8]) -> bool {
    p.exists() && fs::read(p).map(|b| b == bytes).unwrap_or(false)
}

/// Drive each action through the GUI seam targeting `target` (the PROTECTED file)
/// with `keeper` the other group member + `references`. Assert: Err (refused),
/// target SURVIVES (the data-loss gate), keeper survives.
fn assert_protected_refused(label: &str, target: &Path, keeper: &Path, references: &[PathBuf], actions: &[(DedupeAction, &str)]) {
    for (action, aname) in actions {
        // re-seed both files fresh each action (some actions mutate on success)
        payload(target, b"PROTECTED-do-not-touch");
        payload(keeper, CONTENT);
        let r = run_one_dedupe_action(*action, target, keeper, references);
        assert!(r.is_err(), "[{label} / {aname}] gate DID NOT refuse protected target {} (err expected)", target.display());
        assert!(intact(target, b"PROTECTED-do-not-touch"), "[{label} / {aname}] PROTECTED TARGET DESTROYED: {}", target.display());
        assert!(intact(keeper, CONTENT), "[{label} / {aname}] keeper damaged: {}", keeper.display());
        eprintln!("[{label} / {aname}] GREEN: refused ({}) + target+keeper survive", r.err().map(|e| e.to_string()).unwrap_or_default());
    }
}

// ── CLASS 1: SYSTEM-PATH guard (the Linux-can't-test authoritative leg) ──
// A target under a system-critical path must be refused on the GUI action path.
#[test]
fn akp_gui_system_path_windows() {
    let dir = PathBuf::from(r"C:\Windows\sdd-akpgui-sys");
    let _ = fs::create_dir_all(&dir);
    let target = dir.join("target.bin");
    let keeper = fresh_dir("sys-keeper").join("keeper.bin"); // keeper OUTSIDE the system path
    assert_protected_refused("SYS-C:\\Windows", &target, &keeper, &[], ACTIONS);
    let _ = fs::remove_dir_all(&dir);
}
#[test]
fn akp_gui_system_path_progfiles() {
    let dir = PathBuf::from(r"C:\Program Files\sdd-akpgui-sys");
    let _ = fs::create_dir_all(&dir);
    let target = dir.join("target.bin");
    let keeper = fresh_dir("sys-keeper2").join("keeper.bin");
    assert_protected_refused("SYS-C:\\Program Files", &target, &keeper, &[], ACTIONS);
    let _ = fs::remove_dir_all(&dir);
}
#[test]
fn akp_gui_system_path_junction_alias() {
    // junction-alias TO a system path: target reached via a non-system junction that
    // points into C:\Windows must STILL be refused (canonicalize, not raw starts_with).
    let sysdir = PathBuf::from(r"C:\Windows\sdd-akpgui-sysj");
    let _ = fs::create_dir_all(&sysdir);
    payload(&sysdir.join("target.bin"), b"PROTECTED-do-not-touch");
    let stage = fresh_dir("sysj");
    let link = stage.join("jx");
    let ok = Command::new("cmd").args(["/c","mklink","/J",&link.display().to_string(),&sysdir.display().to_string()]).status().map(|s| s.success()).unwrap_or(false);
    if !ok { eprintln!("[SYS-junction] SKIP (mklink /J failed)"); let _ = fs::remove_dir_all(&sysdir); return; }
    let target = link.join("target.bin"); // alias path: non-system string, system-classified real path
    let keeper = stage.join("keeper.bin");
    assert_protected_refused("SYS-junction-alias", &target, &keeper, &[], ACTIONS);
    let _ = fs::remove_dir_all(&sysdir);
}

// ── ALLOW-APPDATA (v0.2.36 re-cut; finding-#1 (b) narrowing) ──
// Anti-over-refusal: a dup under %AppData% / %TEMP% must be ACTIONED (Ok, removed),
// NOT refused as system. Proves is_system_path was narrowed to OS-critical only.
// (On the PRE-narrow 0b53e1b this FAILS — %TEMP%/AppData was wrongly system-refused;
// it flips GREEN on the re-cut. The refuse-System32 cells above stay GREEN throughout.)
fn allow_actioned(label: &str, base: PathBuf) {
    let dir = base.join(format!("sdd-akpgui-allow-{}-{}", std::process::id(), COUNTER.fetch_add(1, Ordering::SeqCst)));
    fs::create_dir_all(&dir).unwrap();
    let keeper = dir.join("keeper.bin");
    let target = dir.join("loser.bin"); // DISTINCT file (own inode), byte-identical dup
    payload(&keeper, CONTENT);
    payload(&target, CONTENT);
    let r = run_one_dedupe_action(DedupeAction::Remove, &target, &keeper, &[]);
    assert!(r.is_ok(), "[{label}] dup under user-data must be ACTIONED, not system-refused (narrowing landed?), got: {:?}", r.err().map(|e| e.to_string()));
    assert!(!target.exists(), "[{label}] target should be removed");
    assert!(intact(&keeper, CONTENT), "[{label}] keeper must survive");
    let _ = fs::remove_dir_all(&dir);
    eprintln!("[{label}] GREEN: user-data dup ACTIONED (removed), keeper survives — narrowing confirmed");
}
#[test]
fn akp_gui_allow_appdata_roaming() {
    let base = std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    allow_actioned("ALLOW-%AppData%", base);
}
#[test]
fn akp_gui_allow_temp() {
    allow_actioned("ALLOW-%TEMP%", std::env::temp_dir());
}

// ── CLASS 2: REFERENCE guard — target under a reference root refused on ALL 5 actions ──
// ── A5 NEGATIVE CONTROL: the gate must NOT refuse-everything ──
// A distinct, unprotected dedup loser (not a keeper-alias, not under a reference,
// not under a system path) MUST be actioned OK. Without this, a "refuse everything"
// bug would false-pass every refusal cell above (the F-SAFE-2 trap superdeduper hit).
#[test]
fn akp_gui_negative_control_unprotected_is_actioned() {
    let dir = fresh_dir("negctl");
    let keeper = dir.join("keeper.bin");
    let target = dir.join("loser.bin");   // DISTINCT file (own inode), byte-identical dup
    payload(&keeper, CONTENT);
    payload(&target, CONTENT);
    let r = run_one_dedupe_action(DedupeAction::Remove, &target, &keeper, &[]);
    assert!(r.is_ok(), "[neg-control] unprotected dedup must SUCCEED (gate is refuse-everything?), got: {:?}", r.err().map(|e| e.to_string()));
    assert!(!target.exists(), "[neg-control] unprotected target should be removed");
    assert!(intact(&keeper, CONTENT), "[neg-control] keeper must survive");
    eprintln!("[neg-control / Remove] GREEN: unprotected target ACTIONED (removed), keeper survives — gate is not refuse-everything");
}

#[test]
fn akp_gui_reference_clean() {
    let refroot = fresh_dir("ref").join("refroot");
    fs::create_dir_all(&refroot).unwrap();
    let target = refroot.join("refdup.bin");       // UNDER the reference root
    let keeper = refroot.parent().unwrap().join("keeper.bin"); // outside the reference
    let refs = vec![refroot.clone()];
    assert_protected_refused("REF-clean", &target, &keeper, &refs, ALL5);
}
#[test]
fn akp_gui_reference_verbatim_alias() {
    // load-bearing: a \\?\ alias to a file under the reference root must STILL be refused
    // (old GUI raw starts_with missed the verbatim prefix).
    let refroot = fresh_dir("refa").join("refroot");
    fs::create_dir_all(&refroot).unwrap();
    let real = refroot.join("refdup.bin");
    payload(&real, b"PROTECTED-do-not-touch");
    let alias = fs::canonicalize(&real).unwrap(); // \\?\ form
    let keeper = refroot.parent().unwrap().join("keeper.bin");
    let refs = vec![refroot.clone()];
    assert_protected_refused("REF-\\\\?\\-alias", &alias, &keeper, &refs, ALL5);
}
