//! Built-in preset-pack data — 15 extensions + 92 path patterns
//! across 8 categories. Drafted at `docs/exclusions-preset-content-draft.md`
//! and approved by design 2026-05-24 (channel `design-superdeduper.md`).
//!
//! Stable: edits must preserve serialisation compatibility for
//! [`super::PresetPackId`]. Adding new packs appends a new variant +
//! a new const array pair; existing user configs continue to parse.
//!
//! All entries are static. Extensions are stored without a leading
//! dot (normalised against [`super::normalize_extension`] at runtime).
//! Path patterns use forward slashes for cross-platform portability;
//! the Windows-specific patterns use backslashes verbatim to match
//! the `C:\Windows\...` shape on Windows builds.
//!
//! Note on `target/` (Pack 2): design reviewed + approved inclusion.
//! It's a regenerable cache; dedup workflow rarely targets actively-
//! building projects; user can opt out via Settings if needed.

#![allow(clippy::needless_raw_string_hashes)]

use super::{PresetPack, PresetPackId, PresetSource};

// =====================================================================
// Pack 1: System libraries
// =====================================================================
const SYSTEM_LIBRARIES_EXTS: &[&str] = &[
    "dll", "so", "dylib", "pyd", "class", "jar", "pak", "asar", "mui", "ocx", "drv", "sys", "lib",
    "a", "node",
];
const SYSTEM_LIBRARIES_PATHS: &[&str] = &[];

// =====================================================================
// Pack 2: Build artefacts
// =====================================================================
const BUILD_ARTEFACTS_EXTS: &[&str] = &[];
const BUILD_ARTEFACTS_PATHS: &[&str] = &[
    "**/node_modules/**",
    "**/__pycache__/**",
    "**/.venv/**",
    "**/venv/**",
    "**/.virtualenv/**",
    "**/target/**",
    "**/build/**",
    "**/dist/**",
    "**/out/**",
    "**/.next/**",
    "**/.nuxt/**",
    "**/.svelte-kit/**",
    "**/.angular/cache/**",
    "**/.gradle/**",
    "**/.idea/build/**",
    "**/cmake-build-*/**",
];

// =====================================================================
// Pack 3: VCS internals
// =====================================================================
const VCS_INTERNALS_EXTS: &[&str] = &[];
const VCS_INTERNALS_PATHS: &[&str] = &[
    "**/.git/objects/**",
    "**/.git/lfs/**",
    "**/.git/refs/**",
    "**/.git/pack/**",
    "**/.svn/**",
    "**/.hg/**",
    "**/.bzr/**",
    "**/CVS/**",
];

// =====================================================================
// Pack 4: Package manager caches
// =====================================================================
const PACKAGE_MANAGER_CACHES_EXTS: &[&str] = &[];
const PACKAGE_MANAGER_CACHES_PATHS: &[&str] = &[
    "**/.m2/repository/**",
    "**/.gradle/caches/**",
    "**/.cargo/registry/**",
    "**/.cargo/git/**",
    "**/.npm/_cacache/**",
    "**/.npm/_logs/**",
    "**/.yarn/cache/**",
    "**/.pnpm-store/**",
    "**/.bundle/cache/**",
    "**/.composer/cache/**",
    "**/.cache/pip/**",
    "**/.cache/uv/**",
    "**/Library/Caches/Homebrew/**",
];

// =====================================================================
// Pack 5: OS system trees
// =====================================================================
const OS_SYSTEM_TREES_EXTS: &[&str] = &[];
const OS_SYSTEM_TREES_PATHS: &[&str] = &[
    // Patterns stored with forward slashes (cross-platform glob
    // convention). The walker's evaluate() path normalises `\` to
    // `/` before matching, so these match both Windows-shell
    // paths ("C:\\Windows\\WinSxS\\...") and Linux-style paths.
    "C:/Windows/WinSxS/**",
    "C:/Windows/System32/**",
    "C:/Windows/SysWOW64/**",
    "C:/Windows/Installer/**",
    "C:/Windows/servicing/**",
    "**/AppData/Local/Microsoft/Windows/Caches/**",
    "**/AppData/Local/Microsoft/Edge/User Data/Default/Cache/**",
    "/usr/lib/**",
    "/usr/lib32/**",
    "/usr/lib64/**",
    "/usr/share/locale/**",
    "/var/lib/dpkg/info/**",
    "**/.local/share/Trash/**",
];

