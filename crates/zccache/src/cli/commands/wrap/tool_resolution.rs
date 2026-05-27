//! Wrapper tool path resolution.

use crate::core::NormalizedPath;
use std::path::Path;

use super::super::daemon::which_on_path;

/// Resolve a compiler name/path to an absolute path.
/// Normalizes MSYS paths on Windows, then searches PATH if not already absolute.
pub(super) fn resolve_compiler_path(compiler: &str) -> NormalizedPath {
    let normalized = crate::core::path::normalize_msys_path(compiler);
    let path = Path::new(&normalized);

    if path.is_absolute() {
        return normalized.into();
    }

    match which_on_path(&normalized) {
        Some(abs) => abs,
        None => normalized.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unresolved_relative_tool_is_left_for_daemon_error() {
        let resolved = resolve_compiler_path("definitely-not-a-zccache-test-tool");
        assert!(resolved.ends_with("definitely-not-a-zccache-test-tool"));
    }
}
