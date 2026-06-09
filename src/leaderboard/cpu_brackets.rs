//! CPU bracket classifier + vendored catalog snapshot (#217).
//!
//! Mirrors the web-side classifier at api/src/buckets/hardware-class.ts
//! (commit 65b91d1 + acfc3f8). The authoritative catalog YAML lives in
//! the web repo at api/data/cpu-brackets-catalog.yaml; engine bundles a
//! frozen JSON snapshot at data/cpu-brackets-catalog.json so the GUI +
//! CLI can render the user's bracket offline with no network round-
//! trip. The snapshot intentionally lags web's catalog by ~one engine
//! release — new CPU patterns added to web reach engine on the next
//! engine release. Same staleness model as the achievements catalog.
//!
//! Live endpoint: GET /api/v1/cpu-brackets/catalog
//! Public reference page: <leaderboard-url>/brackets

use std::sync::OnceLock;

use regex::Regex;
use serde::{Deserialize, Serialize};

const CATALOG_BYTES: &str = include_str!("../../data/cpu-brackets-catalog.json");

/// Top-level catalog response from `GET /api/v1/cpu-brackets/catalog`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Catalog {
    pub version: String,
    pub classifier_version: u32,
    pub brackets: Vec<Bracket>,
}

/// One bracket entry, sorted by `display_order`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Bracket {
    pub id: String,
    pub display_name: String,
    pub display_order: i32,
    pub intent: String,
    pub examples: Vec<String>,
    /// Regex `.source` strings; engine compiles case-insensitive at
    /// classify time.
    pub patterns: Vec<String>,
}

/// Wire id values: `"flagship" | "high-end" | "mid-range" | "older" |
/// "legacy" | "unknown"`. Kept as a string so the engine doesn't have
/// to recompile when web adds a new bracket; the GUI + CLI render the
/// display name from the catalog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BracketId(pub String);

