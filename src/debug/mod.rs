//! `sd debug …` subcommands — read-only helpers that emit
//! engine-internal state for the containment-integration test harness
//! + other auditing tools. Distinct from `sd diagnose` (which is the
//! triage path for performance / inventory issues on a specific scan).
//!
//! Currently houses just the snapshot helper. More debug surface (e.g.
//! `sd debug inode-resolve <path>`) may land here as the
//! containment-integration spec expands.

pub mod snapshot;
