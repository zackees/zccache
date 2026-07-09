//! `Request::ReleaseWorktreeHandles` handler (issue #690).
//!
//! soldr's Tier 3 worktree-teardown fallback calls this to deterministically
//! break Windows file locks before `git worktree remove` / `rmdir`. The
//! daemon's only long-lived handles inside a client-owned directory are the
//! per-session JSONL journal files and per-session log files; mmap'd cache
//! artifacts are short-lived (`hash_file` maps-and-drops). So "release
//! handles under `path`" reduces to: find every session whose `working_dir`
//! is under `path`, end the session, and close its journal sink.
//!
//! Safety: the handler refuses to release `state.cache_dir` (or any ancestor
//! of it). The cache root is shared across every concurrent session — letting
//! a single soldr caller close those handles would corrupt unrelated work.
//! soldr's worktree paths are always disjoint from the cache root, so this
//! refusal never blocks a legitimate Tier 3 call.

use super::*;
use std::path::{Path, PathBuf};

/// Handle a `ReleaseWorktreeHandles` request.
///
/// Returns either `Response::ReleaseWorktreeHandlesResult` on success or
/// `Response::Error` if the requested path overlaps the daemon's cache root.
pub(super) async fn handle_release_worktree_handles(
    state: &SharedState,
    path: &NormalizedPath,
) -> Response {
    let target = match canonicalize_for_match(path.as_path()) {
        Some(p) => p,
        None => path.as_path().to_path_buf(),
    };

    // Refuse if the caller asked us to release our own cache root, or any
    // ancestor of it. The cache root is owned by the daemon, not by any
    // worktree, and closing those handles would corrupt unrelated sessions.
    let cache_root = match canonicalize_for_match(state.cache_dir.as_path()) {
        Some(p) => p,
        None => state.cache_dir.as_path().to_path_buf(),
    };
    if cache_root.starts_with(&target) {
        return Response::Error {
            message: format!(
                "refusing to release handles under {} — that path contains the daemon \
                 cache root {}",
                target.display(),
                cache_root.display(),
            ),
        };
    }

    let session_ids = state.sessions.active_ids();
    let inspected = session_ids.len() as u32;
    let mut released: u32 = 0;
    let mut sessions_dropped: Vec<String> = Vec::new();

    for sid in session_ids {
        let Some(session) = state.sessions.get(&sid) else {
            continue;
        };

        if !path_is_under(&session.working_dir, &target) {
            continue;
        }

        // Mirror the cleanup `Request::SessionEnd` performs so the daemon
        // state is consistent regardless of which path tore the session
        // down. Order matches connection.rs:295 to keep the two sites
        // visually comparable.
        state.session_worktree_roots.remove(&sid);
        if let Some(ended) = state.sessions.end(&sid) {
            if !ended.owner_pids.is_empty() {
                state
                    .private_daemon
                    .release_session(&ended.owner_pids)
                    .await;
            }
            if let Some(ref journal_path) = ended.journal_path {
                state.journal.close_session(journal_path);
            }
            released += 1;
            sessions_dropped.push(sid.to_string());
        }
    }

    tracing::info!(
        path = %target.display(),
        inspected,
        released,
        "released worktree handles"
    );

    Response::ReleaseWorktreeHandlesResult {
        inspected,
        released,
        sessions_dropped,
        // Empty today: no long-lived mmaps to fail-to-release. The field
        // exists so future code that pins file handles inside a worktree
        // can report partial failure without a wire-shape change.
        unreleased: Vec::new(),
    }
}

/// Canonicalize a path for prefix matching. Returns `None` if the path
/// cannot be canonicalized (deleted, permission error, etc.) — callers
/// fall back to the as-supplied path so a soldr teardown of a partially
/// deleted worktree still sweeps sessions whose working_dir is gone.
fn canonicalize_for_match(p: &Path) -> Option<PathBuf> {
    std::fs::canonicalize(p).ok()
}

/// True iff `candidate` resolves to a path under `target`. Both sides are
/// canonicalized when possible so a `\\?\C:\...` verbatim target matches
/// a non-verbatim session working_dir (and vice versa).
fn path_is_under(candidate: &NormalizedPath, target: &Path) -> bool {
    let resolved = canonicalize_for_match(candidate.as_path())
        .unwrap_or_else(|| candidate.as_path().to_path_buf());
    resolved.starts_with(target)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_path_is_canonicalized_for_match() {
        // Verifies the helper accepts canonicalize failures gracefully —
        // a path that does not exist must not panic, just return None.
        let bogus = std::path::Path::new(if cfg!(windows) {
            r"C:\zccache-nonexistent-test-path-690"
        } else {
            "/zccache-nonexistent-test-path-690"
        });
        assert!(canonicalize_for_match(bogus).is_none());
    }
}
