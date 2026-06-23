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

/// Watch multiple directories in a single batch.
///
/// Canonicalizes all paths up front, reserves unwatched paths under
/// `watched_dirs`, then registers each new watch independently.
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

    // Reserve already-watched paths without holding the set lock across
    // notify's blocking watch registration.
    // Each directory here is the exact parent of a source/header file from
    // depfile scanning — no need to walk children or parents.
    let new_dirs: Vec<NormalizedPath> = {
        let mut watched = state.watched_dirs.lock().await;
        canonical
            .into_iter()
            .filter(|p| watched.insert(p.clone()))
            .collect()
    };

    if new_dirs.is_empty() {
        return;
    }

    for dir in new_dirs {
        match watch_one_directory(state, dir.clone()).await {
            Ok(true) => {
                tracing::info!("watching directory: {}", dir.display());
            }
            Ok(false) => {
                state.watched_dirs.lock().await.remove(&dir);
            }
            Err(e) => {
                state.watched_dirs.lock().await.remove(&dir);
                tracing::warn!("failed to watch {}: {e}", dir.display());
            }
        }
    }
}

async fn watch_one_directory(
    state: &SharedState,
    dir: NormalizedPath,
) -> crate::core::Result<bool> {
    if tokio::runtime::Handle::current().runtime_flavor()
        == tokio::runtime::RuntimeFlavor::MultiThread
    {
        return tokio::task::block_in_place(|| {
            let mut watcher_guard = state.watcher.blocking_lock();
            if let Some(ref mut w) = *watcher_guard {
                w.watch(&dir)?;
                Ok(true)
            } else {
                Ok(false)
            }
        });
    }

    let mut watcher_guard = state.watcher.lock().await;
    if let Some(ref mut w) = *watcher_guard {
        w.watch(&dir)?;
        Ok(true)
    } else {
        Ok(false)
    }
}
