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

    #[error("{0}")]
    Other(String),
}

impl Error {
    pub fn other(msg: impl Into<String>) -> Self {
        Error::Other(msg.into())
    }
}
