# test-support binaries

Tiny helper binaries used by integration tests in sibling crates. Each
binary is intentionally minimal — a single file with a `main` and zero
runtime deps beyond `std` — so the test harness can rely on it
deterministically across OS targets.

- `echo_shim.rs` — Reads stdin to EOF, writes a marker to stdout, writes
  a different marker plus the raw stdin payload to stderr, exits with
  `argv[1]` as `i32` (default 0). Used by `wrapper_passthrough` to
  assert byte-for-byte stdio round-trip through the zccache wrapper +
  daemon IPC chain.
