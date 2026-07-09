# zccache-daemon-core/src

- `lib.rs` — subsystem-crate aliases + module declarations (`daemon`, `embedded`, `audit_writer`, `test_support`).
- `daemon/` — IPC server, connection dispatch, `SharedState`, compile/link/exec handlers + pipeline, lifecycle, watchdogs, the daemon `entry` point.
- `embedded.rs` — the in-process `ZccacheService` API (soldr/fbuild).
- `audit_writer.rs` — durable audit JSONL writer for the embedded service.
- `test_support/` — dev-only test helpers (feature `test-support`).
