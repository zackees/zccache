# Adversarial and stress — `stress` test target

One linked test executable for 9 contracts (#1526). Covers compiler, correctness, edge and watcher stress, the adversarial corner-case and mutation matrices, KV stress, and compile-concurrency gating.

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

Its test ID is `stress::<file_stem>::<test_name>`. Run one with:

```
cargo nextest run --test stress -E 'test(/^<file_stem>::/)'
```

## Modules

- `artifact_kv_stress`
- `compile_concurrency_gating`
- `daemon_adversarial_corner_cases`
- `daemon_adversarial_mutations`
- `daemon_stress_compiler_test`
- `daemon_stress_correctness_test`
- `daemon_stress_edges_test`
- `daemon_watcher_adversarial`
- `watcher_stress_test`
