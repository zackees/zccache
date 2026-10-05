//! Lock-file ownership guard for the standalone daemon (#1905).
//!
//! `daemon::entry` only writes the lock file *after* the bind succeeds
//! (`ipc::write_lock_file`, well past the bind block). Every failure path
//! that runs before that point — the generic bind-error arm and the
//! bind-worker-join-error arm — used to call `ipc::remove_lock_file()`
//! unconditionally, which unlinks whichever daemon *did* win the endpoint
//! (or, in the cache-root-contention case, the one that owns the cache root)
//! and leaves a live daemon undiscoverable to `check_running_daemon()`,
//! `probe_existing_daemon` and `zccache stop`.
//!
//! This guard records ownership instead of enumerating error kinds: cleanup
//! is a no-op until [`LockFileGuard::mark_written`] is called, so a failure
//! before `write_lock_file` can never delete a lock file this process did
//! not create. It owns the path it removes rather than calling back into
//! `ipc::remove_lock_file`, so a unit test can point it at a tempdir.

pub(crate) struct LockFileGuard {
    path: crate::core::NormalizedPath,
    written: bool,
}

impl LockFileGuard {
    pub(crate) fn new(path: crate::core::NormalizedPath) -> Self {
        Self {
            path,
            written: false,
        }
    }

    /// Record that this process wrote the lock file; from here it is ours to
    /// delete.
    pub(crate) fn mark_written(&mut self) {
        self.written = true;
    }

    /// Whether this process is the one that created the lock file.
    #[must_use]
    pub(crate) fn wrote_lock_file(&self) -> bool {
        self.written
    }

