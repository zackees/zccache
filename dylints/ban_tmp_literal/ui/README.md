# UI tests

`*.rs` here are minimal programs that exercise specific lint behavior. For
each, `dylint_testing::ui_test` compiles the file and asserts the resulting
diagnostics match the adjacent `*.stderr` snapshot (snapshots not yet
captured — the `ui` test is `#[ignore]`d, same as `ban_unrooted_tempdir`).

- `disallowed.rs` — must trigger the lint on `"/tmp"`, `"/tmp/..."`, and a
  `format!` template starting with `/tmp/`.
- `allowed.rs` — neutral fixture paths, `/var/tmp`, and relative `tmp/`
  segments must NOT trigger the lint.
