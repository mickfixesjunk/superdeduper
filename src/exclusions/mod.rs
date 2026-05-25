//! Client-side file-exclusion filter — extensions + path patterns +
//! preset packs. Per `~/sd-bench-local/design/file-exclusion-spec.md`
//! (Option B hybrid).
//!
//! Default state is OFF. The user opts in via Settings → Exclusions
//! (GUI) or `--exclusions on` (CLI). All preset packs ship visible
//! but inactive; the user flips them on individually.
//!
//! Pipeline shape:
//!
//! ```text
//! ExclusionConfig  ── user-edited, persisted as TOML
//!         │
//!         ▼  (compile-once at scan start)
//! ExclusionPolicy  ── pre-built matchers (globset + HashSet)
//!         │
//!         ▼  (per-file, called from walker hot path)
//! Decision  ── Included | Excluded { reason }
//! ```
//!
//! Per-file evaluation is the hot path; it runs once per walker
//! entry. Compile happens once per scan; pattern matching is O(1)
//! for extensions and O(N_globs) for path patterns. Spec
//! acceptance criterion 12 ("within 5% of baseline scan time with
//! 100 active rules") is the perf bar.
//!
//! Day 1 scope of `feat/scan-options`: structs + matchers + TOML
//! serde + unit tests. Preset-pack content (Day 2), walker hook
//! (Day 2), CLI flags (Day 3), GUI tab (Days 4-5).

pub mod config;
pub mod matcher;
pub mod presets;

use std::collections::HashSet;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use globset::GlobSet;

pub use self::config::{ExclusionConfig, ExclusionConfigError};
pub use self::presets::BuiltinPresets;

/// Stable identifier for a built-in preset pack. The full pack
/// content (extensions + path patterns) is filled in on Day 2 in
/// the (forthcoming) `presets` module. For Day 1 the IDs exist so
/// `ExclusionConfig` can serialise / deserialise them and the
/// runtime policy can report them as the reason for an exclusion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PresetPackId {
    SystemLibraries,
    BuildArtefacts,
    VcsInternals,
    PackageManagerCaches,
    OsSystemTrees,
    BrowserCaches,
    AppCaches,
    AvSignatureDatabases,
}

impl PresetPackId {
    /// All 8 preset packs in display order. Stable; new packs
    /// append at the end to preserve user config compatibility.
    pub const ALL: [PresetPackId; 8] = [
        PresetPackId::SystemLibraries,
        PresetPackId::BuildArtefacts,
        PresetPackId::VcsInternals,
        PresetPackId::PackageManagerCaches,
        PresetPackId::OsSystemTrees,
        PresetPackId::BrowserCaches,
        PresetPackId::AppCaches,
        PresetPackId::AvSignatureDatabases,
    ];

    /// User-facing label for the pack. Used by the Settings UI's
    /// preset row and by the scan summary's per-pack breakdown if
    /// per-pack counters land in v2.
    pub fn label(self) -> &'static str {
        match self {
            PresetPackId::SystemLibraries => "System libraries",
            PresetPackId::BuildArtefacts => "Build artefacts",
            PresetPackId::VcsInternals => "VCS internals",
            PresetPackId::PackageManagerCaches => "Package manager caches",
            PresetPackId::OsSystemTrees => "OS system trees",
            PresetPackId::BrowserCaches => "Browser caches",
            PresetPackId::AppCaches => "App-specific caches",
            PresetPackId::AvSignatureDatabases => "AV signature databases",
        }
    }
}

/// What [`ExclusionPolicy::evaluate`] decides for a given path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// File proceeds into the scan.
    Included,
    /// File is filtered out of the scan; carries the rule class
    /// that triggered the exclusion so the scan summary + Settings
    /// "Excluded last scan" counter can break down by source.
    Excluded(ExclusionReason),
}

/// Why a file was excluded. The carrier distinguishes preset-pack
/// hits (so the scan summary can attribute "excluded by System
/// libraries pack") from user-added custom rules.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExclusionReason {
    /// Path matched a glob pattern from this preset pack.
    PresetPackPath(PresetPackId),
    /// Extension matched an entry from this preset pack.
    PresetPackExtension(PresetPackId),
    /// Path matched a user-defined custom glob pattern.
    CustomPattern,
    /// Extension matched a user-defined custom extension.
    CustomExtension,
}

