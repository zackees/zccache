# Fingerprint

Daemon-owned in-memory fingerprint state for every active fingerprint watch: the
per-file tracked metadata, the watcher-fed `on_batch` path that marks watches
dirty, and the `verify_filesystem` safety net that re-stats tracked files to
catch watcher events the daemon never received (#1897).

Two layers, in order: layer 1 is the cheap stat comparison (mtime, size, file
identity, Unix `ctime`) that skips untouched files without reading them, and
layer 2 re-hashes only the files layer 1 flagged, turning a stat change into
either a content change or a smart touch.

- `mod.rs` — `FingerprintManager`, `WatchKey`, `WatchState`, `TrackedFile`,
  `ChangedMeta`, and the `check` / `mark_success` / `mark_failure` /
  `invalidate` / `on_batch` / `watch_count` methods
- `verify.rs` — `verify_filesystem` and the `FileObservation` stat snapshot
- `tests.rs` — unit tests for all of the above
