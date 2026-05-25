//! Achievement catalog + per-install profile fetchers.
//!
//! The catalog is public + static-ish (~35 entries today, grows as
//! design adds more); per client-spec §10.4 the GUI fetches it once
//! at app start and renders the badge wall against it. The per-
//! install profile carries the granted state — same shape as the
//! catalog plus `granted: bool` + `granted_at` per entry.
//!
//! Wire format pulled from the live backend at
//! `https://api.superdeduper.io/api/v1/achievements/catalog`. Could
//! land typify-driven structs once the JSON schema endpoint is
//! published; for now hand-rolled to keep deps thin.

use std::sync::OnceLock;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

/// Top-level catalog response from `GET /api/v1/achievements/catalog`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Catalog {
    pub version: String,
    pub achievements: Vec<CatalogEntry>,
}

/// One achievement row, sorted by `display_order`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CatalogEntry {
    pub id: String,
    pub name: String,
    pub description: String,
    /// `"low" | "mid" | "high"`. Drives tile colorization (bronze /
    /// silver / gold per the design treatment).
    pub tier: String,
    /// `"cumulative-threshold" | "single-run" | "streak" |
    /// "pathfinder" | "corpus-record" | "rank-tenure" | "easter-egg"
    /// | "client-claimed"`. Diagnostic only at the GUI layer.
    pub unlock_kind: String,
    pub display_order: i32,
}

/// Per-install profile response from
/// `GET /api/v1/profile/{install_id}`. Returns the full catalog
/// with per-entry grant state — one round-trip renders the badge
/// wall.
///
/// Field naming matches the backend's actual wire shape (verified
/// 2026-05-24 against `api.superdeduper.io`): lifetime stats are
/// nested under a `lifetime` object, and per-achievement entries
/// use `id` (not `achievement_id`). Helper methods on `Profile`
/// preserve the flat accessor names callers expected from the
/// earlier draft so the rest of the GUI doesn't have to know.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Profile {
    pub install_id: String,
    #[serde(default)]
    pub lifetime: Lifetime,
    #[serde(default)]
    pub achievements: Vec<ProfileGrant>,
}

