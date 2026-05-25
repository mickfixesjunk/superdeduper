//! TOML-serialisable user-facing configuration for exclusions.
//!
//! Stored as a `[exclusions]` section in sd's main config file (or
//! mirrored into the persisted project bundle for project-local
//! overrides). Round-trip stable: deserialise → mutate → serialise
//! produces deterministic output (active_packs ordered by
//! [`super::PresetPackId::ALL`]).
//!
//! #81 — Default `ExclusionConfig::default()` now ships safe-defaults
//! ON: master toggle enabled + 4 footgun-class packs pre-checked
//! (SystemLibraries, VcsInternals, OsSystemTrees, AvSignatureDatabases).
//! Mick directive 2026-05-25 after #80's archive orphan-copy leak —
//! preventing protected files from entering the dupe table is the
//! preventive complement to #80's defensive fix.
//!
//! For users who were on a pre-#81 build, [`ExclusionConfig::is_pristine_default_off`]
//! lets the migration on launch distinguish "never touched
//! exclusions" from "explicitly turned them off" so we only flip
//! safe-defaults for the former.

use serde::{Deserialize, Serialize};

use super::PresetPackId;

/// User-editable exclusion settings. Persists to TOML; the GUI's
/// Settings → Exclusions tab + the CLI's `--exclude-*` flags both
/// mutate instances of this type.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExclusionConfig {
    /// Master toggle. When false, every other field is inert.
    /// Spec §2.5: Settings tab top-level toggle.
    /// #81: defaults to `true` for new installs.
    #[serde(default = "default_enabled")]
    pub enabled: bool,

    /// Preset packs the user has activated. Order matches
    /// [`PresetPackId::ALL`] when serialised for stability.
    /// #81: defaults to [`PresetPackId::SAFE_DEFAULTS`] for new installs.
    #[serde(default = "default_active_packs")]
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

fn default_enabled() -> bool {
    true
}

fn default_active_packs() -> Vec<PresetPackId> {
    PresetPackId::SAFE_DEFAULTS.to_vec()
}

impl Default for ExclusionConfig {
    /// #81 safe-defaults shape for new installs. Master toggle ON;
    /// the 4 footgun packs pre-checked; custom lists empty.
    fn default() -> Self {
        Self {
            enabled: default_enabled(),
            active_packs: default_active_packs(),
            custom_extensions: Vec::new(),
            custom_patterns: Vec::new(),
        }
    }
}

impl ExclusionConfig {
    /// #81 — the pre-safe-defaults shape an old install would have
    /// on disk if the user never touched exclusions (master OFF +
    /// every list empty). Used by the upgrade-migration path to
    /// detect "user hasn't engaged with this feature yet" so we can
    /// silently flip them to safe-defaults without overriding
    /// anyone who explicitly disabled exclusions.
    pub fn is_pristine_pre_safe_defaults(&self) -> bool {
        !self.enabled
            && self.active_packs.is_empty()
            && self.custom_extensions.is_empty()
            && self.custom_patterns.is_empty()
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
    fn default_is_safe_defaults_on() {
        // #81: First-launch state for v0.2.7+: master toggle ON +
        // 4 footgun-class packs pre-checked. Preventive complement
        // to #80's archive-orphan-copy fix.
        let c = ExclusionConfig::default();
        assert!(c.enabled);
        assert_eq!(c.active_packs, PresetPackId::SAFE_DEFAULTS.to_vec());
        assert!(c.custom_extensions.is_empty());
        assert!(c.custom_patterns.is_empty());
    }

    #[test]
    fn pristine_pre_safe_defaults_detects_old_unmodified_config() {
        // The migration path needs to distinguish "user has never
        // touched exclusions" (we silently flip to safe-defaults)
        // from "user explicitly disabled exclusions" (we leave them
        // alone). The old default-off state is the pristine signal.
        let pristine = ExclusionConfig {
            enabled: false,
            active_packs: vec![],
            custom_extensions: vec![],
            custom_patterns: vec![],
        };
        assert!(pristine.is_pristine_pre_safe_defaults());

        // Any departure from the pristine shape means the user has
        // engaged with the config — don't override their choices.
        let touched_custom = ExclusionConfig {
            enabled: false,
            active_packs: vec![],
            custom_extensions: vec![".tmp".into()],
            custom_patterns: vec![],
        };
        assert!(!touched_custom.is_pristine_pre_safe_defaults());

        // The new safe-defaults shape is also NOT pristine —
        // running migration twice should be a no-op.
        assert!(!ExclusionConfig::default().is_pristine_pre_safe_defaults());
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
    fn deserialises_with_missing_fields_uses_safe_defaults() {
        // #81: old configs that omit fields now get the safe-
        // defaults shape from serde(default = "...") rather than
        // the pre-#81 empty/false state. A v0.2.6 config of just
        // `enabled = true` loads with the SAFE_DEFAULTS packs
        // active because the missing `active_packs` field falls
        // back to default_active_packs().
        let toml_str = "enabled = true";
        let parsed: ExclusionConfig = toml::from_str(toml_str).unwrap();
        assert!(parsed.enabled);
        assert_eq!(parsed.active_packs, PresetPackId::SAFE_DEFAULTS.to_vec());
        assert!(parsed.custom_extensions.is_empty());
        assert!(parsed.custom_patterns.is_empty());
    }

    #[test]
    fn deserialises_empty_toml_to_safe_defaults() {
        // A config file with NO exclusion section at all should
        // also land on safe-defaults (every field omitted →
        // serde(default) per field → all four defaults fire).
        let parsed: ExclusionConfig = toml::from_str("").unwrap();
        assert!(parsed.enabled);
        assert_eq!(parsed.active_packs, PresetPackId::SAFE_DEFAULTS.to_vec());
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
