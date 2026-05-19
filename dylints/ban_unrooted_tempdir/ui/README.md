# UI tests

`*.rs` here are minimal programs that exercise specific lint
behavior. For each, `dylint_testing::ui_test` compiles the file and
asserts the resulting diagnostics match the adjacent `*.stderr` snapshot.

- `disallowed.rs` — must trigger the lint on every banned call form
  (`std::env::temp_dir`, `tempfile::tempdir`, `tempfile::TempDir::new`,
  `tempfile::NamedTempFile::new`).
- `allowed.rs` — the `_in(...)` variants must NOT trigger the lint.
