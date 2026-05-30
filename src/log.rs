//! Persistent engine log — Mick directive 2026-05-29: "PERSIST engine logs to
//! disk ALWAYS — observability gap that bites every retest." Phase 1: a thin
//! `log_info!` / `log_warn!` / `log_err!` macro set that fans every call to
//! BOTH stderr (the existing eprintln! channel; users running the CLI from a
//! terminal still see it) AND a file at
//! `<data_dir>/log/superdeduper.<unix_secs>.<pid>.log`. Keeps the last 10 log
//! files (older are pruned on first write of a new file). No external crates;
//! lazy-open the file on the first call so a process that never logs costs
//! nothing.
//!
//! Phase 2 (separate batch) will migrate the existing eprintln! call sites
//! across leaderboard / dedupe / gui / live to these macros so EVERY log line
//! lands on disk by default. Tonight's slice (#118 v0.2.39) uses the new
//! macros in the action_submission + pending_actions paths so the silent
//! "skipped — no pending submission_id" diagnostic gap that motivated the
//! directive becomes a one-grep affair.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::sync::OnceLock;

use parking_lot::Mutex;

/// Lazily-opened log file handle. `Some(f)` after first successful open;
/// `None` if the log dir / file couldn't be created (we still flush to
/// stderr — fail-soft, never break the caller because logging failed).
fn slot() -> &'static Mutex<Option<std::fs::File>> {
    static SLOT: OnceLock<Mutex<Option<std::fs::File>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

/// Whether we've attempted to open the file yet (so we only try once and
/// don't pound on a failing fs every line).
fn open_attempted() -> &'static Mutex<bool> {
    static SLOT: OnceLock<Mutex<bool>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(false))
}

fn open_log_file() -> Option<std::fs::File> {
    // Build path: <data_dir>/log/superdeduper.<unix_secs>.<pid>.log
    let data = crate::leaderboard::install::data_dir_public().ok()?;
    let log_dir = data.join("log");
    std::fs::create_dir_all(&log_dir).ok()?;

    // Rotate: keep the most-recent 9 existing files; the file we're about to
    // create will be the 10th. We do this BEFORE creating the new file so
    // listing doesn't include it.
    if let Ok(entries) = std::fs::read_dir(&log_dir) {
        let mut files: Vec<PathBuf> = entries
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| n.starts_with("superdeduper.") && n.ends_with(".log"))
                    .unwrap_or(false)
            })
            .collect();
        files.sort(); // unix-ts-prefixed filenames sort chronologically
        while files.len() > 9 {
            let _ = std::fs::remove_file(files.remove(0));
        }
    }

    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    let pid = std::process::id();
    let path = log_dir.join(format!("superdeduper.{ts:010}.{pid}.log"));
    OpenOptions::new().create(true).append(true).open(&path).ok()
}

/// Write one log line. Fan-out: ALWAYS stderr (matches existing eprintln!
/// behavior so terminal output is unchanged), then disk if the file can be
/// opened. Lazy-opens the file on the first call.
pub fn write_line(level: &str, args: std::fmt::Arguments) {
    // 1. stderr fanout. Preserves current observable behavior for anyone
    //    capturing stderr (PowerShell Tee-Object, 2> redirect, etc.).
    eprintln!("[{level}] {args}");

    // 2. Lazy open the disk log on first call.
    {
        let mut attempted = open_attempted().lock();
        if !*attempted {
            *attempted = true;
            *slot().lock() = open_log_file();
        }
    }

    // 3. Append to disk if open. Fail-soft on any IO error (we already wrote
    //    to stderr; losing the disk copy is a degraded mode, not a fatal one).
    let mut guard = slot().lock();
    if let Some(f) = guard.as_mut() {
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        // ISO-ish: `[unix_secs] [level] message`. Cheap, sortable,
        // tooling-friendly. Phase 2 may switch to RFC3339 via chrono.
        let _ = writeln!(f, "{secs} [{level}] {args}");
        let _ = f.flush();
    }
}

/// `log_info!("foo {}", bar)` — fan an info-level line to stderr + disk.
/// Use for routine engine state-transition messages (request submitted,
/// PATCH succeeded, queue drained, etc.).
#[macro_export]
macro_rules! log_info {
    ($($arg:tt)*) => ($crate::log::write_line("INFO", format_args!($($arg)*)));
}

/// `log_warn!("foo {}", bar)` — fan a warn-level line. Use for recoverable
/// degradations: PATCH transient failure, queue grew past hint, etc.
#[macro_export]
macro_rules! log_warn {
    ($($arg:tt)*) => ($crate::log::write_line("WARN", format_args!($($arg)*)));
}

/// `log_err!("foo {}", bar)` — fan an error-level line. Use for engine-bug
/// signals: install state malformed, schema rejected, write failed.
#[macro_export]
macro_rules! log_err {
    ($($arg:tt)*) => ($crate::log::write_line("ERR ", format_args!($($arg)*)));
}

#[cfg(test)]
mod tests {
    use super::*;

    // Smoke-tests: the macros expand + write_line doesn't panic in the
    // common path. We can't easily assert the disk file appears here without
    // overriding data_dir, but exercising the lazy-open path catches any
    // expansion bugs.

    #[test]
    fn write_line_does_not_panic_on_routine_call() {
        write_line("INFO", format_args!("test event id={}", 42));
    }

    #[test]
    fn macros_expand_with_format_args() {
        crate::log_info!("info msg {}", 1);
        crate::log_warn!("warn msg {}", 2);
        crate::log_err!("err msg {}", 3);
    }
}
