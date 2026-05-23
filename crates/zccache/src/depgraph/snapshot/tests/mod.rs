//! `#[cfg(test)]` modules for `snapshot/`, split per concern so every file
//! stays well under 1,000 LOC.
//!
//! See the directory's `README.md` for the layout overview.

use tempfile::TempDir;
use crate::core::NormalizedPath;
use crate::hash::ContentHash;

use super::super::context::CompileContext;
use super::super::search_paths::IncludeSearchPaths;

mod behavioral;
mod persistence;
mod round_trip;

/// Default snapshot path inside a tempdir.
pub(super) fn test_path(dir: &TempDir) -> NormalizedPath {
    dir.path().join("depgraph.bin").into()
}

/// Minimal `CompileContext` with the given source file and everything else
/// defaulted. Shared by every test that needs a quick context.
pub(super) fn make_ctx(source: &str) -> CompileContext {
    CompileContext {
        source_file: NormalizedPath::from(source),
        include_search: IncludeSearchPaths::default(),
        defines: Vec::new(),
        flags: Vec::new(),
        force_includes: Vec::new(),
        unknown_flags: Vec::new(),
    }
}

/// Hash a path's bytes — used by tests that don't care what the hash is,
/// only that it is stable per-path.
pub(super) fn dummy_hash(path: &std::path::Path) -> Option<ContentHash> {
    Some(crate::hash::hash_bytes(path.to_string_lossy().as_bytes()))
}

/// Freshness oracle that always reports "no change since last scan". Used
/// by tests that want to exercise the cached-hit path.
pub(super) fn always_fresh(_: &std::path::Path) -> bool {
    true
}
