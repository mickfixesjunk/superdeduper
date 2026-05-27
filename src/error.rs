//! Crate-wide error type. Everything that can fail funnels through [`Error`].

use std::path::PathBuf;

use thiserror::Error;

/// Result alias used throughout the crate.
pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("path not found: {0}")]
    PathNotFound(PathBuf),

    #[error("path is not on a supported volume: {0}")]
    UnsupportedVolume(PathBuf),

    #[error("MFT enumeration failed on volume {volume}: {source}")]
    MftEnum {
        volume: String,
        #[source]
        source: std::io::Error,
    },

    #[error("retrieval-pointers query failed for {path}: {source}")]
    RetrievalPointers {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("USN journal read failed on volume {volume}: {source}")]
    UsnJournal {
        volume: String,
        #[source]
        source: std::io::Error,
    },

    #[error("cache database error: {0}")]
    Cache(#[from] rusqlite::Error),

    #[error("invalid glob pattern `{pattern}`: {source}")]
    BadGlob {
        pattern: String,
        #[source]
        source: globset::Error,
    },

    #[error("invalid size specifier `{0}`")]
    BadSize(String),

    #[error("operation not supported on this platform: {0}")]
    Unsupported(&'static str),

    /// #136 — Required environment variable missing. Callers
    /// matching this variant can branch on `var_name` without
    /// having to parse a format string.
    #[error("required environment variable not set: {0}")]
    EnvVarMissing(&'static str),

    /// #136 — User-facing config invariant violated (CLI arg, scan
    /// settings, exclusion-config compile). `field` names the input
    /// that drove the rejection; `reason` is the human message.
    /// Matching on this variant alone is enough for callers to
    /// distinguish config errors from I/O / cache / platform errors.
    #[error("invalid configuration for `{field}`: {reason}")]
    ConfigInvalid { field: &'static str, reason: String },

    /// #136 — JSON round-trip failure (encode or decode). Wraps
    /// `serde_json::Error` so callers can downcast for the underlying
    /// line/column/category instead of parsing the message.
    #[error("JSON serialization error: {0}")]
    Serde(#[from] serde_json::Error),

    #[error("{0}")]
    Other(String),
}

impl Error {
    pub fn other(msg: impl Into<String>) -> Self {
        Error::Other(msg.into())
    }
}
