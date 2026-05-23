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
        roots: Vec<PathBuf>,
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
#[derive(Debug, Clone)]
pub struct Grade {
    pub letter: char,
    pub overall_percent: u8,
    pub hardware: AxisScore,
    pub disk: DiskAxis,
    pub hash: AxisScore,
}

#[derive(Debug, Clone, Copy)]
pub struct AxisScore {
    pub percent: u8,
}

/// Per-drive disk scores + the composite (arithmetic mean of the
/// measured drives). Drives without a measurement are still surfaced
/// to the user but excluded from the composite.
#[derive(Debug, Clone)]
pub struct DiskAxis {
    /// Average of the measured drives' percents. `None` ⇒ no drive
    /// was measurable (all read-only / skip-io).
    pub composite_percent: Option<u8>,
    pub drives: Vec<DriveScore>,
}

#[derive(Debug, Clone)]
pub struct DriveScore {
    pub identifier: String,
    /// MB/s aggregate for this drive's tier3 probe. `None` when the
    /// drive could not be measured.
    pub tier3_mbps: Option<f64>,
    pub percent: Option<u8>,
    pub error: Option<String>,
}

const HARDWARE_REF_MBPS: f64 = 20_000.0;
const DISK_REF_MBPS: f64 = 3_000.0;
const HASH_REF_MBPS: f64 = 50_000.0;

pub fn spawn_probe(roots: Vec<PathBuf>) -> PreflightState {
    let (tx, rx) = std::sync::mpsc::channel();
    let probe_roots = roots.clone();
    std::thread::spawn(move || {
        let result = diagnose::run_probes(probe_roots, false);
        let _ = tx.send(result);
    });
    PreflightState::Probing {
        started_at: Instant::now(),
        roots,
        rx,
    }
}

pub fn grade_report(r: &DiagnoseReport) -> Grade {
    let hardware_pct = pct(
        r.hash
            .river5_single_thread_mbps
            .max(r.hash.blake3_single_thread_mbps),
        HARDWARE_REF_MBPS,
    );
    let hash_pct = pct(
        r.hash.river5_aggregate_mbps.max(r.hash.blake3_aggregate_mbps),
        HASH_REF_MBPS,
    );

    let drives: Vec<DriveScore> = r
        .drives
        .iter()
        .map(|d| {
            let (tier3_mbps, percent) = match &d.tier3 {
                Some(t) => (Some(t.aggregate_mbps), Some(pct(t.aggregate_mbps, DISK_REF_MBPS))),
                None => (None, None),
            };
            DriveScore {
                identifier: d.identifier.clone(),
                tier3_mbps,
                percent,
                error: d.error.clone(),
            }
        })
        .collect();

    let measured: Vec<u8> = drives.iter().filter_map(|d| d.percent).collect();
    let composite_percent = if measured.is_empty() {
        None
    } else {
        Some((measured.iter().map(|&p| p as u32).sum::<u32>() / measured.len() as u32) as u8)
    };

    let overall = match composite_percent {
        Some(disk) => (hardware_pct as u32 + disk as u32 + hash_pct as u32) / 3,
        None => (hardware_pct as u32 + hash_pct as u32) / 2,
    };

    Grade {
        letter: letter_for(overall as u8),
        overall_percent: overall as u8,
        hardware: AxisScore { percent: hardware_pct },
        disk: DiskAxis {
            composite_percent,
            drives,
        },
        hash: AxisScore { percent: hash_pct },
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
