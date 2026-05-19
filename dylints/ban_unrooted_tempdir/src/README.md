# ban_unrooted_tempdir — sources

- `lib.rs` — late-pass `LintContext` visitor that fires on calls resolving to
  one of `BANNED_FN_PATHS`. The matching list is exact (no prefix match), so
  `tempfile::tempdir_in` and `tempfile::TempDir::new_in` are intentionally
  not banned.
- `allowlist.txt` — newline-separated tail-suffix matches for source files
  that pre-date the lint and still legitimately use the OS temp dir (mostly
  tests and benches). New non-test files should never be added here.

See the parent directory's `README.md` for the user-facing rationale and the
table of replacements.
