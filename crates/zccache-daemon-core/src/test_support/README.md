# zccache-test-support

Shared test utilities and fixtures (tempfile, tokio test helpers).

- `mod.rs` — tool discovery (`find_clang`, ...), temp dirs, timeouts.
- `miss_overhead.rs` — in-process daemon fixture that measures per-TU
  cache-miss overhead (wall time minus compiler child time); shared by the
  `miss_overhead` criterion bench and the CI budget test (zccache#1670).