/// Compiled, runtime-ready exclusion filter. Construct from an
/// [`ExclusionConfig`] via [`ExclusionPolicy::compile`] at scan
/// start; call [`ExclusionPolicy::evaluate`] for each walker entry.
///
/// Day 1 builds a single combined globset for all active path
/// patterns and a single combined HashSet for all active
/// extensions. This is fast enough — `globset` is the same engine
/// rustc uses for `[workspace.exclude]` and ripgrep-class tools.
/// Day 2 may split per-pack to support per-pack counters.
#[derive(Debug, Clone, Default)]
pub struct ExclusionPolicy {
    /// `false` short-circuits all evaluate() calls to Included.
    /// Matches the master toggle in Settings → Exclusions and the
    /// `--no-exclusions` CLI flag.
    enabled: bool,
    /// Combined path-pattern set across all active sources (preset
    /// packs + custom patterns). When matched, we can't immediately
    /// tell which rule class hit — so we keep parallel per-class
    /// sets below for reason attribution.
    combined_paths: GlobSet,
    /// Same shape, but ONLY user-added custom patterns. Used to
    /// disambiguate "did the combined hit come from a custom rule
    /// or a preset?" without rescanning everything.
    custom_paths: GlobSet,
    /// Combined extension set. Stored lowercase; evaluate()
    /// lowercases the input path's extension before lookup.
    /// Hot-path lookup is O(1) so we don't bother splitting per-pack.
    combined_exts: HashSet<String>,
    /// User-added custom extensions only. Same disambiguation
    /// trick as `custom_paths`.
    custom_exts: HashSet<String>,
}

impl ExclusionPolicy {
    /// Empty "all included" policy. Equivalent to disabled. Used
    /// as the default when the user hasn't touched Settings →
    /// Exclusions yet.
    pub fn disabled() -> Self {
        Self::default()
    }

    /// Build a runtime policy from a config + the preset-pack
    /// data store. Day 1 takes `presets` as an opaque trait object
    /// so the presets module can fill in real content on Day 2
    /// without us refactoring callers.
    ///
    /// Returns an error if any user-supplied pattern or extension
    /// is malformed (so the GUI / CLI can surface "your custom
    /// pattern X is invalid" before the scan begins).
    pub fn compile(
        config: &ExclusionConfig,
        presets: &dyn PresetSource,
    ) -> Result<Self, ExclusionConfigError> {
        if !config.enabled {
            // Master toggle off — short-circuit. All evaluate calls
            // will return Included regardless of the lists below.
            return Ok(Self {
                enabled: false,
                ..Default::default()
            });
        }

        let mut combined_path_builder = globset::GlobSetBuilder::new();
        let mut custom_path_builder = globset::GlobSetBuilder::new();
        let mut combined_exts = HashSet::new();
        let mut custom_exts = HashSet::new();

        for pack_id in &config.active_packs {
            let pack = presets.get(*pack_id);
            for pat in pack.paths {
                let glob = build_glob_with_literal_separator(pat)?;
                combined_path_builder.add(glob);
            }
            for ext in pack.extensions {
                combined_exts.insert(normalize_extension(ext));
            }
        }

        for pat in &config.custom_patterns {
            let glob = build_glob_with_literal_separator(pat)?;
            combined_path_builder.add(glob.clone());
            custom_path_builder.add(glob);
        }

        for ext in &config.custom_extensions {
            let normalised = normalize_extension(ext);
            combined_exts.insert(normalised.clone());
            custom_exts.insert(normalised);
        }

        let combined_paths = combined_path_builder
            .build()
            .map_err(|e| ExclusionConfigError::BuildFailed(format!("{e}")))?;
        let custom_paths = custom_path_builder
            .build()
            .map_err(|e| ExclusionConfigError::BuildFailed(format!("{e}")))?;

        Ok(Self {
            enabled: true,
            combined_paths,
            custom_paths,
            combined_exts,
            custom_exts,
        })
    }

