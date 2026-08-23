# UI tests

`*.rs` here are minimal programs that exercise specific lint behavior. For
each, `dylint_testing::ui_test` compiles the file and asserts the resulting
diagnostics match the adjacent `*.stderr` snapshot.

- `disallowed.rs` — a bare `std::path::PathBuf` binding, which must trigger
  the lint. Raw `PathBuf` does not carry zccache's normalization invariant,
  so it is banned outside the platform leaf and an explicit legacy allowlist.
