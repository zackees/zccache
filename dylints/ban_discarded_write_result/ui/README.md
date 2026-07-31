# UI tests

`*.rs` here are minimal programs that exercise specific lint behavior. For
each, `dylint_testing::ui_test` compiles the file and asserts the resulting
diagnostics match the adjacent `*.stderr` snapshot.

- `disallowed.rs` — must trigger on `let _ = std::fs::write(..)`,
  `let _ = tx.send(..)`, and a statement-position
  `std::fs::rename(..).ok();`.
- `allowed.rs` — the gated-on-success `if let Err(e) = write(..)` form, a
  discarded non-`Result` (`println!`), a discarded non-write `Result`
  (`parse`), and `let _ = std::fs::remove_file(..)` (cleanup names are not
  in the matched set) must NOT trigger.