// =====================================================================
// Pack 6: Browser caches
// =====================================================================
const BROWSER_CACHES_EXTS: &[&str] = &[];
const BROWSER_CACHES_PATHS: &[&str] = &[
    "**/Cache_Data/**",
    "**/cache2/**",
    "**/Service Worker/**",
    "**/IndexedDB/**",
    "**/Local Storage/leveldb/**",
    "**/Session Storage/**",
    "**/Code Cache/**",
    "**/GPUCache/**",
    "**/Cache/Cache_Data/**",
    "**/Profiles/*/cache2/**",
    "**/Profile */Cache/**",
    "**/Default/Cache/**",
    "**/.mozilla/firefox/*/cache2/**",
    "**/Library/Caches/com.apple.Safari/**",
];

// =====================================================================
// Pack 7: App-specific caches
// =====================================================================
const APP_CACHES_EXTS: &[&str] = &[];
const APP_CACHES_PATHS: &[&str] = &[
    "**/Steam/steamapps/shadercache/**",
    "**/Steam/steamapps/downloading/**",
    "**/Steam/steamapps/temp/**",
    "**/Spotify/Storage/**",
    "**/Discord/Cache/**",
    "**/Discord/Code Cache/**",
    "**/Discord/GPUCache/**",
    "**/Adobe/Common/Media Cache/**",
    "**/Adobe/Common/Media Cache Files/**",
    "**/Lightroom Catalog Previews.lrdata/**",
    "**/Office/Containers/Office365ServiceV2/**",
    "**/Code/Cache/**",
    "**/Code/CachedData/**",
    "**/JetBrains/*/caches/**",
    "**/JetBrains/*/log/**",
    "**/Slack/Cache/**",
    "**/Postman/Cache/**",
    "**/zoom/data/**",
];

// =====================================================================
// Pack 8: AV signature databases
// =====================================================================
const AV_SIGNATURE_DATABASES_EXTS: &[&str] = &[];
const AV_SIGNATURE_DATABASES_PATHS: &[&str] = &[
    // Forward-slash form (see Pack 5 comment).
    "C:/ProgramData/Microsoft/Windows Defender/Definition Updates/**",
    "C:/ProgramData/Microsoft/Windows Defender/Scans/**",
    "**/AppData/Local/Microsoft/Windows Defender Scans/**",
    "C:/Program Files (x86)/Norton/**/NDB/**",
    "C:/Program Files/Bitdefender/**/Antivirus/**",
    "C:/Program Files/Common Files/McAfee/**",
    "C:/Program Files/ESET/**/Modules/**",
    "C:/Program Files/Avast/**/setup/**",
    "C:/Program Files/AVG/**/setup/**",
    "**/clamav/database/**",
];

/// Production [`PresetSource`] implementation used by everything
/// outside tests. Maps each [`PresetPackId`] to the const arrays
/// defined above. Cheap to construct (zero-sized) so callers can
/// pass `&BuiltinPresets` wherever a `&dyn PresetSource` is needed.
#[derive(Debug, Clone, Copy, Default)]
pub struct BuiltinPresets;