    /// Hot-path per-file check. Application order per spec §2.4:
    /// path match first, then extension. Excluded files are
    /// dropped silently from the walker output.
    ///
    /// Path normalisation: backslashes in the input are converted
    /// to forward slashes before matching, so Windows-shell paths
    /// (`C:\Windows\WinSxS\...`) match patterns stored with
    /// forward slashes (`C:/Windows/WinSxS/**`). Allocates a
    /// `String` only when the input actually contains a `\` —
    /// pure-forward-slash paths take the zero-allocation path.
    pub fn evaluate(&self, path: &Path) -> Decision {
        if !self.enabled {
            return Decision::Included;
        }
        let path_str_buf;
        let match_target: &Path =
            if cfg!(windows) || path.to_str().map(|s| s.contains('\\')).unwrap_or(false) {
                // Normalise backslashes once; reuse the buffer for
                // both the path-match and the extension lookup.
                let normalised = path
                    .to_str()
                    .map(|s| s.replace('\\', "/"))
                    .unwrap_or_default();
                path_str_buf = std::path::PathBuf::from(normalised);
                &path_str_buf
            } else {
                path
            };
        if self.combined_paths.is_match(match_target) {
            let reason = if self.custom_paths.is_match(match_target) {
                ExclusionReason::CustomPattern
            } else {
                ExclusionReason::PresetPackPath(PresetPackId::SystemLibraries)
            };
            return Decision::Excluded(reason);
        }
        if let Some(ext) = matcher::lowercased_extension(match_target) {
            if self.combined_exts.contains(&ext) {
                let reason = if self.custom_exts.contains(&ext) {
                    ExclusionReason::CustomExtension
                } else {
                    ExclusionReason::PresetPackExtension(PresetPackId::SystemLibraries)
                };
                return Decision::Excluded(reason);
            }
        }
        Decision::Included
    }

    /// Whether evaluate() will short-circuit to Included.
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }
}

/// Lookup interface for preset-pack content. Day 1 only defines the
/// shape; Day 2's `presets` module provides the actual data.
pub trait PresetSource {
    fn get(&self, id: PresetPackId) -> PresetPack;
}

/// Snapshot of one preset pack's content for the duration of a
/// `compile()` call. References, not owned data — the pack's
/// content lives as `'static` arrays in the (Day 2) `presets`
/// module.
pub struct PresetPack {
    pub extensions: &'static [&'static str],
    pub paths: &'static [&'static str],
}

/// Empty preset source. Used by tests + by the disabled default
/// policy. Day 2 replaces with the real preset data.
#[derive(Debug, Clone, Copy, Default)]
pub struct EmptyPresets;

impl PresetSource for EmptyPresets {
    fn get(&self, _id: PresetPackId) -> PresetPack {
        PresetPack {
            extensions: &[],
            paths: &[],
        }
    }
}

/// Live counters bumped by the walker when an exclusion fires.
/// Wrapped in [`Arc`] so multiple worker threads can share a single
/// instance without lock contention — atomics are lock-free for
/// `u64` increments on every supported platform.
///
/// Surfaced in the scan summary as `"Excluded by Settings →
/// Exclusions: N files (Y bytes)"`. Reset implicitly per scan
/// because the walker constructs a fresh `Arc<ExclusionCounters>`
/// alongside the rest of the per-scan state.
#[derive(Debug, Default)]
pub struct ExclusionCounters {
    pub excluded_files: AtomicU64,
    pub excluded_bytes: AtomicU64,
}

impl ExclusionCounters {
    /// Allocate fresh counters. Cheap; just two atomics.
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Record one excluded file. Called from the walker hot path
    /// each time `ExclusionPolicy::evaluate` returns Excluded.
    pub fn record(&self, bytes: u64) {
        self.excluded_files.fetch_add(1, Ordering::Relaxed);
        self.excluded_bytes.fetch_add(bytes, Ordering::Relaxed);
    }

    /// Snapshot reader for the scan summary.
    pub fn snapshot(&self) -> ExclusionCountersSnapshot {
        ExclusionCountersSnapshot {
            excluded_files: self.excluded_files.load(Ordering::Relaxed),
            excluded_bytes: self.excluded_bytes.load(Ordering::Relaxed),
        }
    }
}

/// Value-only snapshot for scan-summary rendering. Decoupled from
/// `ExclusionCounters` so summary code doesn't pull `AtomicU64` or
/// `Arc` into its signature.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ExclusionCountersSnapshot {
    pub excluded_files: u64,
    pub excluded_bytes: u64,
}

