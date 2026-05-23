# Leaderboard — design spec

> **Status:** spec for the **superdeduper-backend** and
> **superdeduper-website** agents to implement against.
> Engine-side stub at `src/telemetry.rs` (when implemented) will
> have the submission signature ready.

---

## Product hook

Users sign in (Google OAuth), submit benchmark runs from sd, and
compete on a public leaderboard for "fastest deduper on your
hardware class."

The key insight: a flat "who's fastest" leaderboard doesn't work
because hardware matters as much as software. **Hardware-class
bucketing** makes the competition fair, and turns the leaderboard
into a meaningful peer comparison ("am I getting the most out of
my machine?").

## Submission anatomy

When a user finishes a scan and opts to submit (one-click in GUI,
`--submit-leaderboard` flag in CLI), sd sends a single POST request:

```
POST https://api.superdeduper.com/v1/leaderboard-submit
Authorization: Bearer <google_oauth_jwt>
Content-Type: application/json
Idempotency-Key: <run_uuid>
```

Body:

```json
{
  "schema": "superdeduper.leaderboard-submit.v1",
  "run_uuid": "9d4a...",                       // anti-replay
  "sd_version": "0.1.0+ba7e66c",
  "sd_build_hash": "sha256:a1d1d9e5...",       // verifies binary identity
  "timestamp_unix": 1748022000,

  "hardware": {
    "cpu_model": "AMD Ryzen 9 9950X3D",
    "cpu_threads": 32,
    "cpu_class": "x86_64-modern-high",         // backend-bucketing-friendly
    "ram_gb_tier": 128,                        // rounded to nearest tier
    "drive_class": "NVMe-PCIe4",
    "drive_seq_read_mbps_observed": 6800,      // from diagnose probe
    "os": "Windows 11 24H2"
  },

  "workload": {
    "file_count_bucket": "100k-1M",            // never the actual count
    "total_size_bucket_gb": "100-1000",
    "avg_file_size_bucket": "1MB-10MB",
    "dup_density_pct_bucket": "0-10"           // % of files in dup groups
  },

  "results": {
    "wall_clock_ms": 12345,
    "bytes_hashed": 9871234567,
    "peak_rss_mb": 142,
    "reclaimable_inode_bytes": 1234567890,
    "dup_groups": 5421,
    "hash_algo": "river5-aesni-v15"
  },

  "attestation": {
    "defender_rtp_state_pre": false,
    "defender_rtp_state_post": true,
    "cache_state_pre": "purged",               // verifiable via NtSetSystemInformation receipt
    "preflight_report_hash": "sha256:...",     // hash of the diagnose JSON for this run
    "corpus_signature_hash": "sha256:..."      // hash of (sorted_file_count, sorted_file_size_distribution_buckets)
  },

  "anti_cheat": {
    "engine_attestation_blob": "...",          // sd computes via a key the binary carries
    "scan_log_proof_hash": "sha256:..."        // hash of internal diagnostic counters
  }
}
```

## Anti-cheat strategy

Submissions can be faked. The backend rejects anything that fails
the following checks:

### 1. Binary identity verification

`sd_build_hash` must match a published superdeduper release SHA.
Backend keeps a list of known-good build hashes. Reject if unknown.

This stops users from modifying sd to lie about timing.

### 2. Run attestation

Engine produces an attestation blob during the scan, signed with a
key that ships with the official binary (key rotated per release).
Backend verifies the signature.

The attestation blob covers:
* `run_uuid` (no replays)
* `wall_clock_ms` (binding submitter's claim to what engine measured)
* `corpus_signature_hash`
* `defender_rtp_state_pre`
* `preflight_report_hash`

Tampering with any field invalidates the signature.

### 3. Plausibility checks (backend-side)

Hard caps that flag for review:
* `wall_clock_ms < workload.total_size_bytes / (1.5 × drive_seq_read_mbps_observed)`
  → faster-than-physically-possible. Reject.
* `bytes_hashed > workload.total_size_bytes × 1.1`
  → over-counted hashed bytes. Suspicious.
* `dup_density_bucket == 0-10` but `dup_groups > workload.file_count_bucket / 5`
  → mismatched dup story. Suspicious.

### 4. Defender state attestation

If `defender_rtp_state_pre == false`, the result might be unfairly
fast (RTP off scans 2–3x faster). The leaderboard CAN show these
results, but they're flagged with a "RTP off" badge so users can
filter to "RTP-on only" for fair comparison.

This also discourages people from running with RTP off just for the
leaderboard.

### 5. Workload signature collision check

`corpus_signature_hash` is a hash of the sorted file-size buckets.
Two users scanning the same corpus produce the same signature. This
isn't private, just useful for the backend to detect "user is
benching the same canonical test corpus that we ship for comparison
purposes" vs "user is benching their own random data."

For an official Leaderboard Mode, the user could be required to
scan a canonical superdeduper test corpus (downloaded from the
website). Run-to-run comparable across all users.

## Hardware-class bucketing

Backend groups submissions into hardware classes for fair comparison:

| dimension | buckets |
|---|---|
| CPU class | `x86_64-modern-high` (Ryzen 9 / i9 11th+ / Apple M-series), `x86_64-modern-mid` (Ryzen 5/7 mid, i5/i7 10th-13th), `x86_64-legacy` (older), `x86_64-low` (Celeron / Atom), `arm64-modern` (Apple/Snapdragon Elite) |
| RAM tier | `≤16`, `17-32`, `33-64`, `>64` GB |
| Drive class | `NVMe-PCIe4`, `NVMe-PCIe3`, `SATA-SSD`, `HDD`, `External-USB` |

Three-dimensional bucket = (cpu × ram × drive). Probably 5 × 4 × 5 = 100 buckets total. Sparse — only populated where users actually exist. Backend can collapse rarely-populated buckets into "uncommon hardware" pools.

Leaderboard view: user picks a bucket dimension to compare on. Default: their own bucket.

## Workload-class bucketing

Within a hardware class, results are further grouped by workload shape:

| dimension | buckets |
|---|---|
| File count | `<1k`, `1k-10k`, `10k-100k`, `100k-1M`, `>1M` |
| Total size | `<1GB`, `1-10GB`, `10-100GB`, `100-1000GB`, `>1TB` |
| Avg file size | `<10KB`, `10-100KB`, `100KB-1MB`, `1MB-10MB`, `10MB-100MB`, `>100MB` |
| Dup density | `0-10%`, `10-25%`, `25-50%`, `50-100%` |

User compares against their workload-class, not the whole pool.

## Leaderboard view (UI for website + GUI)

```
                  Top 10 — Ryzen 9 / 64-128GB / NVMe PCIe4
                  10k-100k files, 10-100GB, 1-10MB avg, 0-10% dups

  #1   alice           sd 0.1.0+ba7e66c       11.2 s     [RTP off]
  #2   bob             sd 0.1.0+ba7e66c       13.4 s     [RTP on]
  #3   you             sd 0.1.0+ba7e66c       14.1 s     [RTP on]
  ...

  Your run: 14.1 s — 78th percentile in your bucket. ↑ from 65th yesterday.
```

* "RTP off" badge clearly distinguishes runs.
* Filter toggles: `RTP on only`, `Top N by …`, `Time range`.
* Tap a row → user's public profile (username + opt-in nickname; no real name).

## Account flow

* Google OAuth only at launch (simplifies; adds GitHub/Apple later).
* `sd` GUI launches the OAuth flow in the system browser; receives a JWT.
* Token persisted at `%LOCALAPPDATA%\superdeduper\auth.json` with file permissions = current user only.
* Refresh on expiry.
* "Sign out" wipes the token file.

## Engine stub

Add `src/telemetry.rs` with:

```rust
//! Engine-side stub for leaderboard + telemetry submissions.
//! Backend at https://api.superdeduper.com is built by the
//! superdeduper-backend agent; this module is the engine's view of
//! what data goes in and how it's signed.

pub struct SubmissionPayload { ... }
pub struct AttestationBlob { ... }

/// Build a SubmissionPayload from a completed scan.
pub fn build_submission(...) -> SubmissionPayload { todo!() }

/// Sign the attestation portion with the binary's release key.
pub fn sign_attestation(...) -> AttestationBlob { todo!() }

/// POST to the backend. No-op stub until backend exists.
#[cfg(feature = "telemetry")]
pub async fn submit(...) -> anyhow::Result<SubmissionResponse> { todo!() }
```

Feature-gated behind `telemetry` so the binary can ship without
network code until backend is live.

## Privacy considerations

* What's submitted: machine fingerprint (hashed, irreversible),
  workload-shape buckets (no paths/names), wall-clock + reclaimable.
* What's not: paths, filenames, file contents, hash values that could
  be reversed to identify files, hostname, user account.
* EU default: opt-in only with explicit consent text.
* Right to delete: account deletion wipes all submitted runs.
* Privacy policy URL: https://superdeduper.com/privacy (TBD).

## API endpoints (backend ships these)

* `POST /v1/leaderboard-submit` — submit a run
* `GET  /v1/leaderboard/:bucket` — list top N for a bucket
* `GET  /v1/users/:id/runs` — public profile
* `POST /v1/users/me/delete` — account deletion
* `GET  /v1/hardware-classes` — current bucket definitions (versioned)
* `POST /v1/preflight-submit` — anonymous preflight telemetry (Block T)

## Phased rollout

1. **Phase 1 (post-engine-stable):** Backend builds endpoint, accepts
   submissions, stores them. No leaderboard UI yet. Just data
   collection to verify schema works.
2. **Phase 2:** Backend builds leaderboard query API + bucket
   definitions. Website agent builds public leaderboard view.
3. **Phase 3:** GUI integration — "submit your run" button in sd's
   scan-complete view. Settings opt-in.
4. **Phase 4:** Anti-cheat hardening based on observed gaming
   attempts. Probably need this from day 1 actually — submission
   signing key rotation, plausibility caps, etc.

## Open questions for backend agent

1. **Auth provider:** Google only at launch? Or also GitHub/Apple?
2. **Storage:** Postgres / DynamoDB / something else?
3. **Region:** US-only at launch? CDN?
4. **GDPR posture:** consent-banner standard or stricter?
5. **API key issuance for the engine attestation signature:** how
   does the engine receive its release key? Ship in the binary?
   Fetch at first run? Implications for offline scans.

Will sync with backend agent on these once the spec is in their hands.
