//! Pre-flight modal — state + grading logic.
//!
//! When the user clicks Scan, the GUI kicks off `diagnose::run_probes`
//! on a background thread, shows a modal with the result, and only
//! starts the scan once the user confirms. Per docs/preflight-spec.md.
//!
//! Slice 1: no cache, no action buttons, no telemetry checkbox. The
//! goal here is the visible UX (credit-report-style score card) so we
//! can iterate on the aesthetic before adding the cache + actions
//! infrastructure.

use std::path::PathBuf;
use std::sync::mpsc::Receiver;
use std::time::Instant;

use crate::diagnose::{self, DiagnoseReport};

pub enum PreflightState {
    Idle,
    Probing {
        started_at: Instant,
        root: PathBuf,
        rx: Receiver<anyhow::Result<DiagnoseReport>>,
    },
    Showing {
        report: Box<DiagnoseReport>,
        grade: Grade,
    },
    Failed(String),
}

impl Default for PreflightState {
    fn default() -> Self {
        Self::Idle
    }
}

impl PreflightState {
    pub fn is_active(&self) -> bool {
        !matches!(self, Self::Idle)
    }
}

/// Composite grade + three per-axis subscores. Each axis is a
/// percentage of a "saturated for sd's purposes" reference point —
/// 100% means the machine is fast enough that the axis won't be a
/// gating factor on typical workloads.
#[derive(Debug, Clone, Copy)]
pub struct Grade {
    pub letter: char,
    pub overall_percent: u8,
    pub hardware: AxisScore,
    pub disk: AxisScore,
    pub hash: AxisScore,
}

#[derive(Debug, Clone, Copy)]
pub struct AxisScore {
    pub percent: u8,
    /// Human-readable raw number for the score-card line, e.g.
    /// `"45,000 MB/s single-stream"`.
    pub raw: &'static str,
}

// Per-axis reference points. A machine at or above these has the axis
// effectively saturated for sd's purposes. Numbers chosen against
// 2026-era hardware — adjust as the ecosystem shifts.
const HARDWARE_REF_MBPS: f64 = 20_000.0;
const DISK_REF_MBPS: f64 = 3_000.0;
const HASH_REF_MBPS: f64 = 50_000.0;

pub fn spawn_probe(root: PathBuf) -> PreflightState {
    let (tx, rx) = std::sync::mpsc::channel();
    let probe_root = root.clone();
    std::thread::spawn(move || {
        let result = diagnose::run_probes(probe_root, false);
        let _ = tx.send(result);
    });
    PreflightState::Probing {
        started_at: Instant::now(),
        root,
        rx,
    }
}

pub fn grade_report(r: &DiagnoseReport) -> Grade {
    let hardware_pct =
        pct(r.hash.river5_single_thread_mbps.max(r.hash.blake3_single_thread_mbps), HARDWARE_REF_MBPS);
    let disk_pct = r
        .tier3
        .as_ref()
        .map(|t| pct(t.aggregate_mbps, DISK_REF_MBPS))
        .unwrap_or(50); // tier3 skipped — show as middle-of-the-road
    let hash_pct = pct(
        r.hash.river5_aggregate_mbps.max(r.hash.blake3_aggregate_mbps),
        HASH_REF_MBPS,
    );

    let overall = (hardware_pct as u32 + disk_pct as u32 + hash_pct as u32) / 3;

    Grade {
        letter: letter_for(overall as u8),
        overall_percent: overall as u8,
        hardware: AxisScore {
            percent: hardware_pct,
            // SAFETY: these are static slogans, not actually populated
            // from runtime data. The render function will format the
            // real numbers from the underlying report at draw time —
            // we only use `raw` for the static label suffix.
            raw: "single-stream",
        },
        disk: AxisScore {
            percent: disk_pct,
            raw: "Tier 3 sequential read",
        },
        hash: AxisScore {
            percent: hash_pct,
            raw: "aggregate",
        },
    }
}

fn pct(value: f64, reference: f64) -> u8 {
    if reference <= 0.0 {
        return 0;
    }
    ((value / reference) * 100.0).clamp(0.0, 100.0) as u8
}

fn letter_for(percent: u8) -> char {
    match percent {
        90..=100 => 'A',
        75..=89 => 'B',
        60..=74 => 'C',
        45..=59 => 'D',
        _ => 'F',
    }
}

/// User choice from the modal. `Start` proceeds to the original
/// `start_live` body; `Cancel` aborts and leaves state Idle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreflightAction {
    Start,
    Cancel,
}
