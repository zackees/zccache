## scanner/

Submodules of `scanner.rs`, the `#include` directive scanner.

- `lex.rs` — single-pass byte lexer behind `scan_includes_str`: skips line
  splices and comments in place and only copies out lines that may be an
  `#include` (zccache#1670).
- `lex_tests.rs` — lexer tests, including a differential check against the
  previous multi-pass scanner (kept there as `legacy`) and a speed-ratio
  perf unit test.
