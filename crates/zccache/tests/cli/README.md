# CLI surface and wrapper — `cli` test target

One linked test executable for 17 contracts (#1526). Covers the `zccache` CLI verbs, the compiler wrappers, installer/distribution shape, and the formatter API.

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

Its test ID is `cli::<file_stem>::<test_name>`. Run one with:

```
cargo nextest run --test cli -E 'test(/^<file_stem>::/)'
```

## Modules

- `ci_timeout_and_progress`
- `cli_cache_root`
- `cli_cli_crash_test`
- `cli_daemon_start`
- `cli_defender_exclusions`
- `cli_ino_convert`
- `cli_installers`
- `cli_kv`
- `cli_meson_configure_cache`
- `cli_no_spawn_guard`
- `cli_rust_plan_lifecycle`
- `cli_session_end`
- `cli_single_daemon_per_session`
- `cli_wrapper_failure_boundaries`
- `cli_wrapper_passthrough`
- `formatter_api`
- `single_binary_distribution`
