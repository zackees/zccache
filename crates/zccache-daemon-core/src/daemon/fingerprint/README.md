# daemon `fingerprint`

Daemon-side fingerprint watch manager: in-memory per-watch dirty state fed by FS
watcher events (`on_batch`) and queried over IPC through `check`, plus the
safety net that re-stats tracked files to catch watcher events the daemon never
received (#1897).

Two layers, in order: layer 1 is the cheap stat comparison (mtime, size, file
identity, Unix `ctime`) that skips untouched files without reading them, and
layer 2 re-hashes only the files layer 1 flagged, turning a stat change into
either a content change or a smart touch.

- `mod.rs` — `FingerprintManager`, `WatchKey`, `WatchState`, `TrackedFile`,
  `ChangedMeta`, and the `check` / `mark_success` / `mark_failure` /
  `invalidate` / `on_batch` / `watch_count` methods
- `verify.rs` — the lock-free filesystem-verification collect/apply split
  (issue #1908): `collect_verifications` does all the stat + blake3 hashing
  with no watch-map guard in scope, and `apply_verifications` only mutates the
  in-memory `HashMap<String, TrackedFile>`. Also `FileObservation`, the
  `observe` stat snapshot, and the layer-1 `unchanged` predicate.
- `tests.rs` — unit tests for all of the above

**Lock discipline:** no function in this directory may hold a `DashMap`
read/write shard guard on `FingerprintManager::watches` across filesystem I/O
(`mtime_ns`, `file_size`, `hash_file`, `canonicalize`, `walk_files`). This is the
rule `on_batch` already follows for issue #724; `check` is the sibling path that
previously violated it (#1908).
