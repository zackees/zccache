# Daemon lifecycle — `daemon_lifecycle` test target

One linked test executable for 11 contracts (#1526). Covers start/stop, cwd release, exe overwrite, stdio detach, spawn budgets and storms, crash minidumps, and the embedded service.

Each file here is a **module** of `main.rs`, not its own integration-test
binary. Cargo compiles every top-level `tests/*.rs` file as a separate
executable that statically links the whole reachable graph; 98 of those
produced 1.77 GB of test binaries on one host. Adding a contract here costs
nothing extra — adding a new top-level file costs another full link.

## Adding a test

1. Drop the file in this directory.
2. Add `mod <file_stem>;` to `main.rs` (a missing `mod` line compiles fine and
   silently runs nothing).
3. Keep any `#![cfg(...)]` gate or `#![allow(...)]` block as an inner attribute
   at the top of your file — it applies to your module alone.

Its test ID is `daemon_lifecycle::<file_stem>::<test_name>`. Run one with:

```
cargo nextest run --test daemon_lifecycle -E 'test(/^<file_stem>::/)'
```

## Modules

- `daemon_cli_flow_test`
- `daemon_crash_minidump_test`
- `daemon_cwd_release`
- `daemon_exe_overwrite`
- `daemon_integration_test`
- `daemon_session_stats_test`
- `daemon_spawn_lockfile_budget_test`
- `daemon_spawn_storm_test`
- `daemon_stdio_detach`
- `daemon_tokio_console_test`
- `embedded_service_test`