impl PresetSource for BuiltinPresets {
    fn get(&self, id: PresetPackId) -> PresetPack {
        match id {
            PresetPackId::SystemLibraries => PresetPack {
                extensions: SYSTEM_LIBRARIES_EXTS,
                paths: SYSTEM_LIBRARIES_PATHS,
            },
            PresetPackId::BuildArtefacts => PresetPack {
                extensions: BUILD_ARTEFACTS_EXTS,
                paths: BUILD_ARTEFACTS_PATHS,
            },
            PresetPackId::VcsInternals => PresetPack {
                extensions: VCS_INTERNALS_EXTS,
                paths: VCS_INTERNALS_PATHS,
            },
            PresetPackId::PackageManagerCaches => PresetPack {
                extensions: PACKAGE_MANAGER_CACHES_EXTS,
                paths: PACKAGE_MANAGER_CACHES_PATHS,
            },
            PresetPackId::OsSystemTrees => PresetPack {
                extensions: OS_SYSTEM_TREES_EXTS,
                paths: OS_SYSTEM_TREES_PATHS,
            },
            PresetPackId::BrowserCaches => PresetPack {
                extensions: BROWSER_CACHES_EXTS,
                paths: BROWSER_CACHES_PATHS,
            },
            PresetPackId::AppCaches => PresetPack {
                extensions: APP_CACHES_EXTS,
                paths: APP_CACHES_PATHS,
            },
            PresetPackId::AvSignatureDatabases => PresetPack {
                extensions: AV_SIGNATURE_DATABASES_EXTS,
                paths: AV_SIGNATURE_DATABASES_PATHS,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    //! Acceptance shape: the right counts per pack, no rule
    //! duplication within a pack, every PresetPackId has data
    //! reachable through BuiltinPresets, and (most importantly)
    //! the patterns actually compile through globset without
    //! errors at runtime.

    use super::*;
    use crate::exclusions::{ExclusionConfig, ExclusionPolicy, PresetPackId};

    #[test]
    fn pack_1_system_libraries_has_15_extensions() {
        assert_eq!(SYSTEM_LIBRARIES_EXTS.len(), 15);
        assert_eq!(SYSTEM_LIBRARIES_PATHS.len(), 0);
    }

    #[test]
    fn pack_2_build_artefacts_has_16_path_patterns() {
        assert_eq!(BUILD_ARTEFACTS_EXTS.len(), 0);
        assert_eq!(BUILD_ARTEFACTS_PATHS.len(), 16);
    }

    #[test]
    fn pack_3_vcs_internals_has_8_path_patterns() {
        assert_eq!(VCS_INTERNALS_PATHS.len(), 8);
    }

    #[test]
    fn pack_4_pkg_caches_has_13_path_patterns_post_trash_move() {
        // Trash moved to Pack 5 per design review. Counter dropped
        // from 14 → 13.
        assert_eq!(PACKAGE_MANAGER_CACHES_PATHS.len(), 13);
        assert!(
            !PACKAGE_MANAGER_CACHES_PATHS
                .iter()
                .any(|p| p.contains("Trash")),
            "Trash should be in OS system trees, not PM caches"
        );
    }

    #[test]
    fn pack_5_os_trees_has_13_path_patterns_post_trash_move() {
        assert_eq!(OS_SYSTEM_TREES_PATHS.len(), 13);
        assert!(
            OS_SYSTEM_TREES_PATHS
                .iter()
                .any(|p| p.contains("Trash")),
            "Trash should be in OS system trees per design review"
        );
    }

    #[test]
    fn pack_6_browser_caches_has_14_path_patterns() {
        assert_eq!(BROWSER_CACHES_PATHS.len(), 14);
    }

    #[test]
    fn pack_7_app_caches_has_18_path_patterns() {
        assert_eq!(APP_CACHES_PATHS.len(), 18);
    }

    #[test]
    fn pack_8_av_dbs_has_10_path_patterns() {
        assert_eq!(AV_SIGNATURE_DATABASES_PATHS.len(), 10);
    }

    #[test]
    fn total_patterns_match_draft_doc() {
        // Doc says 15 extensions + 92 path patterns. Lock that
        // count so accidental edits to the const arrays surface
        // here before the spec drifts.
        let total_exts: usize = [
            SYSTEM_LIBRARIES_EXTS,
            BUILD_ARTEFACTS_EXTS,
            VCS_INTERNALS_EXTS,
            PACKAGE_MANAGER_CACHES_EXTS,
            OS_SYSTEM_TREES_EXTS,
            BROWSER_CACHES_EXTS,
            APP_CACHES_EXTS,
            AV_SIGNATURE_DATABASES_EXTS,
        ]
        .iter()
        .map(|a| a.len())
        .sum();
        let total_paths: usize = [
            SYSTEM_LIBRARIES_PATHS,
            BUILD_ARTEFACTS_PATHS,
            VCS_INTERNALS_PATHS,
            PACKAGE_MANAGER_CACHES_PATHS,
            OS_SYSTEM_TREES_PATHS,
            BROWSER_CACHES_PATHS,
            APP_CACHES_PATHS,
            AV_SIGNATURE_DATABASES_PATHS,
        ]
        .iter()
        .map(|a| a.len())
        .sum();
        assert_eq!(total_exts, 15, "matches docs/exclusions-preset-content-draft.md header");
        assert_eq!(total_paths, 92, "matches docs/exclusions-preset-content-draft.md header");
    }

    #[test]
    fn all_pack_ids_reachable_via_builtin_source() {
        // Every variant of PresetPackId must produce a real
        // (possibly-empty) pack. Missing one would mean
        // ExclusionPolicy::compile silently treats it as empty.
        let p = BuiltinPresets;
        for &id in &PresetPackId::ALL {
            let pack = p.get(id);
            // Each pack has SOME content (either exts or paths,
            // possibly both). No pack should be silently empty
            // until we decide to skip it.
            assert!(
                !pack.extensions.is_empty() || !pack.paths.is_empty(),
                "pack {id:?} has no content; should at least be removed from PresetPackId::ALL"
            );
        }
    }

    #[test]
    fn all_packs_compile_through_globset() {
        // The acceptance test: every preset pack's path patterns
        // build into a working GlobSet without errors. If a
        // pattern is malformed, ExclusionPolicy::compile would
        // fail at scan time — better to catch here at build time.
        for &id in &PresetPackId::ALL {
            let config = ExclusionConfig {
                enabled: true,
                active_packs: vec![id],
                custom_extensions: vec![],
                custom_patterns: vec![],
            };
            let result = ExclusionPolicy::compile(&config, &BuiltinPresets);
            assert!(
                result.is_ok(),
                "preset pack {id:?} failed to compile: {:?}",
                result.err()
            );
        }
    }

    #[test]
    fn all_packs_combined_compile() {
        // Compiling ALL 8 packs together should also work — the
        // globset combiner deduplicates internally so this is
        // also a bounds check that we don't trip any limits.
        let config = ExclusionConfig {
            enabled: true,
            active_packs: PresetPackId::ALL.to_vec(),
            custom_extensions: vec![],
            custom_patterns: vec![],
        };
        let result = ExclusionPolicy::compile(&config, &BuiltinPresets);
        assert!(result.is_ok());
    }

    #[test]
    fn system_libraries_pack_excludes_dlls_when_enabled() {
        let config = ExclusionConfig {
            enabled: true,
            active_packs: vec![PresetPackId::SystemLibraries],
            custom_extensions: vec![],
            custom_patterns: vec![],
        };
        let policy = ExclusionPolicy::compile(&config, &BuiltinPresets).unwrap();
        // `.dll` → excluded (matches the System libraries ext list)
        assert!(matches!(
            policy.evaluate(std::path::Path::new("system32/kernel32.dll")),
            crate::exclusions::Decision::Excluded(_)
        ));
        // `.txt` → included (not in any pack)
        assert_eq!(
            policy.evaluate(std::path::Path::new("user/notes.txt")),
            crate::exclusions::Decision::Included
        );
    }

    #[test]
    fn build_artefacts_pack_excludes_node_modules_paths() {
        let config = ExclusionConfig {
            enabled: true,
            active_packs: vec![PresetPackId::BuildArtefacts],
            custom_extensions: vec![],
            custom_patterns: vec![],
        };
        let policy = ExclusionPolicy::compile(&config, &BuiltinPresets).unwrap();
        assert!(matches!(
            policy.evaluate(std::path::Path::new("project/node_modules/foo/index.js")),
            crate::exclusions::Decision::Excluded(_)
        ));
        assert!(matches!(
            policy.evaluate(std::path::Path::new("project/target/release/binary")),
            crate::exclusions::Decision::Excluded(_)
        ));
        // Regular project files included
        assert_eq!(
            policy.evaluate(std::path::Path::new("project/src/main.rs")),
            crate::exclusions::Decision::Included
        );
    }

    #[test]
    fn vcs_internals_excludes_git_objects_not_user_git_dirs() {
        // §"No false-positive cascade" review criterion: VCS pack
        // must NOT catch the whole `.git/` tree — only its
        // content-addressable subdirs. Hooks, configs, refs, etc.
        // remain visible.
        let config = ExclusionConfig {
            enabled: true,
            active_packs: vec![PresetPackId::VcsInternals],
            custom_extensions: vec![],
            custom_patterns: vec![],
        };
        let policy = ExclusionPolicy::compile(&config, &BuiltinPresets).unwrap();
        // Excluded: known content-addressable subdirs
        assert!(matches!(
            policy.evaluate(std::path::Path::new("project/.git/objects/ab/cdef123")),
            crate::exclusions::Decision::Excluded(_)
        ));
        // Included: things outside the narrowed subdirs (hooks, config)
        assert_eq!(
            policy.evaluate(std::path::Path::new("project/.git/hooks/pre-commit")),
            crate::exclusions::Decision::Included
        );
        assert_eq!(
            policy.evaluate(std::path::Path::new("project/.git/config")),
            crate::exclusions::Decision::Included
        );
    }

    // ============================================================
    // Follow-up tests per design review (Q3 + Pack 5 backslash)
    // ============================================================

    #[test]
    fn globset_single_star_does_not_cross_path_separators() {
        // Design Q3: locks the behaviour for the Firefox cache
        // pattern (`**/.mozilla/firefox/*/cache2/**`). With
        // `literal_separator(true)` (which `ExclusionPolicy::compile`
        // sets), globset's single `*` matches a single path
        // component only — does NOT span separators. `**`
        // continues to match across separators. Without that
        // setting the test would fail (and the Firefox pattern
        // would catch too broadly).
        let pattern = "**/.mozilla/firefox/*/cache2/**";
        let glob = globset::GlobBuilder::new(pattern)
            .literal_separator(true)
            .build()
            .unwrap()
            .compile_matcher();

        // Matches: standard Firefox profile dir naming.
        assert!(glob.is_match("home/user/.mozilla/firefox/abc123.default-release/cache2/file.bin"));
        // Doesn't match: two-deep profile path (no Firefox does
        // this in practice; but if globset broke this we'd want
        // to know).
        assert!(!glob.is_match("home/user/.mozilla/firefox/foo/bar/cache2/file.bin"));
        // Doesn't match: anything outside the `cache2/` subdir.
        assert!(!glob.is_match("home/user/.mozilla/firefox/abc.default/places.sqlite"));
    }

    #[test]
    fn globset_handles_backslash_paths_via_normalisation() {
        // Design's Pack 5 review: confirm Windows-style backslash
        // input paths still match against the forward-slash
        // patterns we store in const data. The walker's
        // `ExclusionPolicy::evaluate` does the `\` → `/`
        // normalisation; this test verifies the normalisation
        // logic by going end-to-end through evaluate(), since
        // raw globset cannot reconcile backslash paths against
        // forward-slash patterns.
        let config = ExclusionConfig {
            enabled: true,
            active_packs: vec![PresetPackId::OsSystemTrees],
            custom_extensions: vec![],
            custom_patterns: vec![],
        };
        let policy = ExclusionPolicy::compile(&config, &BuiltinPresets).unwrap();

        // Windows-shell-style backslash input must hit the
        // C:/Windows/WinSxS/** pattern after normalisation.
        let path = std::path::Path::new(r"C:\Windows\WinSxS\amd64_microsoft-windows-foo");
        assert!(matches!(
            policy.evaluate(path),
            crate::exclusions::Decision::Excluded(_)
        ));

        // Forward-slash equivalent must also match (the
        // normalisation is idempotent).
        let path = std::path::Path::new("C:/Windows/WinSxS/amd64_microsoft-windows-foo");
        assert!(matches!(
            policy.evaluate(path),
            crate::exclusions::Decision::Excluded(_)
        ));
    }

    #[test]
    fn os_system_trees_pack_matches_winsxs_path() {
        // End-to-end check: WinSxS path entering the walker hot
        // path with OS system trees pack enabled should be
        // excluded (combined GlobSet test, not raw globset).
        let config = ExclusionConfig {
            enabled: true,
            active_packs: vec![PresetPackId::OsSystemTrees],
            custom_extensions: vec![],
            custom_patterns: vec![],
        };
        let policy = ExclusionPolicy::compile(&config, &BuiltinPresets).unwrap();
        let path = std::path::Path::new(r"C:\Windows\WinSxS\amd64_microsoft-windows-foo");
        assert!(matches!(
            policy.evaluate(path),
            crate::exclusions::Decision::Excluded(_)
        ));
    }

    #[test]
    fn os_system_trees_pack_matches_linux_trash() {
        // Trash moved to Pack 5 per design review. Confirm it
        // matches the Linux XDG trash path.
        let config = ExclusionConfig {
            enabled: true,
            active_packs: vec![PresetPackId::OsSystemTrees],
            custom_extensions: vec![],
            custom_patterns: vec![],
        };
        let policy = ExclusionPolicy::compile(&config, &BuiltinPresets).unwrap();
        assert!(matches!(
            policy.evaluate(std::path::Path::new("home/mick/.local/share/Trash/files/old.pdf")),
            crate::exclusions::Decision::Excluded(_)
        ));
    }
}
