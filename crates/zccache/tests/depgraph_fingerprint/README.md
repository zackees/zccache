# Dependency-graph and fingerprint — `depgraph_fingerprint` test target

One linked test executable for 15 contracts (#1526). Covers depgraph persistence, warm classification, depfile ingestion and drift detection, the fingerprint matrices, and lineage propagation.

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

Its test ID is `depgraph_fingerprint::<file_stem>::<test_name>`. Run one with:

```
cargo nextest run --test depgraph_fingerprint -E 'test(/^<file_stem>::/)'
```

## Modules

- `daemon_depgraph_persistence_test`
- `daemon_depgraph_warm_classify_test`
- `daemon_fingerprint_test`
- `daemon_lineage_propagation`
- `depgraph_depfile_integration_test`
- `depgraph_drift_detection_test`
- `depgraph_stress_adversarial_test`
- `depgraph_stress_concurrent_test`
- `depgraph_stress_integration_test`
- `fingerprint_concurrent`
- `fingerprint_edge_cases`
- `fingerprint_end_to_end`
- `fingerprint_glob_scan`
- `fingerprint_stress`
- `symbols_stamp_roundtrip`
