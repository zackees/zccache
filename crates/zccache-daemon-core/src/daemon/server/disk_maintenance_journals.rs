//! Compile-journal handle release for the session reaper (issue #1907).
//!
//! Split out of `disk_maintenance.rs` for the same reason as
//! `disk_maintenance_retired.rs`: the parent module is already at the
//! repo's 1,000-LOC ceiling and the reaper's journal bookkeeping is a
//! self-contained concern that needs no access to retention policy.

use std::collections::HashSet;

use super::SharedState;
use crate::core::NormalizedPath;
use crate::depgraph::Session;

/// #1907: the journal paths a reap pass must close, given the journal paths
/// still claimed by sessions that survived it.
///
/// Drops any path a surviving session still claims — a newer live session may
/// have reused the same journal path, and closing it here would steal that
/// session's buffered writer out from under it — and deduplicates the rest so
/// each path is closed once. First-seen order is preserved.
///
/// Free of daemon state so it is unit-testable without a running daemon.
pub(super) fn journal_paths_to_release<'a>(
    reaped: impl IntoIterator<Item = &'a NormalizedPath>,
    still_claimed: &HashSet<NormalizedPath>,
) -> Vec<NormalizedPath> {
    let mut seen: HashSet<&'a NormalizedPath> = HashSet::new();
    let mut out = Vec::new();
    for path in reaped {
        if still_claimed.contains(path) {
            continue;
        }
        if seen.insert(path) {
            out.push(path.clone());
        }
    }
    out
}

/// #1907: release the compile-journal handles of the sessions this reap pass
/// removed. Returns how many distinct journal paths were closed.
///
/// The reaper is the only path that removes a session *without* a
/// `SessionEnd` — the client crashed or was killed, so the dispatch handler
/// that calls `close_session` never runs. `CompileJournal::log` opens the
/// per-session JSONL file lazily on the background writer thread and keeps the
/// `BufWriter` in that thread's `session_files` map, so a reaped session left
/// an fd plus up to 64 KiB of unflushed records behind for the life of the
/// daemon, unbounded across crashed clients.
pub(super) fn release_reaped_session_journals(state: &SharedState, reaped: &[Session]) -> usize {
    // Most passes reap nothing, and most sessions never asked for a journal.
    // Bail before snapshotting the live sessions, which is the only part of
    // this that costs anything proportional to the daemon's session count.
    let reaped_paths: Vec<&NormalizedPath> = reaped
        .iter()
        .filter_map(|s| s.journal_path.as_ref())
        .collect();
    if reaped_paths.is_empty() {
        return 0;
    }

    let mut still_claimed: HashSet<NormalizedPath> = HashSet::new();
    for id in state.sessions.active_ids() {
        if let Some(session) = state.sessions.get(&id) {
            if let Some(path) = session.journal_path {
                still_claimed.insert(path);
            }
        }
    }
    let paths = journal_paths_to_release(reaped_paths, &still_claimed);
    for path in &paths {
        state.journal.close_session(path.as_path());
    }
    paths.len()
}
