//! Persistence-schema discipline — single trait for every store
//! that lands a versioned JSON / TOML / SQLite-row blob on disk.
//!
//! Pre-#92 the 11 persistence layers used two paradigms (10 ×
//! `superdeduper.foo.v1` strings, 1 × integer field) and three
//! load-side check styles (`starts_with` permissive, `==` strict,
//! no check at all). The dangerous one is permissive
//! `starts_with`: a future v2 that changes a field's *meaning*
//! parses as v1, silent data corruption masquerading as
//! forward-compat. This module locks the policy and gives every
//! new store a canonical pattern.
//!
//! ## Locked policy
//!
//! 1. **Schema is identified by `(SCHEMA_NAME, version)`** — a
//!    free-form string for the family + a `u32` for the version.
//!    A name change is a fresh family; a version bump preserves
//!    the family and signals a breakable schema change.
//! 2. **Exact match on load** — if the loaded record's name +
//!    version don't equal the store's `SCHEMA_NAME` +
//!    `current_version()`, the load fails fast with [`CheckError`].
//!    No silent forward-compat; migrations are explicit.
//! 3. **Explicit migration helpers** — when a store revs from
//!    v1 → v2, the store writes a `migrate_from_v1(record_v1) ->
//!    RecordV2` helper. The load pipeline tries
//!    `check::<RecordV2>()` first; on `UnsupportedVersion { got:
//!    1 }` it falls back to `check::<RecordV1>()` then
//!    `migrate_from_v1`. The check helper here returns the
//!    structured error so callers can pattern-match.
//!
//! ## What stays the same
//!
//! - `scan_history::ScanRecord::schema_version` (already `u32` +
//!   `>` comparison) — extend the winner; don't churn the field
//!   shape. Future migration: implement `SchemaVersioned` on top.
//! - egui's `superdeduper.app.v1` blob — egui owns its own
//!   versioning policy. Out of scope.
//! - Existing v1 stores stay v1 with their current load-side
//!   checks until they next rev. Mass migration would touch every
//!   on-disk file the user has; opportunistic migration on rev is
//!   the lighter-cost path.
//!
//! ## Canonical pattern
//!
//! ```ignore
//! use crate::schema::{check, CheckError, SchemaVersioned};
//!
//! #[derive(Serialize, Deserialize)]
//! pub struct DriveOverrides {
//!     pub schema: String,
//!     pub schema_version: u32,
//!     // ... real fields ...
//! }
//!
//! impl SchemaVersioned for DriveOverrides {
//!     const SCHEMA_NAME: &'static str = "superdeduper.drive-overrides";
//!     fn schema_version(&self) -> u32 { self.schema_version }
//!     fn current_version() -> u32 { 1 }
//! }
//!
//! pub fn load(path: &Path) -> Result<DriveOverrides, MyError> {
//!     let bytes = std::fs::read(path)?;
//!     let loaded: DriveOverrides = serde_json::from_slice(&bytes)?;
//!     crate::schema::check(&loaded).map_err(MyError::from)?;
//!     Ok(loaded)
//! }
//! ```

use std::fmt;

/// One persisted record family. Every on-disk schema declares its
/// name + version via this trait + lets [`check`] enforce the
/// equality policy.
pub trait SchemaVersioned {
    /// Stable family identifier — e.g. `"superdeduper.archive"`.
    /// Does NOT include the `.v1` suffix; the version lives in
    /// [`Self::schema_version`].
    const SCHEMA_NAME: &'static str;

    /// Name field on the loaded record. Returned verbatim so the
    /// check helper can compare against [`Self::SCHEMA_NAME`] +
    /// surface the mismatch in error messages.
    fn schema_name(&self) -> &str;

    /// Version field on the loaded record.
    fn schema_version(&self) -> u32;

    /// What this binary considers the current version. Bump on
    /// any schema change that would break older loaders. Bumping
    /// requires a `migrate_from_vN` helper on the store side.
    fn current_version() -> u32;
}

/// Why a [`check`] call refused the loaded record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckError {
    /// Loaded record's `schema_name()` doesn't match this binary's
    /// `SCHEMA_NAME` — wrong family entirely (renamed file path,
    /// stale fixture, user copied another tool's file in by hand).
    WrongSchema {
        expected: &'static str,
        got: String,
    },
    /// Right family, wrong version. `got` is what the file said;
    /// `current` is what this binary writes. `got > current` means
    /// the file is newer than this binary (downgrade case);
    /// `got < current` means the file is older than this binary
    /// (forward-load case — needs explicit migration).
    UnsupportedVersion {
        schema: &'static str,
        got: u32,
        current: u32,
    },
}