    /// Delete the lock file only when this process wrote it.
    ///
    /// Returns `true` when a removal was attempted. A `false` return means
    /// some other daemon owns the file and it was left untouched.
    pub(crate) fn remove_if_owned(&self) -> bool {
        if self.written {
            let _ = std::fs::remove_file(&self.path);
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::core::NormalizedPath;

    /// Write a sentinel lock file into `dir` and return it normalized, the
    /// shape both `ipc::lock_file_path()` and a losing daemon would see.
    fn sentinel_lock_file(dir: &std::path::Path, contents: &str) -> NormalizedPath {
        let path = NormalizedPath::new(dir.join(format!("daemon-{}.lock", std::process::id())));
        std::fs::write(&path, contents).unwrap();
        path
    }

    /// The RED test for #1905. Before the guard existed, every pre-write
    /// failure path ran `ipc::remove_lock_file()` unconditionally, so a
    /// daemon that never created the file still unlinked it and the winner
    /// became undiscoverable. Cleanup must be a no-op until `mark_written`.
    #[test]
    fn unwritten_lock_file_is_never_removed() {
        let tmp = tempfile::tempdir().unwrap();
        let path = sentinel_lock_file(tmp.path(), "4242");

        let guard = LockFileGuard::new(path.clone());
        assert!(!guard.wrote_lock_file());
        assert!(
            !guard.remove_if_owned(),
            "no removal may be attempted for an unwritten lock file"
        );
        assert!(
            path.as_path().exists(),
            "a lock file this daemon did not write must survive"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "4242");
    }

    /// The positive half: once this process wrote the lock file, the daemon
    /// IS the owner and its clean-exit / server-error cleanup must still
    /// remove it.
    #[test]
    fn written_lock_file_is_removed() {
        let tmp = tempfile::tempdir().unwrap();
        let path = sentinel_lock_file(tmp.path(), "4242");

        let mut guard = LockFileGuard::new(path.clone());
        guard.mark_written();
        assert!(guard.wrote_lock_file());
        assert!(guard.remove_if_owned());
        assert!(
            !path.as_path().exists(),
            "our own lock file must be cleaned up"
        );
    }

    /// End-to-end shape of the #1905 failure: a winner holds the cache root,
    /// the loser binds a *different* endpoint on the *same* root and fails
    /// with `WouldBlock` (cache-root writer contention, not an endpoint
    /// conflict — see `server::lifecycle::cache_root_error`). Because the
    /// loser never reached `write_lock_file`, the winner's lock file must
    /// survive the loser's cleanup.
    // `DaemonServer::bind` installs an IPC owner on a Tokio reactor (the
    // kernal-api `ipc_owner_only` path panics with "there is no reactor
    // running" otherwise), so this needs a runtime even though it awaits
    // nothing — same reason the other `bind_with_cache_dir` tests do.
    #[tokio::test]
    async fn failed_cache_root_bind_leaves_the_winning_daemons_lock_file_alone() {
        let cache_tmp = tempfile::tempdir().unwrap();
        let cache_dir: NormalizedPath = cache_tmp.path().join("cache").into();

        // Named binding: a bare `_` would drop the server immediately and
        // release the writer lock, so the second bind would succeed.
        let _winner = crate::daemon::DaemonServer::bind_with_cache_dir(
            &crate::ipc::unique_test_endpoint(),
            &cache_dir,
        )
        .unwrap();

        // `expect_err` would need `DaemonServer: Debug`; match instead.
        let Err(error) = crate::daemon::DaemonServer::bind_with_cache_dir(
            &crate::ipc::unique_test_endpoint(),
            &cache_dir,
        ) else {
            panic!("a second daemon on the same cache root must be refused");
        };
        assert!(
            matches!(
                &error,
                crate::ipc::IpcError::Io(io)
                    if io.kind() == std::io::ErrorKind::WouldBlock
            ),
            "cache-root contention must surface as WouldBlock, got: {error:?}"
        );

        // The loser's cleanup runs with no recorded ownership.
        let lock_tmp = tempfile::tempdir().unwrap();
        let path = sentinel_lock_file(lock_tmp.path(), "4242");
        assert!(!LockFileGuard::new(path.clone()).remove_if_owned());
        assert!(path.as_path().exists());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "4242");
    }

    /// `daemon/entry.rs` read from disk with comment lines stripped.
    ///
    /// `entry` is `#[cfg(feature = "daemon-entry")]`, so a plain
    /// `-p zccache-daemon-core` test run never compiles it and no
    /// behavioral test can reach its cleanup wiring — and its failure
    /// arms call `std::process::exit`, so they are untestable even when
    /// the feature is on. Read the source instead: that is the only way
    /// to make reverting #1905 fail a test.
    fn entry_source() -> String {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/daemon/entry.rs");
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()));
        text.lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The RED->GREEN wiring guard for #1905. Before the fix, `entry.rs`
    /// called `crate::ipc::remove_lock_file()` unconditionally on every
    /// pre-write failure path, so this assertion fails on the old code.
    #[test]
    fn daemon_entry_removes_the_lock_file_only_through_the_ownership_guard() {
        let source = entry_source();

        // Spelled with `concat!` so this file never contains the needle.
        let raw_removal = concat!("ipc::remove_", "lock_file");
        assert!(
            !source.contains(raw_removal),
            "daemon::entry must route every lock-file cleanup through \
             LockFileGuard. A raw removal deletes whichever daemon won the \
             bind, not the one that is failing, and leaves a live daemon \
             undiscoverable to check_running_daemon() and `zccache stop` \
             (#1905)."
        );
        assert!(
            source.contains("LockFileGuard::new"),
            "daemon::entry must construct the ownership guard before the bind"
        );
        assert!(
            source.contains(concat!("mark_", "written")),
            "daemon::entry must record ownership after the lock-file write"
        );
        assert!(
            source.contains(concat!("remove_if_", "owned")),
            "daemon::entry must keep its bind-failure / server-error / \
             clean-exit cleanups, routed through the guard"
        );
    }

    /// Ownership has to be recorded *after* the write, and the
    /// pre-write cleanups have to exist at all — that ordering is the
    /// invariant the whole #1905 fix rests on, and it is invisible to
    /// the unit tests above.
    #[test]
    fn daemon_entry_records_lock_file_ownership_after_the_write() {
        let source = entry_source();
        let position = |needle: &str| {
            source
                .find(needle)
                .unwrap_or_else(|| panic!("daemon/entry.rs no longer contains `{needle}`"))
        };

        // #1903 moved the write itself into `daemon::startup_lockfile::record_ownership`,
        // which returns the decision startup acts on, so the post-bind
        // ownership write is now that call rather than a bare
        // `ipc::write_lock_file` in this file. #1904 gave that call its
        // endpoint argument; the ordering invariant is unchanged: still
        // strictly between the pre-write cleanups and `mark_written`.
        let write = position("record_ownership(&endpoint, pid)");
        assert!(
            position(concat!("mark_", "written")) > write,
            "ownership may only be recorded after the lock-file write; \
             marking it earlier re-opens #1905"
        );
        assert!(
            position(concat!("remove_if_", "owned")) < write,
            "the bind-failure cleanup must run before the lock-file write \
             — that is the arm #1905 is about, and it only stays safe \
             while it goes through the guard"
        );
    }
}
