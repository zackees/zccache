# `zccache-cli` test fixture binaries

Auxiliary `[[bin]]` targets used only by the CLI crate's integration
tests. They are not shipped with the released `zccache` binary.

- **`cli_crash_trigger.rs`** — installs `zccache_monocrate::core::crash::install("zccache")`
  and then deliberately faults (panic / SIGSEGV / SIGABRT). Used by
  `tests/cli_crash_test.rs` to assert the dump filename includes the
  `zccache` binary stem and the right kind label. See issue #313.
