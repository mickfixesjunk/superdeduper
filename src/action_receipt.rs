//! Structured action-receipt emission for `--integration-test-mode`.
//!
//! When the flag is set, every destructive action attempted by
//! [`crate::dedupe::process_group`] emits one NDJSON line carrying
//! everything an integration-test harness needs to assert containment:
//! source / keeper paths, inode IDs before + after, hardlink-count
//! delta, recycle bin metadata, outcome enum, and any error message.
//!
//! Schema `superdeduper.action_receipt.v1` per testdesign's
//! `gui-destructible-action-integration-spec.md` §5.1. Carried
//! forward unchanged into `behavior-containment-integration-spec.md`
//! v2; the v2 receipt-extension fields (snapshot paths +
//! containment-assert outcome) are recorded by the HARNESS, not the
//! engine — engine just emits the per-action receipt.
//!
//! Output goes to stdout unless the caller passed `--receipt-file
//! <path>`, in which case [`ReceiptWriter`] opens that file in
//! truncate-on-first-write mode + appends thereafter. Caller is
//! responsible for constructing the writer once per run + flushing
//! at end (Drop handles the flush automatically).

use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Sink for action receipts. Either stdout (the default when
/// `--integration-test-mode` is on) or a file (when
/// `--receipt-file <path>` is also passed).
///
/// Stateful: opens the file lazily on first write so a run that
/// produces zero receipts doesn't leave an empty file behind.
pub enum ReceiptWriter {
    Stdout,
    File {
        path: PathBuf,
        handle: Option<BufWriter<File>>,
    },
    /// Silent — `--integration-test-mode` not set; emit calls
    /// short-circuit to no-op. Lets callers always invoke `emit`
    /// without checking the flag first.
    Disabled,
}

impl ReceiptWriter {
    /// Build from CLI flag inputs. If `integration_test_mode == false`,
    /// returns the Disabled sink (emits are no-ops). If true with
    /// `receipt_file = Some(path)`, returns a File sink. If true
    /// with `receipt_file = None`, returns a Stdout sink.
    pub fn from_flags(integration_test_mode: bool, receipt_file: Option<&Path>) -> Self {
        match (integration_test_mode, receipt_file) {
            (false, _) => ReceiptWriter::Disabled,
            (true, None) => ReceiptWriter::Stdout,
            (true, Some(p)) => ReceiptWriter::File {
                path: p.to_path_buf(),
                handle: None,
            },
        }
    }

    /// Emit one receipt as a single NDJSON line. Silent no-op when
    /// the writer is Disabled. Failure to write surfaces as an
    /// error so callers can choose between hard-fail vs log-and-
    /// continue — the dedupe path opts for log-and-continue so an
    /// unwritable receipt file doesn't kill a long-running dedupe.
    pub fn emit(&mut self, receipt: &ActionReceipt) -> io::Result<()> {
        if matches!(self, ReceiptWriter::Disabled) {
            return Ok(());
        }
        let line = serde_json::to_string(receipt).map_err(io::Error::other)?;
        match self {
            ReceiptWriter::Disabled => unreachable!(),
            ReceiptWriter::Stdout => {
                let stdout = io::stdout();
                let mut handle = stdout.lock();
                handle.write_all(line.as_bytes())?;
                handle.write_all(b"\n")?;
                handle.flush()?;
            }
            ReceiptWriter::File { path, handle } => {
                if handle.is_none() {
                    // Truncate on first write; subsequent writes
                    // append within the same opened handle.
                    let f = OpenOptions::new()
                        .create(true)
                        .write(true)
                        .truncate(true)
                        .open(path)?;
                    *handle = Some(BufWriter::new(f));
                }
                if let Some(h) = handle.as_mut() {
                    h.write_all(line.as_bytes())?;
                    h.write_all(b"\n")?;
                }
            }
        }
        Ok(())
    }
}

impl Drop for ReceiptWriter {
    fn drop(&mut self) {
        if let ReceiptWriter::File {
            handle: Some(h), ..
        } = self
        {
            let _ = h.flush();
        }
    }
}

/// One action receipt. Field set + naming locks to schema
/// `superdeduper.action_receipt.v1`. Optional fields use
/// `#[serde(skip_serializing_if = "Option::is_none")]` so empty
/// receipts stay terse; required fields are always present.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionReceipt {
    /// Schema version. Always `"superdeduper.action_receipt.v1"`.
    pub schema: String,
    /// ISO 8601 UTC timestamp, millisecond precision.
    pub timestamp: String,
    /// Action kind: `"delete-to-recycle" | "delete-permanently" |
    /// "hardlink-replace" | "reflink-replace" | "safe-rename" |
    /// "dry-run" | "left-alone"`.
    pub action: String,
    /// File the action targeted.
    pub source_path: String,
    /// Keeper that survived the dedup (canonical "winner" of the
    /// group). For `safe-rename`, the keeper is the same as the
    /// source's untouched alias.
    pub keeper_path: String,
    /// Outcome enum per spec §5.3.
    pub outcome: String,
    /// Hex-string inode-or-file-ref BEFORE the action. `None` for
    /// dry-run / left-alone.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inode_before: Option<String>,
    /// Hex-string inode-or-file-ref AFTER the action. `None` when
    /// the file no longer exists (deleted) OR for actions that
    /// don't move data (dry-run / left-alone). For hardlink-replace,
    /// `inode_after` equals the keeper's inode (the source now
    /// shares the keeper's underlying inode).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inode_after: Option<String>,
    /// Source file size in bytes.
    pub size_bytes: u64,
    /// Recycle Bin entry details, populated only on
    /// `delete-to-recycle` Windows success. `None` on Linux/macOS
    /// or non-recycle actions or failure.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recycle_bin_entry: Option<RecycleBinEntry>,
    /// Hardlink-count delta on the inode underlying `source_path`:
    /// negative when a link was freed (delete-to-recycle of a hardlinked
    /// file frees its link; permanent-delete of the last link drops to
    /// 0); positive on hardlink-replace (the source now points at the
    /// keeper's inode, so the keeper's nlink incremented); 0 for no-op
    /// (dry-run, already_hardlinked, etc.).
    pub hardlink_count_delta: i64,
    /// Free-text error message when `outcome == "error"`. `None`
    /// otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl ActionReceipt {
    pub const SCHEMA: &'static str = "superdeduper.action_receipt.v1";

    /// Helper: build a fresh receipt with the required fields set.
    /// Optional fields default to None / 0 and can be set by the
    /// caller before emitting.
    pub fn new(action: &str, source_path: &str, keeper_path: &str, size_bytes: u64) -> Self {
        Self {
            schema: Self::SCHEMA.to_string(),
            timestamp: now_iso8601_ms(),
            action: action.to_string(),
            source_path: source_path.to_string(),
            keeper_path: keeper_path.to_string(),
            outcome: "ok".to_string(),
            inode_before: None,
            inode_after: None,
            size_bytes,
            recycle_bin_entry: None,
            hardlink_count_delta: 0,
            error: None,
        }
    }

    /// Convenience: produce a receipt for a dry-run no-op.
    pub fn dry_run(source_path: &str, keeper_path: &str, size_bytes: u64) -> Self {
        let mut r = Self::new("dry-run", source_path, keeper_path, size_bytes);
        r.outcome = "dry_run_no_change".to_string();
        r
    }

    /// Convenience: produce a receipt for an action that errored.
    pub fn error_for(
        action: &str,
        source_path: &str,
        keeper_path: &str,
        size_bytes: u64,
        err: &str,
    ) -> Self {
        let mut r = Self::new(action, source_path, keeper_path, size_bytes);
        r.outcome = "error".to_string();
        r.error = Some(err.to_string());
        r
    }
}

