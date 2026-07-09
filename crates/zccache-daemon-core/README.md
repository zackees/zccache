# zccache-daemon-core

The zccache **daemon subsystem**, extracted from the monolithic `zccache` crate
(#1018 Phase 1) to cut incremental recompile time — editing the CLI no longer
recompiles the 32K-LOC daemon, and the two now build in parallel.

Contains the former `zccache::daemon` module tree (IPC server, connection
dispatch, `SharedState`, the compile/link/exec handlers + pipeline, lifecycle,
watchdogs), the embedded `ZccacheService` (`embedded`), the durable audit JSONL
writer (`audit_writer`), and the dev-only `test_support` helpers.

## Public path stability

The crate re-exports the subsystem crates under the same short aliases the
daemon code uses (`core`, `ipc`, `protocol`, `depgraph`, …) at its lib root, and
preserves the `pub mod daemon` / `embedded` / `audit_writer` / `test_support`
module structure — so internal `crate::daemon::…` / `crate::embedded` /
`crate::test_support` paths resolve unchanged. The `zccache` facade re-exports
this crate's modules (`pub use zccache_daemon_core::daemon` etc.), so the public
`zccache::daemon::…` / `zccache::embedded::…` paths are unchanged for consumers
(soldr/fbuild, the CLI, integration tests).

## Features

- `daemon-entry` — the daemon process entry point (`daemon::entry`); pulls clap + tracing-subscriber.
- `test-support` — dev-only test utilities.
- `tokio-console` — tokio-console instrumentation.