impl BracketId {
    pub fn unknown() -> Self {
        Self("unknown".into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Returns the bundled snapshot. Parsed once; the parse result is
/// `Result<Catalog, _>` so a corrupted vendored JSON surfaces at first
/// call rather than panicking at startup.
pub fn catalog() -> &'static Catalog {
    static CATALOG: OnceLock<Catalog> = OnceLock::new();
    CATALOG.get_or_init(|| {
        serde_json::from_str(CATALOG_BYTES)
            .expect("vendored cpu-brackets-catalog.json is malformed")
    })
}

/// Strip Intel-style trademark markers from a CPU brand string so the
/// catalog regexes (which don't include them) match. Mirrors web's
/// hardware-class.ts fix at commit 65b91d1: `/\((?:R|TM|C)\)/gi`.
///
/// Why: Intel emits `Intel(R) Core(TM) i7-13700K` and the catalog
/// patterns assume `Intel Core i7-13700K`. AMD / Apple / Snapdragon
/// brand strings carry no embedded markers so the strip is a no-op for
/// them.
pub fn strip_trademark_markers(brand: &str) -> String {
    static MARKER_RE: OnceLock<Regex> = OnceLock::new();
    let re = MARKER_RE.get_or_init(|| {
        Regex::new(r"(?i)\((?:R|TM|C)\)").expect("trademark marker regex is well-formed")
    });
    re.replace_all(brand, "").into_owned()
}

/// Classify a CPU brand string against the bundled catalog. Returns
/// the first bracket whose patterns match (top-down per
/// `display_order`); falls back to `unknown` if no pattern matches.
/// Matching is case-insensitive. Pre-strips `(R)`/`(TM)`/`(C)` markers
/// via [`strip_trademark_markers`].
///
/// Patterns are compiled per call (see [`classify_cpu_with`] for the
/// rationale). A pattern that fails to compile is logged once per
/// (bracket, pattern) site and skipped — catalog is authored by
/// humans, so a stray regex shouldn't crash the binary but also
/// shouldn't silently mis-classify users into the wrong bracket.
pub fn classify_cpu(brand: &str) -> BracketId {
    classify_cpu_with(brand, catalog())
}

/// Look up a bracket's `display_name` from the bundled catalog given
/// its wire id (`"flagship"`, `"high-end"`, etc.). Returns `None` for
/// `"unknown"` and any id absent from the snapshot. Useful for the
/// CLI + GUI render paths where the id is what's stored / submitted
/// but the user sees the prettier name.
pub fn bracket_display_name(id: &str) -> Option<&'static str> {
    catalog()
        .brackets
        .iter()
        .find(|b| b.id == id)
        .map(|b| b.display_name.as_str())
}

/// Same as [`classify_cpu`] but accepts a caller-supplied catalog. Used
/// by tests to exercise alternate catalogs (mock data, fixture
/// catalogs) without touching the vendored snapshot.
///
/// Patterns are compiled per call rather than cached at the catalog
/// level. ~20 patterns at a few µs of regex compile each is well
/// below the cost of `diagnose` (a one-shot CLI path); caching would
/// shave a negligible slice while complicating `OnceLock` storage of
/// `Vec<Vec<Regex>>` alongside the deserialized JSON. Revisit only
/// if a future caller hits this in a hot loop.
///
/// A pattern that fails to compile is logged once (catalog authoring
/// bug — the caller's submission would have classified as the bracket
/// the broken pattern was supposed to match, so a silent skip can
/// mask a real bug) and the next pattern is tried.
pub fn classify_cpu_with(brand: &str, cat: &Catalog) -> BracketId {
    let normalized = strip_trademark_markers(brand);
    for bracket in &cat.brackets {
        for pattern in &bracket.patterns {
            let compiled = match Regex::new(&format!("(?i){pattern}")) {
                Ok(re) => re,
                Err(e) => {
                    log_pattern_error(&bracket.id, pattern, &e);
                    continue;
                }
            };
            if compiled.is_match(&normalized) {
                return BracketId(bracket.id.clone());
            }
        }
    }
    BracketId::unknown()
}

/// Log a malformed catalog pattern at most once per (bracket, pattern)
/// site per process. Catalog authoring bugs (an invalid regex in
/// web-side YAML) silently dropping at classify time mask real
/// classification gaps; the dedup keeps a long-running GUI from
/// spamming the log with the same broken pattern every frame.
fn log_pattern_error(bracket_id: &str, pattern: &str, err: &regex::Error) {
    use std::collections::HashSet;
    use std::sync::Mutex;
    static SEEN: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    let key = format!("{bracket_id}::{pattern}");
    let mut seen = SEEN.get_or_init(|| Mutex::new(HashSet::new())).lock().unwrap();
    if seen.insert(key) {
        crate::log_warn!(
            "cpu_brackets: malformed catalog pattern bracket={} pattern={:?} error={}",
            bracket_id,
            pattern,
            err
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vendored_catalog_parses() {
        let cat = catalog();
        assert_eq!(cat.version, "v1");
        assert_eq!(cat.classifier_version, 4);
        assert_eq!(cat.brackets.len(), 5);
        let ids: Vec<&str> = cat.brackets.iter().map(|b| b.id.as_str()).collect();
        assert_eq!(ids, vec!["flagship", "high-end", "mid-range", "older", "legacy"]);
    }

    /// Regression: explicit pin for AMD Ryzen 7 7800X3D — a popular
    /// benchmark CPU. Catalog v2 had the example mislabeled as
    /// 'Ryzen 9 7800X3D' and the pattern only matched a non-existent
    /// 'Ryzen 9 78XX' SKU; web v3 corrects both (example +
    /// `ryzen\s*[79]\s*78[0-9]{2}` defensive pattern). Pin so any
    /// future re-vendor that drops the v3 fix fails CI. Surfaced by
    /// codex on PR #183 (2026-06-08).
    #[test]
    fn classify_ryzen_7_7800x3d_lands_in_high_end() {
        assert_eq!(classify_cpu("AMD Ryzen 7 7800X3D").as_str(), "high-end");
    }

    /// Catalog v4 cross-section (web d2d92f4 2026-06-08). Hardens the
    /// methodology gap codex flagged on PR #183: catalog_examples_
    /// self_classify is necessary but not sufficient — a curated
    /// real-world brand list catches example-pattern joint-drift like
    /// the v2 'Ryzen 9 7800X3D' that self-classified but never matched
    /// real cpuid strings. Each row is a real shipping CPU brand
    /// string + the bracket web's marketing-tier semantics expect
    /// (per design 18:55 + 19:00 PDT 2026-06-08).
    #[test]
    fn classify_v4_cross_section_pins() {
        let cases: &[(&str, &str)] = &[
            ("AMD Ryzen 7 7800X3D", "high-end"),
            ("AMD Ryzen 7 9800X3D", "high-end"),
            ("AMD Ryzen 5 7600X", "high-end"),
            ("Apple M4 Pro", "high-end"),
            ("Apple M4", "high-end"),
            ("Intel(R) Core(TM) i7-14700K", "high-end"),
            ("Intel(R) Core(TM) i5-14600K", "high-end"),
            ("Intel(R) Core(TM) Ultra 7 265K", "high-end"),
            ("Intel(R) Core(TM) i9-10900K", "mid-range"),
            ("AMD Ryzen Threadripper 3970X", "older"),
        ];
        for (brand, want) in cases {
            let got = classify_cpu(brand);
            assert_eq!(
                got.as_str(),
                *want,
                "cross-section: brand {brand:?} classified as {:?}, expected {want:?}",
                got.as_str()
            );
        }
    }

    #[test]
    fn strip_trademark_markers_removes_r_tm_c() {
        assert_eq!(
            strip_trademark_markers("Intel(R) Core(TM) i7-13700K"),
            "Intel Core i7-13700K"
        );
        assert_eq!(strip_trademark_markers("Brand(C) Foo"), "Brand Foo");
        assert_eq!(strip_trademark_markers("brand(r) foo"), "brand foo");
        assert_eq!(strip_trademark_markers("brand(tm) foo"), "brand foo");
    }

    #[test]
    fn strip_trademark_markers_noop_for_clean_brands() {
        assert_eq!(strip_trademark_markers("Apple M3 Max"), "Apple M3 Max");
        assert_eq!(
            strip_trademark_markers("AMD Ryzen 9 7950X"),
            "AMD Ryzen 9 7950X"
        );
        assert_eq!(strip_trademark_markers("Snapdragon X Elite"), "Snapdragon X Elite");
    }

    /// Representative cross-section per design 16:50 PDT 2026-06-08
    /// ask. Each canonical model maps to its expected bracket.
    #[test]
    fn classify_representative_cross_section() {
        assert_eq!(classify_cpu("Apple M3 Max").as_str(), "flagship");
        assert_eq!(classify_cpu("Intel(R) Core(TM) i9-13900K").as_str(), "high-end");
        assert_eq!(classify_cpu("Apple M1").as_str(), "mid-range");
        assert_eq!(classify_cpu("Intel Core i7-9700K").as_str(), "older");
        assert_eq!(
            classify_cpu("Intel(R) Core(TM) i5-5250U CPU @ 1.60GHz").as_str(),
            "legacy"
        );
    }

    #[test]
    fn classify_unknown_brand_falls_back() {
        assert_eq!(classify_cpu("Fictional QuantumChip 9000").as_str(), "unknown");
        assert_eq!(classify_cpu("").as_str(), "unknown");
    }

    /// Every catalog example must classify back into its own bracket.
    /// Mirrors web's hardware-class.test self-classify round-trip
    /// (case 17 per acfc3f8) — catches catalog authoring drift where
    /// an example is added but no pattern matches it, or a pattern is
    /// changed in a way that breaks a stated example.
    #[test]
    fn catalog_examples_self_classify() {
        let cat = catalog();
        for bracket in &cat.brackets {
            for example in &bracket.examples {
                let got = classify_cpu(example);
                assert_eq!(
                    got.as_str(),
                    bracket.id,
                    "example {example:?} (under bracket {:?}) classified as {:?}",
                    bracket.id,
                    got.as_str()
                );
            }
        }
    }

    #[test]
    fn classify_is_case_insensitive() {
        assert_eq!(classify_cpu("APPLE M3 MAX").as_str(), "flagship");
        assert_eq!(classify_cpu("intel core i7-9700k").as_str(), "older");
    }

    #[test]
    fn classify_intel_trademark_strip_lands_in_bracket() {
        // Bare brand strings emit verbatim from cpuid / sysctl etc.;
        // ensure the (R)/(TM)-laden form matches the same bracket as
        // the clean form.
        let dirty = classify_cpu("Intel(R) Core(TM) i9-13900K");
        let clean = classify_cpu("Intel Core i9-13900K");
        assert_eq!(dirty, clean);
    }
}
