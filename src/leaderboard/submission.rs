//! Submit a completed scan's results to the leaderboard backend.
//!
//! Flow per client-spec §6:
//! 1. Build the payload from scan results + hardware fingerprint
//! 2. Canonicalize to JSON (sorted keys, no whitespace)
//! 3. HMAC-sign with the install_key
//! 4. POST to `/api/v1/submit` with `X-Sd-Signature` header
//! 5. On 5xx or network failure: enqueue to disk (offline queue,
//!    50-submission cap) and retry on next launch
//! 6. On 200: surface rank + achievements to the GUI
//! 7. On 409 (`duplicate_submission`): show "no change since last
//!    submit" as a neutral status, not an error
//!
//! TODO(g1): implement against client-spec §6.

pub fn submit_with_offline_queue() -> std::io::Result<SubmissionOutcome> {
    todo!("g1: canonicalize + sign + POST + queue-on-fail")
}

#[derive(Debug)]
pub enum SubmissionOutcome {
    Accepted {
        submission_id: String,
        ranks: Vec<RankEntry>,
        achievements: Vec<String>,
    },
    DuplicateNoChange,
    Queued,
    Rejected(String),
}

#[derive(Debug)]
pub struct RankEntry {
    pub category: String,
    pub bracket: String,
    pub rank: u64,
    pub bucket_size: u64,
}