/// Normalise an extension to lowercase + strip any leading dot.
/// `.DLL` and `dll` both store as `dll`. Comparison happens
/// against the lowercased file extension extracted by
/// `matcher::lowercased_extension`.
fn normalize_extension(ext: &str) -> String {
    ext.trim_start_matches('.').to_ascii_lowercase()
}

/// Build a glob with `literal_separator(true)`. This makes the
/// single-`*` wildcard match exactly one path segment (no `/` or
/// `\` crossings). `**` continues to match across separators
/// regardless of the setting.
///
/// Why this matters: a pattern like `**/.mozilla/firefox/*/cache2/**`
/// is meant to match a profile dir + its `cache2/` subdir.
/// With default `literal_separator(false)`, the middle `*` would
/// match `foo/bar/baz/cache2`, catching everything under the
/// firefox dir. Spec acceptance §2.4 ordering depends on accurate
/// per-segment matching to avoid over-broad exclusions.
fn build_glob_with_literal_separator(pat: &str) -> Result<globset::Glob, ExclusionConfigError> {
    globset::GlobBuilder::new(pat)
        .literal_separator(true)
        .build()
        .map_err(|e| ExclusionConfigError::BadPattern {
            pattern: pat.to_string(),
            source: e,
        })
}

#[cfg(test)]
mod tests {
    //! Day 1 acceptance: enable/disable short-circuit + custom
    //! extension hit + custom pattern hit + no-match path + bad
    //! pattern error. Preset-pack hits exercised on Day 2 with
    //! the real preset content.

    use super::*;
    use std::path::PathBuf;

    fn empty_presets() -> EmptyPresets {
        EmptyPresets
    }

    #[test]
    fn disabled_policy_includes_everything() {
        let policy = ExclusionPolicy::disabled();
        assert_eq!(policy.evaluate(Path::new("foo.dll")), Decision::Included);
        assert_eq!(
            policy.evaluate(Path::new("/home/user/anything")),
            Decision::Included
        );
    }

    #[test]
    fn master_toggle_off_includes_everything_even_with_rules() {
        let config = ExclusionConfig {
            enabled: false,
            active_packs: vec![],
            custom_extensions: vec![".dll".into()],
            custom_patterns: vec!["**/*".into()],
        };
        let policy = ExclusionPolicy::compile(&config, &empty_presets()).unwrap();
        // Even though .dll + **/* would match, master toggle is
        // off so every file flows through.
        assert_eq!(policy.evaluate(Path::new("foo.dll")), Decision::Included);
    }

    #[test]
    fn custom_extension_hit_excludes() {
        let config = ExclusionConfig {
            enabled: true,
            active_packs: vec![],
            custom_extensions: vec![".dll".into()],
            custom_patterns: vec![],
        };
        let policy = ExclusionPolicy::compile(&config, &empty_presets()).unwrap();
        assert_eq!(
            policy.evaluate(Path::new("system32/kernel32.dll")),
            Decision::Excluded(ExclusionReason::CustomExtension)
        );
    }

    #[test]
    fn custom_extension_hit_is_case_insensitive() {
        let config = ExclusionConfig {
            enabled: true,
            active_packs: vec![],
            // Pack lists '.DLL' uppercase; file is '.dll' lower.
            custom_extensions: vec![".DLL".into()],
            custom_patterns: vec![],
        };
        let policy = ExclusionPolicy::compile(&config, &empty_presets()).unwrap();
        assert!(matches!(
            policy.evaluate(Path::new("a/b/foo.dll")),
            Decision::Excluded(ExclusionReason::CustomExtension)
        ));
        assert!(matches!(
            policy.evaluate(Path::new("a/b/FOO.DLL")),
            Decision::Excluded(ExclusionReason::CustomExtension)
        ));
    }

    #[test]
    fn extension_normalisation_strips_leading_dot() {
        // User can write '.dll' or 'dll' — both must match.
        for ext in [".dll", "dll", ".DLL", "DLL"] {
            let config = ExclusionConfig {
                enabled: true,
                active_packs: vec![],
                custom_extensions: vec![ext.into()],
                custom_patterns: vec![],
            };
            let policy = ExclusionPolicy::compile(&config, &empty_presets()).unwrap();
            assert!(
                matches!(
                    policy.evaluate(Path::new("a.dll")),
                    Decision::Excluded(ExclusionReason::CustomExtension)
                ),
                "extension form {ext:?} should match a.dll"
            );
        }
    }

