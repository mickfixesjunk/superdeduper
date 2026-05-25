//! G-track: gamification + leaderboards + achievements + canonical
//! bench corpus. Engine-side implementation per:
//!
//! * `~/sd-bench-local/design/gamification-client-spec.md` — contract
//! * `~/sd-bench-local/design/gamification-design.md` — vision
//! * `~/sd-bench-local/design/gamification-threat-model.md` — defenses
//! * `~/sd-bench-local/design/gamification-backend-spec.md` — API
//!
//! Phased rollout (single `feat/g-track` branch, one user-visible
//! release once all phases land):
//!
//! * **G1** (in progress) — hardware-detect / install.json /
//!   registration / HMAC submit / CLI `--share` / GUI post-scan modal
//! * **G2** — achievement evaluator + GUI unlock-toast + CLI rank
//! * **G3** — Google + Discord OAuth + cross-machine roll-up
//! * **G4** — canonical-bench corpus generator + Merkle proof flow
//!
//! Strictly gated on the `telemetry` Cargo feature. Distros / paranoid
//! users can ship a telemetry-stripped binary with
//! `--no-default-features --features gui`.

#![cfg(feature = "telemetry")]

pub mod account_badge_summary;
pub mod account_privacy;
pub mod captcha;
pub mod catalog;
pub mod hardware;
pub mod hmac_signer;
pub mod install;
pub mod oauth;
pub mod predicates;
pub mod ranks_poll;
pub mod registration;
pub mod submission;
pub mod vanity_slug;

// G2-G4 modules slot in here as the phases land:
// pub mod achievements;  // G2
// pub mod corpus;        // G4
// pub mod merkle;        // G4
