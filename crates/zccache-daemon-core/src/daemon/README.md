## daemon/

Daemon library and runtime — server, compile journal, lifecycle, crash handling. Bins live at `crates/zccache/src/bin/zccache-daemon.rs` and `crash-trigger.rs`. `fingerprint/` holds the in-memory per-watch fingerprint state the CLI queries over IPC, plus the guard-free collect/apply verification safety net that re-stats tracked files to catch watcher events that were never delivered (#1908).
