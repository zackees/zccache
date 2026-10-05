//! Startup policy for the daemon's lock-file ownership record (#1903).
//!
//! Extracted out of `daemon::entry` so the degraded arm is reachable from a
//! unit test. Inline it was an `if let Err(e) = write_lock_file(pid) { warn! }`
//! that could only ever be pinned by re-emitting its events by hand, which
//! cannot catch an arm that stops firing.
//!
//! The invariant: after winning the endpoint bind, a daemon that cannot record
//! its PID in the lock file is undiscoverable and unstoppable — both
//! `check_running_daemon` and `probe_existing_daemon` read the lock file, so
//! they keep reporting the daemon as absent while it holds the cache root, and
//! `zccache stop` reports that nothing is running. Such a daemon must refuse to
//! serve rather than report a successful start (zackees/zccache#1903).

/// What startup must do once it has tried to record its PID.
pub(crate) enum StartupDecision {
    /// Ownership is recorded; serve normally.
    Serve,
    /// Ownership is unrecorded, so serving would be unmanageable. `error` is
    /// the underlying IO failure, carried for the operator-facing message.
    RefuseServing { error: String },
}

/// Record `pid` in the daemon lock file and decide what startup does next.
///
/// This is the whole #1903 policy: the decision is derived from the write's
/// result rather than left to the caller's `match`, so "unrecorded implies
/// refuse to serve" cannot be re-broken by a caller that ignores an arm.
pub(crate) fn record_ownership(pid: u32) -> StartupDecision {
    match crate::ipc::write_lock_file(pid) {
        Ok(()) => StartupDecision::Serve,
        Err(e) => StartupDecision::RefuseServing {
            error: e.to_string(),
        },
    }
}

/// Write the durable lifecycle row for a refused start.
///
/// This only reports. It deliberately does **not** call
/// `crate::ipc::remove_lock_file()` — removing a lock file this daemon never
/// wrote can delete the winner's record (that invariant is #1905, and the
/// bind-loser paths in `entry.rs` are where the removal belongs) — and it does
/// **not** call `std::process::exit`, because the exit is the caller's
/// `entry.rs` decision to make once it has logged the row.
pub(crate) fn report_unrecorded(endpoint: &str, error: &str) {
    crate::daemon::lifecycle::write_event(
        crate::daemon::lifecycle::EVENT_DAEMON_LOCKFILE_UNRECORDED,
        serde_json::json!({
            "endpoint": endpoint,
            "pid": std::process::id(),
            "error": error,
            "consequence": "refusing to serve without an ownership record",
        }),
    );
}

#[cfg(test)]
#[path = "startup_lockfile_tests.rs"]
mod tests;
