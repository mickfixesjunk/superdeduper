//! TOML-serialisable user-facing configuration for exclusions.
//!
//! Stored as a `[exclusions]` section in sd's main config file (or
//! mirrored into the persisted project bundle for project-local
//! overrides). Round-trip stable: deserialise → mutate → serialise
//! produces deterministic output (active_packs ordered by
//! [`super::PresetPackId::ALL`]).
//!
//! Default `ExclusionConfig::default()` matches the master toggle
//! OFF state: feature exists but no rules are applied. This is the
//! state sd starts in for first-time users.

use serde::{Deserialize, Serialize};

use super::PresetPackId;

/// User-editable exclusion settings. Persists to TOML; the GUI's
/// Settings → Exclusions tab + the CLI's `--exclude-*` flags both
/// mutate instances of this type.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExclusionConfig {
    /// Master toggle. When false, every other field is inert.
    /// Spec §2.5: Settings tab top-level toggle.
    #[serde(default)]
    pub enabled: bool,

    /// Preset packs the user has activated. Order matches
    /// [`PresetPackId::ALL`] when serialised for stability.
    #[serde(default)]
    pub active_packs: Vec<PresetPackId>,

    /// User-added extension exclusions (e.g. `.dll`, `dll`,
    /// `.PAK`). Leading dot optional; case-insensitive; stored
    /// verbatim in TOML and normalised at compile time.
    #[serde(default)]
    pub custom_extensions: Vec<String>,

    /// User-added path-pattern globs (e.g. `**/node_modules/**`).
    /// Validated at compile time; malformed patterns surface as
    /// [`ExclusionConfigError::BadPattern`] before the scan runs.
    #[serde(default)]
    pub custom_patterns: Vec<String>,
}

impl Default for ExclusionConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            active_packs: Vec::new(),
            custom_extensions: Vec::new(),
            custom_patterns: Vec::new(),
        }
    }
}

/// Errors raised when compiling an [`ExclusionConfig`] into a
/// runtime [`super::ExclusionPolicy`].
#[derive(Debug, thiserror::Error)]
pub enum ExclusionConfigError {
    /// One of the user-supplied path patterns isn't a valid glob.
    /// The `pattern` field carries the offending string verbatim
    /// so the GUI can highlight the bad row.
    #[error("invalid glob pattern {pattern:?}: {source}")]
    BadPattern {
        pattern: String,
        #[source]
        source: globset::Error,
    },

    /// `GlobSet::build` itself failed (rare; typically OOM or an
    /// internal globset bug). Stored as a String so we don't drag
    /// globset's internal error type into the public API.
    #[error("globset build failed: {0}")]
    BuildFailed(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_master_toggle_off() {
        // First-launch state: feature exists, all lists empty,
        // nothing applies. Matches Mick's "off by default" directive.
        let c = ExclusionConfig::default();
        assert!(!c.enabled);
        assert!(c.active_packs.is_empty());
        assert!(c.custom_extensions.is_empty());
        assert!(c.custom_patterns.is_empty());
    }

    #[test]
    fn toml_round_trip_default() {
        // Default should round-trip cleanly so users opening the
        // pretty-printed config see exactly what was saved.
        let original = ExclusionConfig::default();
        let serialised = toml::to_string(&original).unwrap();
        let parsed: ExclusionConfig = toml::from_str(&serialised).unwrap();
        assert_eq!(original, parsed);
    }

    #[test]
    fn toml_round_trip_with_full_payload() {
        let original = ExclusionConfig {
            enabled: true,
            active_packs: vec![
                PresetPackId::SystemLibraries,
                PresetPackId::BuildArtefacts,
                PresetPackId::VcsInternals,
            ],
            custom_extensions: vec![".dll".into(), ".so".into()],
            custom_patterns: vec!["**/node_modules/**".into(), "**/.cache/**".into()],
        };
        let serialised = toml::to_string(&original).unwrap();
        let parsed: ExclusionConfig = toml::from_str(&serialised).unwrap();
        assert_eq!(original, parsed);
    }

    #[test]
    fn deserialises_with_missing_fields_using_defaults() {
        // Backwards-compat: users with older configs that omit
        // newer fields should still load. serde(default) on each
        // field makes that work.
        let toml_str = "enabled = true";
        let parsed: ExclusionConfig = toml::from_str(toml_str).unwrap();
        assert!(parsed.enabled);
        assert!(parsed.active_packs.is_empty());
        assert!(parsed.custom_extensions.is_empty());
        assert!(parsed.custom_patterns.is_empty());
    }

    #[test]
    fn deserialises_kebab_case_pack_ids() {
        // Pack IDs serialise as kebab-case in TOML for readability.
        // User-editable files should look like
        // `active_packs = ["system-libraries", "build-artefacts"]`
        // not `["SystemLibraries", "BuildArtefacts"]`.
        let toml_str = r#"
            enabled = true
            active_packs = ["system-libraries", "build-artefacts"]
        "#;
        let parsed: ExclusionConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(
            parsed.active_packs,
            vec![PresetPackId::SystemLibraries, PresetPackId::BuildArtefacts]
        );
    }

    #[test]
    fn unknown_pack_id_fails_parse() {
        // Future engine versions may add packs; older engines
        // reading newer config should reject unknown IDs rather
        // than silently dropping (which would be confusing).
        let toml_str = r#"
            enabled = true
            active_packs = ["never-heard-of-this-pack"]
        "#;
        let result: Result<ExclusionConfig, _> = toml::from_str(toml_str);
        assert!(result.is_err());
    }
}