/// Recycle Bin entry metadata per spec §5.1. Windows-only fields;
/// engine populates these from the IFileOperation result on
/// successful `delete-to-recycle`. On other platforms the entire
/// `recycle_bin_entry` field stays `None`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecycleBinEntry {
    /// Path to the Recycle Bin container that received the file
    /// (e.g. `C:\$Recycle.Bin\S-1-5-21-...`).
    pub container: String,
    /// `$I*` file name (Recycle Bin metadata file).
    pub index_file: String,
    /// `$R*` file name (the renamed data file).
    pub data_file: String,
    /// Original path of the file before recycling. Equal to
    /// `source_path` on the receipt; carried here for harness
    /// convenience (avoids cross-referencing fields).
    pub original_path: String,
}

/// Map `DedupeAction` enum variant to the receipt schema's action
/// string. Stable; harness parses these literally.
pub fn action_label(action: crate::cli::DedupeAction) -> &'static str {
    use crate::cli::DedupeAction;
    match action {
        DedupeAction::Recycle => "delete-to-recycle",
        DedupeAction::Remove => "delete-permanently",
        DedupeAction::Hardlink => "hardlink-replace",
        DedupeAction::Reflink => "reflink-replace",
        DedupeAction::SafeRename => "safe-rename",
    }
}

