//! PCH (precompiled header) source-header resolution and friends.
//!
//! Clang PCH output is non-deterministic (embeds timestamps), so hashing the
//! binary produces different keys even when headers haven't changed. Instead,
//! we hash the source header which IS deterministic — and we use the in-memory
//! PCH registry first (populated when PCH generation succeeds) before falling
//! back to filesystem heuristics.

use super::*;

/// For a PCH binary (.pch/.gch), return the path to its source header.
///
/// Clang PCH output is non-deterministic (embeds timestamps), so hashing the
/// binary produces different keys even when headers haven't changed. Instead,
/// we hash the source header which IS deterministic.
///
/// `test_pch.h.pch` → `test_pch.h`
/// `.build/meson-quick/tests/test_pch.h.pch` → tries sibling `test_pch.h`,
/// then walks parent directories looking for `tests/test_pch.h`.
pub(super) fn pch_source_header(path: &Path) -> Option<NormalizedPath> {
    let ext = path.extension()?.to_str()?;
    if ext != "pch" && ext != "gch" {
        return None;
    }
    // The stem of "test_pch.h.pch" is "test_pch.h"
    let header_name = path.file_stem()?;
    // Try sibling: same directory
    let sibling = path.with_file_name(header_name);
    if sibling.exists() {
        return Some(sibling.into());
    }
    // The PCH is typically in a build directory. Walk up looking for the
    // source header by matching the last path component(s).
    // e.g., .build/meson-quick/tests/test_pch.h.pch → look for tests/test_pch.h
    if let Some(parent) = path.parent() {
        // Get the directory name (e.g., "tests")
        if let Some(dir_name) = parent.file_name() {
            let relative = NormalizedPath::new(dir_name).join(header_name);
            // Walk up from the build dir looking for a matching path
            let mut search: NormalizedPath = parent.into();
            for _ in 0..10 {
                if let Some(up) = search.parent() {
                    let candidate = up.join(&relative);
                    if candidate.exists() {
                        return Some(candidate.into());
                    }
                    search = up.into();
                } else {
                    break;
                }
            }
        }
    }
    None
}

/// Resolve the source header for a PCH binary. First checks the in-memory
/// registry (populated when PCH generation succeeds), then falls back to the
/// filesystem heuristic. Returns `None` for non-PCH files.
pub(super) fn resolve_pch_source(
    path: &Path,
    pch_map: &DashMap<NormalizedPath, NormalizedPath>,
) -> Option<NormalizedPath> {
    // Fast path: check registry (covers build-dir separation).
    if let Some(src) = pch_map.get(&NormalizedPath::new(path)) {
        return Some(src.clone());
    }
    // Fallback: filesystem heuristic.
    pch_source_header(path)
}
