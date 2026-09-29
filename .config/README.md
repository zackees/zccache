# .config

Tool configuration read from the workspace root.

- `nextest.toml` — cargo-nextest profile. Timing- and budget-sensitive tests
  reserve every test slot (`threads-required = "num-cpus"`) so parallel
  neighbours cannot skew their wall-clock assertions.
