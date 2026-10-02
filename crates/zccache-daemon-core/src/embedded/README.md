# Embedded service internals

- `mode_tests.rs` covers the `ZCCACHE_MODE` service setting (#1683): the
  setter round trip and a real rustc hit delivered by the resolved mode.
- `tests.rs` contains streaming, cancellation, flush-report, runtime-hook,
  host-identity, and journal tests for the public `embedded.rs` facade.
- `observed.rs` holds the request-scoped API (#1550): `CompileOptions`,
  `CompileObservation`, `compile_with_options` and
  `compile_streaming_with_options`; `compile`/`compile_streaming` delegate
  here with default options.
- `test_harness_admission_tests.rs` drives one running service with real
  rustc `--test` harness compiles under alternating policies, and proves a
  changed input (source, codegen option, cfg, link argument, `--test`)
  misses.