/// Stat-and-extract: read a file's metadata and return
/// `(inode_hex, nlink)`. `None` when the file can't be stat'd
/// (e.g. already deleted). Cross-platform; on Windows we use the
/// file_ref via `winapi_wrappers` if available, else fall back to
/// `MetadataExt::file_index` on supported Windows builds; on Unix
/// we use `MetadataExt::ino`.
pub fn read_inode_and_nlink(path: &Path) -> Option<(String, u64)> {
    let meta = std::fs::metadata(path).ok()?;
    let (ino, nlink) = inode_nlink_from_metadata(&meta);
    let ino_hex = format!("0x{ino:016x}");
    Some((ino_hex, nlink))
}

#[cfg(unix)]
fn inode_nlink_from_metadata(meta: &std::fs::Metadata) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt;
    (meta.ino(), meta.nlink())
}

#[cfg(windows)]
fn inode_nlink_from_metadata(_meta: &std::fs::Metadata) -> (u64, u64) {
    // `std::os::windows::fs::MetadataExt::file_index()` is gated
    // behind the unstable `windows_by_handle` feature on stable
    // Rust through 1.87 — cross-builds break with E0658. The
    // honest fallback for action-receipt purposes is `(0, 1)`:
    // harness callers that need real inode IDs should invoke
    // `superdeduper debug snapshot`, which uses CreateFileW +
    // GetFileInformationByHandle via the windows crate (see
    // `src/debug/snapshot.rs::win_file_info`). Receipts get inode
    // for free on Unix; on Windows they degrade to a structural
    // placeholder until we plumb the same windows-crate call here.
    (0, 1)
}

#[cfg(not(any(unix, windows)))]
fn inode_nlink_from_metadata(_meta: &std::fs::Metadata) -> (u64, u64) {
    (0, 1)
}

fn now_iso8601_ms() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs();
    let ms = now.subsec_millis();
    let (year, month, day, hour, min, sec) = epoch_to_ymd_hms(secs);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        year, month, day, hour, min, sec, ms
    )
}

