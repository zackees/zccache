# zccache-cli-core

The zccache **CLI subsystem**, extracted from the `zccache` crate (#1022
Phase 2, Split A) to cut incremental recompile time — editing the CLI no longer
recompiles the facade's other code, and it builds in parallel with the daemon
crate.

Contains the former `zccache::cli` module tree (subcommands, compiler wrapper
mode, client) plus the `download_client` and `download_daemon` modules, which
are used only by the CLI.

## Public path stability

The crate re-exports the subsystem crates under the same short aliases the CLI
uses (`core`, `ipc`, `protocol`, …) and `daemon` (from `zccache-daemon-core`,
for the `daemon-run` escape hatch's `daemon::entry`), and preserves the
`pub mod cli` / `download_client` / `download_daemon` structure — so internal
`crate::cli::…` / `crate::download_client` / `crate::daemon::entry` paths
resolve unchanged. The `zccache` facade re-exports these modules, so the public
`zccache::cli::…` paths are stable for the bins and integration tests.

## Features

`cli`, `download` / `download-client` / `download-daemon` / `download-protocol`,
`gha`, `symbols` — mirror the former `zccache` features they replaced; the
facade forwards to them.
