//! Types backing the request-level fast path and cross-root validation cache.
//!
//! These records describe pre-computed compile-request data so repeated
//! invocations of the same request can skip arg parsing, dep-graph
//! registration, system-include discovery, and response-file expansion.

use super::*;

/// Pre-computed compile request data for the request-level fast path.
pub(super) struct RequestCacheEntry {
    pub(super) context_key: ContextKey,
    pub(super) root: Option<NormalizedPath>,
    pub(super) source_path: CachedRequestPath,
    pub(super) output_path: CachedRequestPath,
    pub(super) input_paths: Vec<CachedRequestPath>,
    pub(super) cross_root_shareable: bool,
    pub(super) cached_at: std::time::Instant,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) struct RequestValidationKey {
    pub(super) request_fp: ContentHash,
    pub(super) root: NormalizedPath,
}

pub(super) struct RequestValidationEntry {
    pub(super) artifact_key_hex: String,
    pub(super) clock: Clock,
    pub(super) cached_at: std::time::Instant,
}

pub(super) struct SessionWorktreeRoot {
    pub(super) working_dir: NormalizedPath,
    pub(super) root: Option<NormalizedPath>,
}

#[derive(Clone)]
pub(super) enum CachedRequestPath {
    RootRelative(NormalizedPath),
    Absolute(NormalizedPath),
}

impl CachedRequestPath {
    pub(super) fn capture(path: &Path, key_root: Option<&Path>) -> Self {
        if let Some(root) = key_root {
            if let Ok(relative) = path.strip_prefix(root) {
                return Self::RootRelative(NormalizedPath::new(relative));
            }
        }
        Self::Absolute(NormalizedPath::new(path))
    }

    pub(super) fn resolve(&self, key_root: Option<&Path>) -> NormalizedPath {
        match (self, key_root) {
            (Self::RootRelative(relative), Some(root)) => root.join(relative).into(),
            (Self::RootRelative(relative), None) | (Self::Absolute(relative), _) => {
                relative.clone()
            }
        }
    }

    pub(super) fn is_root_relative(&self) -> bool {
        matches!(self, Self::RootRelative(_))
    }
}
