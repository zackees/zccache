# Performance and profiling — `perf` test target

One linked test executable for 10 contracts (#1526). Covers artifact fallback, persist-pool and heap benchmarks, the profile matrices, and the inner trace file.

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

Its test ID is `perf::<file_stem>::<test_name>`. Run one with:

```
cargo nextest run --test perf -E 'test(/^<file_stem>::/)'
```

## Modules

- `artifact_perf_index_durability_test`
- `daemon_cold_path_profile_test`
- `daemon_perf_artifact_fallback_test`
- `daemon_perf_test`
- `daemon_persist_pool_bench`
- `daemon_profile_multi_test`
- `daemon_profile_test`
- `fscache_persistence_perf_test`
- `heap_profile_test`
- `inner_trace_file_test`
