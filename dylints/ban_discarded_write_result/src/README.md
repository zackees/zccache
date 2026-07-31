# Source

`lib.rs` holds the whole lint. Detection is two shapes:

1. `check_local` — `let _ = <expr>;` (a wildcard pattern) whose initializer
   type is `core::result::Result`.
2. `check_stmt` — a statement-position `<expr>.ok();` whose receiver type is
   `core::result::Result`.

In both cases the discarded expression is walked (bodies of nested closures
are not entered) looking for a call whose callee name is in
`WRITE_CALLEE_NAMES`. Only then does the lint fire.

`allowlist.txt` is `include_str!`-embedded and matched against the tail of the
source path with slashes normalized — the same mechanism as
`ban_tmp_literal`. Every entry carries a comment justifying the dropped
error.