impl Profile {
    pub fn lifetime_reclaimed_bytes(&self) -> u64 {
        self.lifetime.bytes_reclaimed
    }
    pub fn lifetime_scans(&self) -> u64 {
        self.lifetime.total_scans
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Lifetime {
    #[serde(default)]
    pub bytes_reclaimed: u64,
    #[serde(default)]
    pub total_scans: u64,
    #[serde(default)]
    pub total_files_seen: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProfileGrant {
    #[serde(rename = "id")]
    pub achievement_id: String,
    /// `true` when the install has earned this achievement.
    #[serde(default)]
    pub granted: bool,
    /// RFC3339 UTC timestamp, or `null` for ungranted entries.
    #[serde(default)]
    pub granted_at: Option<String>,
}

#[derive(Debug)]
pub enum FetchError {
    /// 4xx — server rejected (e.g. install_id unknown for the
    /// profile endpoint before first submission).
    Status(u16, String),
    /// Transport or 5xx. Retryable.
    Transient(String),
    /// JSON didn't deserialise to the expected shape.
    BadJson(String),
}

/// 10-second timeout — both endpoints are tiny + public + on a CDN.
const FETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

pub fn fetch_catalog(server_url: &str) -> Result<Catalog, FetchError> {
    let url = format!(
        "{}/api/v1/achievements/catalog",
        server_url.trim_end_matches('/')
    );
    let resp = ureq::get(&url).timeout(FETCH_TIMEOUT).call();
    match resp {
        Ok(r) => r
            .into_json::<Catalog>()
            .map_err(|e| FetchError::BadJson(format!("{e}"))),
        Err(ureq::Error::Status(code, r)) => Err(FetchError::Status(
            code,
            r.into_string().unwrap_or_default(),
        )),
        Err(ureq::Error::Transport(t)) => Err(FetchError::Transient(format!("{t}"))),
    }
}

pub fn fetch_profile(server_url: &str, install_id: &str) -> Result<Profile, FetchError> {
    fetch_profile_inner(server_url, install_id, /* cache_bust */ false)
}

/// Refresh-path variant. Forces a cache-buster query param + a
/// no-cache request header so CDN/Cloudflare layers can't return a
/// stale snapshot of the profile (a known footgun after a fresh
/// achievement grant: the GET-cached response is the just-stale
/// one). Initial app-start fetches skip the cache-bust because
/// there's no preceding state to invalidate.
pub fn fetch_profile_fresh(server_url: &str, install_id: &str) -> Result<Profile, FetchError> {
    fetch_profile_inner(server_url, install_id, /* cache_bust */ true)
}

fn fetch_profile_inner(
    server_url: &str,
    install_id: &str,
    cache_bust: bool,
) -> Result<Profile, FetchError> {
    let base = format!(
        "{}/api/v1/profile/{}",
        server_url.trim_end_matches('/'),
        urlencode_path(install_id),
    );
    let url = if cache_bust {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        format!("{base}?_t={ts}")
    } else {
        base
    };
    let mut req = ureq::get(&url).timeout(FETCH_TIMEOUT);
    if cache_bust {
        req = req
            .set("Cache-Control", "no-cache")
            .set("Pragma", "no-cache");
    }
    let resp = req.call();
    match resp {
        Ok(r) => r
            .into_json::<Profile>()
            .map_err(|e| FetchError::BadJson(format!("{e}"))),
        Err(ureq::Error::Status(code, r)) => Err(FetchError::Status(
            code,
            r.into_string().unwrap_or_default(),
        )),
        Err(ureq::Error::Transport(t)) => Err(FetchError::Transient(format!("{t}"))),
    }
}

fn urlencode_path(s: &str) -> String {
    // install_id is a UUID, so alphanumeric + dashes pass through
    // unchanged. Conservative encoder for safety.
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b == b'-' {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{:02X}", b));
        }
    }
    out
}

// ============================================================
// Process-wide cache for the GUI badge-wall renderer.
//
// Same pattern as the submission slots in `submission.rs`: one
// global mutex per kind. App start fires a worker that calls
// `fetch_catalog` + `fetch_profile` (if registered) and stashes
// the results; the badge-wall widget reads via `peek_*` each
// frame.
// ============================================================

#[derive(Debug, Clone)]
pub struct CatalogState {
    /// `None` while fetch is in flight or hasn't started; `Some(Ok)`
    /// on success; `Some(Err)` on terminal failure.
    pub catalog: Option<Result<Catalog, String>>,
    pub profile: Option<Result<Profile, String>>,
}

impl CatalogState {
    fn empty() -> Self {
        Self {
            catalog: None,
            profile: None,
        }
    }
}

static STATE: OnceLock<Mutex<CatalogState>> = OnceLock::new();

fn slot() -> &'static Mutex<CatalogState> {
    STATE.get_or_init(|| Mutex::new(CatalogState::empty()))
}

pub fn peek_state() -> CatalogState {
    slot().lock().clone()
}

pub fn set_catalog(r: Result<Catalog, String>) {
    slot().lock().catalog = Some(r);
}

pub fn set_profile(r: Result<Profile, String>) {
    slot().lock().profile = Some(r);
}

/// Spawn a background fetch for the catalog (always) and profile
/// (only if registered). Called once at app start. Cheap to call
/// twice; subsequent calls overwrite the slot with fresh data.
pub fn spawn_initial_fetch(server_url: String, install_id: Option<String>) {
    std::thread::spawn(move || {
        match fetch_catalog(&server_url) {
            Ok(c) => set_catalog(Ok(c)),
            Err(e) => set_catalog(Err(format!("{e:?}"))),
        }
        if let Some(id) = install_id {
            match fetch_profile(&server_url, &id) {
                Ok(p) => set_profile(Ok(p)),
                Err(e) => set_profile(Err(format!("{e:?}"))),
            }
        }
    });
}

/// Refetch only the profile and overwrite the slot. Fired by the
/// post-submit path: when a submission grants achievements the
/// backend updates the profile but the GUI's cached snapshot is
/// stale until we re-fetch. Catalog is untouched (its contents
/// don't change per-submit).
///
/// Uses [`fetch_profile_fresh`] with a cache-buster + no-cache
/// header so any CDN layer between client and backend is forced
/// to revalidate against origin. Logs each step to stderr so beta
/// testers can confirm the path executed.
pub fn spawn_profile_refresh(server_url: String, install_id: String) {
    std::thread::spawn(move || {
        eprintln!(
            "catalog: profile refresh fired (install_id={}, server_url={})",
            install_id, server_url
        );
        match fetch_profile_fresh(&server_url, &install_id) {
            Ok(p) => {
                let granted = p.achievements.iter().filter(|g| g.granted).count();
                let total = p.achievements.len();
                eprintln!(
                    "catalog: profile refresh OK (granted={}, total={}, lifetime_reclaimed={})",
                    granted,
                    total,
                    p.lifetime_reclaimed_bytes()
                );
                set_profile(Ok(p));
            }
            Err(e) => {
                eprintln!("catalog: profile refresh failed: {e:?}");
                // Deliberately don't overwrite a previously-good
                // profile with an error — a transient network blip
                // shouldn't grey out the badge wall.
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urlencode_path_passes_uuid_chars() {
        let s = "9d4a0000-0000-0000-0000-000000000001";
        assert_eq!(urlencode_path(s), s);
    }

    #[test]
    fn urlencode_path_escapes_specials() {
        assert_eq!(urlencode_path("a/b"), "a%2Fb");
    }

    #[test]
    fn catalog_state_starts_empty() {
        let s = CatalogState::empty();
        assert!(s.catalog.is_none());
        assert!(s.profile.is_none());
    }

    #[test]
    fn catalog_deserialises_minimal() {
        let json = r#"{"version":"v1","achievements":[
            {"id":"foo","name":"Foo","description":"d","tier":"low","unlock_kind":"single-run","display_order":1}
        ]}"#;
        let c: Catalog = serde_json::from_str(json).unwrap();
        assert_eq!(c.version, "v1");
        assert_eq!(c.achievements.len(), 1);
        assert_eq!(c.achievements[0].id, "foo");
    }

    #[test]
    fn profile_deserialises_with_missing_optional_fields() {
        // Backend may omit empty fields; serde defaults must catch them.
        let json = r#"{"install_id":"abc"}"#;
        let p: Profile = serde_json::from_str(json).unwrap();
        assert_eq!(p.install_id, "abc");
        assert_eq!(p.lifetime_reclaimed_bytes(), 0);
        assert!(p.achievements.is_empty());
    }

    #[test]
    fn profile_deserialises_live_backend_shape() {
        // Pinning the EXACT shape the backend emits. Drift here is
        // what caused the badge wall to show 0/N greyed: server uses
        // `id` per achievement (not `achievement_id`) and nests
        // lifetime stats under `lifetime`. If either shape changes,
        // this test fails first and we update the structs deliberately.
        let json = r#"{
            "install_id": "abc",
            "lifetime": { "bytes_reclaimed": 731677101, "total_scans": 3, "total_files_seen": 42 },
            "achievements": [
                { "id": "brisk", "name": "Brisk", "tier": "low", "display_order": 200, "granted": true, "granted_at": "2026-05-24T14:22:52Z" },
                { "id": "founder", "name": "Founder", "tier": "high", "display_order": 550, "granted": true, "granted_at": "2026-05-24T05:20:37Z" },
                { "id": "pioneer", "name": "Pioneer", "tier": "mid", "display_order": 560, "granted": false, "granted_at": null }
            ]
        }"#;
        let p: Profile = serde_json::from_str(json).expect("live-shape JSON parses");
        assert_eq!(p.install_id, "abc");
        assert_eq!(p.lifetime_reclaimed_bytes(), 731_677_101);
        assert_eq!(p.lifetime_scans(), 3);
        assert_eq!(p.achievements.len(), 3);
        assert_eq!(p.achievements[0].achievement_id, "brisk");
        assert!(p.achievements[0].granted);
        assert_eq!(p.achievements[1].achievement_id, "founder");
        assert!(p.achievements[1].granted);
        assert_eq!(p.achievements[2].achievement_id, "pioneer");
        assert!(!p.achievements[2].granted);
        let granted_count = p.achievements.iter().filter(|g| g.granted).count();
        assert_eq!(granted_count, 2);
    }
}
