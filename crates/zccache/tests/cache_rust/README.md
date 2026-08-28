# Rust-toolchain cache — `cache_rust` test target

One linked test executable for 12 contracts (#1526). Covers the rustc cache (basic, restore, worktree, path remap, async populate), its adversarial matrices, the dylint cache, and the copy-on-write cargo contracts.

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

Its test ID is `cache_rust::<file_stem>::<test_name>`. Run one with:

```
cargo nextest run --test cache_rust -E 'test(/^<file_stem>::/)'
```

## Modules

- `cow_mediated_contracts`
- `cow_readonly_cargo`
- `daemon_dylint_cache_test`
- `daemon_rustc_adversarial_concurrency_test`
- `daemon_rustc_adversarial_corner_cases_test`
- `daemon_rustc_adversarial_mutations_test`
- `daemon_rustc_cache_basic_test`
- `daemon_rustc_cache_path_remap_test`
- `daemon_rustc_cache_worktree_test`
- `daemon_rustc_issue_210_async_populate_test`
- `daemon_rustc_restore_test`
- `daemon_workspace_pin_747`
