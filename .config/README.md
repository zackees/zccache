# .config

Tool configuration read from the workspace root.

- `nextest.toml` — cargo-nextest profile. Timing- and budget-sensitive tests
  reserve every test slot (`threads-required = "num-cpus"`) so parallel
  neighbours cannot skew their wall-clock assertions.
- The `isolation-guard` run-wrapper (`ci/nextest_isolation_guard.sh`) refuses
  every test outside CI or the isolated bosn gate image (zackees/ci.yml#168,
  GATE-005).