    #[test]
    fn custom_path_pattern_hit_excludes() {
        let config = ExclusionConfig {
            enabled: true,
            active_packs: vec![],
            custom_extensions: vec![],
            custom_patterns: vec!["**/node_modules/**".into()],
        };
        let policy = ExclusionPolicy::compile(&config, &empty_presets()).unwrap();
        assert_eq!(
            policy.evaluate(Path::new("project/node_modules/foo/bar.js")),
            Decision::Excluded(ExclusionReason::CustomPattern)
        );
        // Same depth but a different dir doesn't trigger.
        assert_eq!(
            policy.evaluate(Path::new("project/src/foo.js")),
            Decision::Included
        );
    }

    #[test]
    fn path_match_takes_precedence_over_extension() {
        // Spec §2.4: path match runs first; extension second.
        // A .dll inside node_modules counts as path-match, not
        // ext-match — so reason should be CustomPattern.
        let config = ExclusionConfig {
            enabled: true,
            active_packs: vec![],
            custom_extensions: vec![".dll".into()],
            custom_patterns: vec!["**/node_modules/**".into()],
        };
        let policy = ExclusionPolicy::compile(&config, &empty_presets()).unwrap();
        assert_eq!(
            policy.evaluate(Path::new("project/node_modules/foo.dll")),
            Decision::Excluded(ExclusionReason::CustomPattern)
        );
    }

    #[test]
    fn included_when_nothing_matches() {
        let config = ExclusionConfig {
            enabled: true,
            active_packs: vec![],
            custom_extensions: vec![".dll".into()],
            custom_patterns: vec!["**/node_modules/**".into()],
        };
        let policy = ExclusionPolicy::compile(&config, &empty_presets()).unwrap();
        assert_eq!(
            policy.evaluate(Path::new("user/Documents/photo.jpg")),
            Decision::Included
        );
    }

    #[test]
    fn extensionless_file_included_when_only_ext_rules() {
        let config = ExclusionConfig {
            enabled: true,
            active_packs: vec![],
            custom_extensions: vec![".dll".into()],
            custom_patterns: vec![],
        };
        let policy = ExclusionPolicy::compile(&config, &empty_presets()).unwrap();
        assert_eq!(
            policy.evaluate(Path::new("user/Documents/Makefile")),
            Decision::Included
        );
    }

    #[test]
    fn bad_pattern_surfaces_as_config_error() {
        let config = ExclusionConfig {
            enabled: true,
            active_packs: vec![],
            custom_extensions: vec![],
            custom_patterns: vec!["[invalid".into()],
        };
        let r = ExclusionPolicy::compile(&config, &empty_presets());
        assert!(
            r.is_err(),
            "malformed glob must surface as ExclusionConfigError::BadPattern"
        );
        match r.unwrap_err() {
            ExclusionConfigError::BadPattern { pattern, .. } => {
                assert_eq!(pattern, "[invalid");
            }
            other => panic!("expected BadPattern, got {other:?}"),
        }
    }

    #[test]
    fn preset_pack_id_all_includes_all_eight_packs() {
        // Locks the pack count to 8 per spec §2.2. Adding a 9th
        // pack must update the spec + the user config schema.
        assert_eq!(PresetPackId::ALL.len(), 8);
    }

    #[test]
    fn preset_pack_id_labels_are_unique() {
        // No two packs share the same label — Settings UI keys
        // off these. A collision would break the user mental model.
        let labels: std::collections::HashSet<&str> =
            PresetPackId::ALL.iter().map(|id| id.label()).collect();
        assert_eq!(labels.len(), PresetPackId::ALL.len());
    }

    // Sanity: PathBuf-based call site (mimics walker hot-path
    // which holds owned paths). Confirms we accept &Path generically.
    #[test]
    fn evaluate_accepts_pathbuf_via_borrow() {
        let config = ExclusionConfig {
            enabled: true,
            active_packs: vec![],
            custom_extensions: vec![".dll".into()],
            custom_patterns: vec![],
        };
        let policy = ExclusionPolicy::compile(&config, &empty_presets()).unwrap();
        let p = PathBuf::from("a/b/c.dll");
        assert!(matches!(
            policy.evaluate(&p),
            Decision::Excluded(ExclusionReason::CustomExtension)
        ));
    }
}