impl fmt::Display for CheckError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WrongSchema { expected, got } => {
                write!(f, "wrong schema family: expected `{expected}`, got `{got}`")
            }
            Self::UnsupportedVersion {
                schema,
                got,
                current,
            } => write!(
                f,
                "schema `{schema}` version {got} not supported by this binary \
                 (current = {current}); load via the store's explicit \
                 migrate_from_v{got} helper if available"
            ),
        }
    }
}

impl std::error::Error for CheckError {}

/// Enforce the locked policy: loaded record's
/// `(schema_name, schema_version)` must equal
/// `(S::SCHEMA_NAME, S::current_version())` exactly.
///
/// Returns [`CheckError::WrongSchema`] when the family differs and
/// [`CheckError::UnsupportedVersion`] when only the version
/// differs. Callers that support multi-version loads should
/// pattern-match on `UnsupportedVersion { got: N, .. }` and
/// dispatch to a `migrate_from_vN` helper.
pub fn check<S: SchemaVersioned>(loaded: &S) -> Result<(), CheckError> {
    if loaded.schema_name() != S::SCHEMA_NAME {
        return Err(CheckError::WrongSchema {
            expected: S::SCHEMA_NAME,
            got: loaded.schema_name().to_string(),
        });
    }
    if loaded.schema_version() != S::current_version() {
        return Err(CheckError::UnsupportedVersion {
            schema: S::SCHEMA_NAME,
            got: loaded.schema_version(),
            current: S::current_version(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        name: String,
        version: u32,
    }

    impl SchemaVersioned for Fixture {
        const SCHEMA_NAME: &'static str = "superdeduper.test-fixture";
        fn schema_name(&self) -> &str {
            &self.name
        }
        fn schema_version(&self) -> u32 {
            self.version
        }
        fn current_version() -> u32 {
            2
        }
    }

    #[test]
    fn check_accepts_matching_schema_and_version() {
        let f = Fixture {
            name: "superdeduper.test-fixture".into(),
            version: 2,
        };
        assert!(check(&f).is_ok());
    }

    #[test]
    fn check_rejects_wrong_schema_family() {
        let f = Fixture {
            name: "superdeduper.different-family".into(),
            version: 2,
        };
        assert_eq!(
            check(&f),
            Err(CheckError::WrongSchema {
                expected: "superdeduper.test-fixture",
                got: "superdeduper.different-family".to_string(),
            }),
        );
    }

    #[test]
    fn check_rejects_old_version() {
        let f = Fixture {
            name: "superdeduper.test-fixture".into(),
            version: 1,
        };
        assert_eq!(
            check(&f),
            Err(CheckError::UnsupportedVersion {
                schema: "superdeduper.test-fixture",
                got: 1,
                current: 2,
            }),
        );
    }

    #[test]
    fn check_rejects_newer_version_too() {
        // Same policy — exact match — so a file FROM the future
        // also fails. The error type lets the caller distinguish
        // future (got > current) vs past (got < current) and
        // either refuse-with-message or implement downgrade.
        let f = Fixture {
            name: "superdeduper.test-fixture".into(),
            version: 99,
        };
        match check(&f) {
            Err(CheckError::UnsupportedVersion {
                got: 99,
                current: 2,
                ..
            }) => {}
            other => panic!("expected UnsupportedVersion {{ got: 99, current: 2 }}, got {other:?}"),
        }
    }

    #[test]
    fn display_messages_carry_field_names() {
        let wrong = CheckError::WrongSchema {
            expected: "superdeduper.fixture",
            got: "other".into(),
        };
        let s = wrong.to_string();
        assert!(s.contains("superdeduper.fixture"));
        assert!(s.contains("other"));

        let unsupported = CheckError::UnsupportedVersion {
            schema: "superdeduper.fixture",
            got: 1,
            current: 3,
        };
        let s = unsupported.to_string();
        assert!(s.contains("superdeduper.fixture"));
        assert!(s.contains("version 1"));
        assert!(s.contains("current = 3"));
        assert!(s.contains("migrate_from_v1"));
    }
}
