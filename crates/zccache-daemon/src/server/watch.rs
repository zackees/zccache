//! Directory watch registration helpers.
//!
//! Compile/link requests call into these to ensure that the directories
//! holding their source/header files are being watched. Canonicalization is
//! deferred behind a raw-path pre-filter because `canonicalize()` is 1-5ms on
//! Windows for paths already known to be watched.

use super::*;

/// Watch a directory for file changes, if not already watched.
pub(super) async fn watch_directory(state: &SharedState, dir: &Path) {
    watch_directories(state, &[dir.into()]).await;
}

/// Watch multiple directories in a single batch, acquiring locks once.
///
/// Canonicalizes all paths up front, deduplicates against already-watched set,
/// then registers all new watches in one lock acquisition.
pub(super) async fn watch_directories(state: &SharedState, dirs: &[NormalizedPath]) {
    if dirs.is_empty() {
        return;
    }

    // Pre-filter: skip dirs we've already processed (by raw path).
    // This avoids expensive canonicalize() syscalls (~1-5ms each on Windows)
    // for directories that are already being watched.
    let new_raw: Vec<&NormalizedPath> = dirs
        .iter()
        .filter(|d| !state.watched_raw_dirs.contains_key(*d))
        .collect();
    if new_raw.is_empty() {
        return;
    }

    // Canonicalize only new paths (filesystem work, no lock needed).
    // On Windows, canonicalize() produces \\?\ extended-length paths which
    // don't match the paths reported by notify's ReadDirectoryChangesW.
    // Strip the prefix so watched paths match event paths.
    let canonical: Vec<NormalizedPath> = new_raw
        .iter()
        .filter_map(|dir| match dir.canonicalize() {
            Ok(p) => {
                #[cfg(windows)]
                {
                    let s = p.to_string_lossy();
                    if let Some(stripped) = s.strip_prefix(r"\\?\") {
                        Some(stripped.into())
                    } else {
                        Some(p.into())
                    }
                }
                #[cfg(not(windows))]
                {
                    Some(p.into())
                }
            }
            Err(e) => {
                tracing::debug!("cannot canonicalize {}: {e}", dir.display());
                None
            }
        })
        .collect();

    // Mark raw paths as processed (even if canonicalize failed) so we don't
    // retry them on every subsequent call.
    for d in &new_raw {
        state.watched_raw_dirs.insert((*d).clone(), ());
    }

    if canonical.is_empty() {
        return;
    }

    // Single lock acquisition: filter already-watched and register new ones.
    // Each directory here is the exact parent of a source/header file from
    // depfile scanning — no need to walk children or parents.
    let mut watched = state.watched_dirs.lock().await;
    let new_dirs: Vec<NormalizedPath> = canonical
        .into_iter()
        .filter(|p| !watched.contains(p))
        .collect();

    if new_dirs.is_empty() {
        return;
    }

    let mut watcher_guard = state.watcher.lock().await;
    if let Some(ref mut w) = *watcher_guard {
        for dir in new_dirs {
            if let Err(e) = w.watch(&dir) {
                tracing::warn!("failed to watch {}: {e}", dir.display());
                continue;
            }
            tracing::info!("watching directory: {}", dir.display());
            watched.insert(dir);
        }
    }
}
