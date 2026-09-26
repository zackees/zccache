# Embedded service internals

- `mode_tests.rs` covers the `ZCCACHE_MODE` service setting (#1683): the
  setter round trip and a real rustc hit delivered by the resolved mode.
- `tests.rs` contains streaming, cancellation, flush-report, runtime-hook,
  host-identity, and journal tests for the public `embedded.rs` facade.
