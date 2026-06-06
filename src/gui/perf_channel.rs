//! Scan-worker -> GUI channel instrumentation (A-perf-channel-h2,
//! testdesign ASK 3 2026-06-05 23:24 PDT). SUPERDEDUPER_SKIP_ACCESSKIT
//! re-fire showed wall UNCHANGED despite update() CPU dropping 224x:
//! H3 (accesskit per-frame) is REAL CPU but NOT wall-load-bearing.
//! The 30+s of cold-GUI wall NOT eliminated by SKIP_ACCESSKIT is in
//! H2 (channel back-pressure) OR adjacent (winit msg-pump idle /
//! accesskit_windows IPC blocking / scan-worker thread contention /
//! structural startup tail). This module instruments the producer
//! (scan-worker -> [`PerfTx`]) and consumer ([`drain_events`] in
//! [`super::app`]) sides of the bounded crossbeam channel so the
//! decomposition disambiguates: producer back-pressure vs consumer
//! blocked on something other than channel vs structural frame-pace.
//!
//! Activation: emissions gated by SUPERDEDUPER_PERF_INSTRUMENT_UPDATE=1
//! (reuses existing env var; no new flag). One perf-channel line per
//! update() frame while is_scanning, routed through crate::log_info!.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use crossbeam_channel::{SendError, Sender, TrySendError};

use crate::gui::events::EngineEvent;

/// 0.5ms threshold for declaring a send "contended". send() on a
/// bounded crossbeam channel that isn't full returns in <10us; >0.5ms
/// implies the send was queued because the channel was full (producer
/// back-pressure). Kept cheap so per-send cost stays negligible.
const CONTENDED_THRESHOLD_NS: u64 = 500_000;

static TX_COUNT: AtomicU64 = AtomicU64::new(0);
static TX_BLOCKED_NS: AtomicU64 = AtomicU64::new(0);
static TX_CONTENDED_COUNT: AtomicU64 = AtomicU64::new(0);
static TX_DROPPED_COUNT: AtomicU64 = AtomicU64::new(0);

static RX_COUNT: AtomicU64 = AtomicU64::new(0);
static RX_LEN_SAMPLES: AtomicU64 = AtomicU64::new(0);
static RX_LEN_SUM: AtomicU64 = AtomicU64::new(0);
static RX_LEN_MAX: AtomicU64 = AtomicU64::new(0);

/// Instrumented wrapper around `Sender<EngineEvent>`. Drop-in for the
/// scan-worker side: same `send` / `try_send` API so the 80+ call sites
/// in [`super::live`] don't change. Per-send timing accumulates into
/// the module's static counters; reset by [`drain_and_format`] each
/// emit cycle.
#[derive(Clone)]
pub struct PerfTx {
    inner: Sender<EngineEvent>,
}

impl PerfTx {
    pub fn new(inner: Sender<EngineEvent>) -> Self {
        Self { inner }
    }

    pub fn send(&self, ev: EngineEvent) -> Result<(), SendError<EngineEvent>> {
        let t0 = Instant::now();
        let r = self.inner.send(ev);
        let dt_ns = t0.elapsed().as_nanos() as u64;
        TX_COUNT.fetch_add(1, Ordering::Relaxed);
        TX_BLOCKED_NS.fetch_add(dt_ns, Ordering::Relaxed);
        if dt_ns >= CONTENDED_THRESHOLD_NS {
            TX_CONTENDED_COUNT.fetch_add(1, Ordering::Relaxed);
        }
        if r.is_err() {
            TX_DROPPED_COUNT.fetch_add(1, Ordering::Relaxed);
        }
        r
    }

    pub fn try_send(&self, ev: EngineEvent) -> Result<(), TrySendError<EngineEvent>> {
        let r = self.inner.try_send(ev);
        TX_COUNT.fetch_add(1, Ordering::Relaxed);
        if r.is_err() {
            TX_DROPPED_COUNT.fetch_add(1, Ordering::Relaxed);
        }
        r
    }
}

/// Record one rx-side observation: queue depth at moment of poll +
/// per-recv count after drain. Called from drain_events at start of
/// each frame (depth sample) and per try_recv hit (recv count).
pub fn note_recv() {
    RX_COUNT.fetch_add(1, Ordering::Relaxed);
}

pub fn note_queue_depth(len: usize) {
    let len = len as u64;
    RX_LEN_SAMPLES.fetch_add(1, Ordering::Relaxed);
    RX_LEN_SUM.fetch_add(len, Ordering::Relaxed);
    // CAS-loop for max
    let mut cur = RX_LEN_MAX.load(Ordering::Relaxed);
    while len > cur {
        match RX_LEN_MAX.compare_exchange_weak(cur, len, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => break,
            Err(observed) => cur = observed,
        }
    }
}

/// Snapshot counters and format the perf-channel line. Resets all
/// counters after read so each emit reports the window since the
/// prior emit (matches the per-frame cadence of perf-update / -body
/// / -sidebar emissions). Returns None if there's been zero activity
/// in the window (no sends + no rx samples) to keep the log quiet
/// pre-scan.
pub fn drain_and_format() -> Option<String> {
    let tx_count = TX_COUNT.swap(0, Ordering::Relaxed);
    let tx_blocked_ns = TX_BLOCKED_NS.swap(0, Ordering::Relaxed);
    let tx_contended = TX_CONTENDED_COUNT.swap(0, Ordering::Relaxed);
    let tx_dropped = TX_DROPPED_COUNT.swap(0, Ordering::Relaxed);
    let rx_count = RX_COUNT.swap(0, Ordering::Relaxed);
    let rx_len_samples = RX_LEN_SAMPLES.swap(0, Ordering::Relaxed);
    let rx_len_sum = RX_LEN_SUM.swap(0, Ordering::Relaxed);
    let rx_len_max = RX_LEN_MAX.swap(0, Ordering::Relaxed);

    if tx_count == 0 && rx_len_samples == 0 {
        return None;
    }

    let tx_blocked_ms = (tx_blocked_ns as f64) / 1_000_000.0;
    let rx_len_mean = if rx_len_samples == 0 {
        0.0
    } else {
        rx_len_sum as f64 / rx_len_samples as f64
    };
    Some(format!(
        "perf-channel: tx_count={tx_count} tx_blocked_ms={tx_blocked_ms:.3} \
         tx_contended={tx_contended} tx_dropped={tx_dropped} rx_count={rx_count} \
         rx_len_mean={rx_len_mean:.2} rx_len_max={rx_len_max} samples={rx_len_samples}"
    ))
}
