//! Synthetic event generator. Spins up a background thread that emits
//! [`EngineEvent`]s shaped like a real scan would, so the GUI can be
//! demoed (and screenshotted) on any host. Nothing in here touches the
//! filesystem.
//!
//! Two drives are simulated: a 4 TB spinning HDD and a 1 TB NVMe SSD.
//! The HDD's reads are scheduled in monotonically-increasing LCN order
//! (the killer trick the engine actually does); the SSD's reads are
//! sprayed across the address space at high parallelism. This makes
//! the LCN trace widget genuinely instructive — you can see why HDD
//! sort-by-LCN matters.

use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::Sender;

use crate::gui::events::{DriveInfo, DuplicateGroupSummary, EngineEvent, ReadSample, Stage};

pub fn spawn(tx: Sender<EngineEvent>) -> thread::JoinHandle<()> {
    thread::Builder::new()
        .name("superdupe-demo".into())
        .spawn(move || run(tx))
        .expect("spawn demo thread")
}

fn run(tx: Sender<EngineEvent>) {
    let _ = tx.send(EngineEvent::ScanStarted {
        at: Instant::now(),
        roots: vec![PathBuf::from(r"D:\Media"), PathBuf::from(r"E:\Backups")],
    });
    let _ = tx.send(EngineEvent::DriveDiscovered(DriveInfo {
        id: 0,
        model: "Seagate ST4000DM004 4TB".into(),
        has_seek_penalty: true,
        capacity_bytes: 4_000_787_030_016,
        volume_label: r"D:\".into(),
    }));
    let _ = tx.send(EngineEvent::DriveDiscovered(DriveInfo {
        id: 1,
        model: "Samsung 980 PRO 1TB".into(),
        has_seek_penalty: false,
        capacity_bytes: 1_000_204_886_016,
        volume_label: r"E:\".into(),
    }));
    let _ = tx.send(EngineEvent::Status(
        "MFT enumeration + LCN-sorted IOCP scheduling".into(),
    ));

    // Initial stage seeds (modelling a 1.2 M-file library).
    let seeds = [
        (Stage::Inventory, 1_240_312u64),
        (Stage::SizeGroup, 318_402),
        (Stage::LayoutResolve, 318_402),
        (Stage::Tier0Format, 64_211),
        (Stage::Tier1Head, 41_018),
        (Stage::Tier2HeadMidTail, 12_904),
        (Stage::Tier3Full, 4_812),
        (Stage::Confirmed, 0),
    ];
    for (stage, total) in seeds {
        let _ = tx.send(EngineEvent::StageTick {
            stage,
            delta: 0,
            total,
        });
    }

    let start = Instant::now();
    let mut hdd_lcn: u64 = 0;
    let mut ssd_seed: u64 = 0x9E37_79B9_7F4A_7C15;
    let hdd_lcn_max: u64 = 4_000_787_030_016 / 2;
    let mut group_id = 0u64;
    let mut confirmed: u64 = 0;

    // Emit ~120 frames of activity (~12 s of demo).
    for frame in 0..240 {
        if tx.is_full() {
            // Be a polite producer.
            thread::sleep(Duration::from_millis(50));
            continue;
        }

        // HDD: ~64 sequential LCN reads per frame, monotonically up.
        for _ in 0..64 {
            hdd_lcn = (hdd_lcn + jitter(&mut ssd_seed, 1 << 18, 1 << 22)) % hdd_lcn_max;
            let _ = tx.send(EngineEvent::Read(ReadSample {
                drive: 0,
                lcn_bytes: hdd_lcn,
                bytes: 64 * 1024 + (jitter(&mut ssd_seed, 0, 8) * 4096),
                latency_us: 4500 + jitter(&mut ssd_seed, 0, 1200),
                at: Instant::now(),
            }));
        }

        // SSD: ~256 random reads per frame.
        for _ in 0..256 {
            let lcn = next_rand(&mut ssd_seed) % 1_000_204_886_016;
            let _ = tx.send(EngineEvent::Read(ReadSample {
                drive: 1,
                lcn_bytes: lcn,
                bytes: 128 * 1024,
                latency_us: 120 + jitter(&mut ssd_seed, 0, 80),
                at: Instant::now(),
            }));
        }

        // Stage ticks: as work progresses, Confirmed and Tier3 grow.
        let progress = frame as u64 + 1;
        let _ = tx.send(EngineEvent::StageTick {
            stage: Stage::Tier3Full,
            delta: 20,
            total: 4_812 + progress * 18,
        });
        if frame % 2 == 0 {
            confirmed += 7;
            let _ = tx.send(EngineEvent::StageTick {
                stage: Stage::Confirmed,
                delta: 7,
                total: confirmed,
            });
        }

        // Periodically discover a duplicate group.
        if frame % 5 == 0 {
            group_id += 1;
            let size = match group_id % 6 {
                0 => 12 * 1024,           // text doc
                1 => 350 * 1024,          // photo
                2 => 5 * 1024 * 1024,     // mp3
                3 => 42 * 1024 * 1024,    // raw
                4 => 1_200 * 1024 * 1024, // 1.2 GB movie
                _ => 8_500 * 1024 * 1024, // 8.5 GB iso
            };
            let count = 2 + (group_id % 4) as usize;
            let files: Vec<PathBuf> = (0..count)
                .map(|i| {
                    PathBuf::from(format!(
                        r"D:\Media\Library\group{:03}\{:02}-copy{}.bin",
                        group_id,
                        group_id % 50,
                        i
                    ))
                })
                .collect();
            let _ = tx.send(EngineEvent::DuplicateFound(DuplicateGroupSummary {
                size,
                content_hash: fake_hash(group_id),
                files,
            }));
        }

        if frame == 60 {
            let _ = tx.send(EngineEvent::Status(
                "Tier 2 → Tier 3 escalation (large media files)".into(),
            ));
        }
        if frame == 160 {
            let _ = tx.send(EngineEvent::Status(
                "USN delta complete · cache hit ratio 99.2%".into(),
            ));
        }

        thread::sleep(Duration::from_millis(50));
    }

    let _ = tx.send(EngineEvent::ScanFinished {
        at: Instant::now(),
        total_files: 1_240_312,
        total_bytes_read: 286 * 1024 * 1024 * 1024,
        duplicates: confirmed,
        reclaimable_bytes: 184 * 1024 * 1024 * 1024,
    });
    let _ = tx.send(EngineEvent::Status(format!(
        "Scan complete in {:.1}s — 184.0 GiB reclaimable",
        start.elapsed().as_secs_f64()
    )));
}

fn next_rand(state: &mut u64) -> u64 {
    // SplitMix64 — adequate for visual variety.
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

fn jitter(state: &mut u64, lo: u64, hi: u64) -> u64 {
    let span = hi.saturating_sub(lo).max(1);
    lo + (next_rand(state) % span)
}

fn fake_hash(seed: u64) -> String {
    let mut s = String::with_capacity(64);
    let mut x = seed;
    for _ in 0..32 {
        x = x
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        use std::fmt::Write;
        let _ = write!(s, "{:02x}", (x >> 24) as u8);
    }
    s
}