/// Local epoch-to-Y/M/D-H/M/S converter so action_receipt has zero
/// external date deps. Civil-from-days algorithm by Howard Hinnant.
fn epoch_to_ymd_hms(secs: u64) -> (i32, u32, u32, u32, u32, u32) {
    let z = secs as i64 / 86_400 + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i32 + era as i32 * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp.wrapping_sub(9) };
    let year = if m <= 2 { y + 1 } else { y };
    let sec_of_day = (secs % 86_400) as u32;
    let hour = sec_of_day / 3_600;
    let min = (sec_of_day % 3_600) / 60;
    let sec = sec_of_day % 60;
    (year, m, d, hour, min, sec)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_string_is_v1() {
        let r = ActionReceipt::new("delete-to-recycle", "a", "b", 1024);
        assert_eq!(r.schema, "superdeduper.action_receipt.v1");
    }

    #[test]
    fn dry_run_receipt_has_correct_outcome() {
        let r = ActionReceipt::dry_run("a", "b", 1024);
        assert_eq!(r.outcome, "dry_run_no_change");
        assert_eq!(r.action, "dry-run");
        assert!(r.inode_before.is_none());
        assert!(r.inode_after.is_none());
    }

    #[test]
    fn error_receipt_carries_message() {
        let r = ActionReceipt::error_for("delete-to-recycle", "a", "b", 1024, "permission denied");
        assert_eq!(r.outcome, "error");
        assert_eq!(r.error.as_deref(), Some("permission denied"));
    }

    #[test]
    fn action_label_maps_enum_variants() {
        use crate::cli::DedupeAction;
        assert_eq!(action_label(DedupeAction::Recycle), "delete-to-recycle");
        assert_eq!(action_label(DedupeAction::Remove), "delete-permanently");
        assert_eq!(action_label(DedupeAction::Hardlink), "hardlink-replace");
        assert_eq!(action_label(DedupeAction::Reflink), "reflink-replace");
        assert_eq!(action_label(DedupeAction::SafeRename), "safe-rename");
    }

    #[test]
    fn receipt_serialises_as_single_line_json() {
        let r = ActionReceipt::new("delete-to-recycle", "/a/b.txt", "/a/c.txt", 2048);
        let line = serde_json::to_string(&r).unwrap();
        assert!(
            !line.contains('\n'),
            "receipt must serialise without newlines"
        );
        assert!(line.contains("\"schema\":\"superdeduper.action_receipt.v1\""));
        assert!(line.contains("\"action\":\"delete-to-recycle\""));
        assert!(line.contains("\"source_path\":\"/a/b.txt\""));
        assert!(line.contains("\"keeper_path\":\"/a/c.txt\""));
        assert!(line.contains("\"size_bytes\":2048"));
        // Optional fields with None values must not appear in JSON.
        assert!(!line.contains("inode_before"));
        assert!(!line.contains("inode_after"));
        assert!(!line.contains("recycle_bin_entry"));
        assert!(!line.contains("\"error\""));
    }

    #[test]
    fn receipt_includes_optional_fields_when_set() {
        let mut r = ActionReceipt::new("hardlink-replace", "/a/b.txt", "/a/c.txt", 1024);
        r.inode_before = Some("0x0000000000000abc".to_string());
        r.inode_after = Some("0x0000000000000def".to_string());
        r.hardlink_count_delta = 1;
        let line = serde_json::to_string(&r).unwrap();
        assert!(line.contains("\"inode_before\":\"0x0000000000000abc\""));
        assert!(line.contains("\"inode_after\":\"0x0000000000000def\""));
        assert!(line.contains("\"hardlink_count_delta\":1"));
    }

    #[test]
    fn disabled_writer_swallows_emit() {
        let mut w = ReceiptWriter::Disabled;
        let r = ActionReceipt::new("delete-to-recycle", "a", "b", 1);
        // Should succeed silently. No file, no stdout pollution
        // (tests can't observe stdout cleanly, but emit returns Ok).
        assert!(w.emit(&r).is_ok());
    }

    #[test]
    fn file_writer_writes_one_line_per_emit() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
        {
            let mut w = ReceiptWriter::from_flags(true, Some(&path));
            let r1 = ActionReceipt::new("delete-to-recycle", "a", "b", 1);
            let r2 = ActionReceipt::dry_run("c", "d", 2);
            w.emit(&r1).unwrap();
            w.emit(&r2).unwrap();
        } // writer drops here, file gets flushed
        let body = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 2, "two emits → two lines");
        for line in &lines {
            // Each line must parse back as a receipt.
            let _round_trip: ActionReceipt =
                serde_json::from_str(line).expect("each line parses as a receipt");
        }
    }

    #[test]
    fn iso8601_format_is_correct_shape() {
        // Known epoch second: 2020-01-01T00:00:00Z = 1577836800.
        let (y, m, d, h, mi, s) = epoch_to_ymd_hms(1_577_836_800);
        assert_eq!((y, m, d, h, mi, s), (2020, 1, 1, 0, 0, 0));
        // 2000-02-29T12:34:56Z = 951827696 (leap-year edge case).
        let (y, m, d, h, mi, s) = epoch_to_ymd_hms(951_827_696);
        assert_eq!((y, m, d, h, mi, s), (2000, 2, 29, 12, 34, 56));
        // 1970-01-01T00:00:00Z = 0 (epoch itself).
        let (y, m, d, h, mi, s) = epoch_to_ymd_hms(0);
        assert_eq!((y, m, d, h, mi, s), (1970, 1, 1, 0, 0, 0));
    }

    #[test]
    fn from_flags_returns_disabled_when_mode_off() {
        assert!(matches!(
            ReceiptWriter::from_flags(false, None),
            ReceiptWriter::Disabled
        ));
        // receipt_file is ignored when mode is off.
        assert!(matches!(
            ReceiptWriter::from_flags(false, Some(Path::new("/tmp/x"))),
            ReceiptWriter::Disabled
        ));
    }

    #[test]
    fn from_flags_returns_stdout_when_no_file() {
        assert!(matches!(
            ReceiptWriter::from_flags(true, None),
            ReceiptWriter::Stdout
        ));
    }

    #[test]
    fn from_flags_returns_file_when_path_set() {
        let w = ReceiptWriter::from_flags(true, Some(Path::new("/tmp/x.ndjson")));
        assert!(matches!(w, ReceiptWriter::File { .. }));
    }
}
