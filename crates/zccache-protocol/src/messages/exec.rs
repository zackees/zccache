//! Generic tool execution protocol payloads.

use serde::{Deserialize, Serialize};
/// Which streams a `GenericToolExec` should capture and replay.
///
/// Default is `{ stdout: true, stderr: true }`. The CLI exposes
/// `--output-stdout` / `--output-stderr` to override.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecOutputStreams {
    /// Capture and cache stdout.
    pub stdout: bool,
    /// Capture and cache stderr.
    pub stderr: bool,
}

impl Default for ExecOutputStreams {
    fn default() -> Self {
        Self {
            stdout: true,
            stderr: true,
        }
    }
}

/// Cache lookup/store policy for `GenericToolExec`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ExecCachePolicy {
    /// Look up cache; on miss, run the tool and store the result. (default)
    #[default]
    Normal,
    /// Never consult the cache and never store the result; passthrough only.
    /// Intended for debugging and `--no-cache`.
    Bypass,
    /// Look up cache; on miss, run the tool but do NOT store the result.
    /// Lets callers verify the tool runs deterministically without polluting
    /// the cache.
    ReadOnly,
}
