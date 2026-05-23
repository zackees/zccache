//! Error types for zccache.

use super::NormalizedPath;

/// Top-level error type for zccache operations.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("IPC error: {message}")]
    Ipc { message: String },

    #[error("protocol error: {message}")]
    Protocol { message: String },

    #[error("cache error: {message}")]
    Cache { message: String },

    #[error("file not found: {0}")]
    FileNotFound(NormalizedPath),

    #[error("daemon not running")]
    DaemonNotRunning,

    #[error("daemon already running")]
    DaemonAlreadyRunning,

    #[error("configuration error: {message}")]
    Config { message: String },
}

/// Convenience result type for zccache operations.
pub type Result<T> = std::result::Result<T, Error>;
