# Daemon wire-protocol — `wire` test target

One linked test executable for 6 contracts (#1526). Covers frame v1, the dispatcher, prost round-trips, protocol versioning, and IPC timeouts.

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

Its test ID is `wire::<file_stem>::<test_name>`. Run one with:

```
cargo nextest run --test wire -E 'test(/^<file_stem>::/)'
```

## Modules

- `daemon_wire_dispatcher`
- `daemon_wire_frame_v1_test`
- `daemon_wire_perf_scaffold`
- `daemon_wire_prost_roundtrip`
- `daemon_wire_protocol_version`
- `ipc_timeout`
