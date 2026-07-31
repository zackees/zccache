# zccache-artifact

Unpublished internal crate for zccache artifact storage and Rust artifact plan save/restore.

- `store.rs` — `ArtifactIndex` / `ArtifactStore`: the in-memory index and its
  `index.bin` bincode snapshot. A load that finds the blob present but
  unparseable starts empty *and* records it, so the daemon can tell corruption
  apart from a cold cache (`take_started_corrupt`).
- `layout.rs` — the shared read resolver for every on-disk artifact layout
  (staged-v2, pack, flat-v1). Also `fixtures` (feature `test-support`) for
  seeding a real staged generation from crates above this one.
- `reconcile.rs` — rebuilding index entries from surviving on-disk payloads
  when `index.bin` is unreadable (#1157). Read its module docs before widening
  what it reconstructs: output filenames are not recoverable from disk and are
  load-bearing for multi-output delivery, so only single-output staged
  generations are rebuilt. The daemon owns *when* this runs
  (`zccache-daemon-core::daemon::server::index_reconcile`); this crate only
  supplies the scan.
- `kv.rs` — the generic namespaced key/value store.
- `rust_plan.rs` — Rust artifact plan bundle save/restore.
