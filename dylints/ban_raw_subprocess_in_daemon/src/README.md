# ban_raw_subprocess_in_daemon — sources

- `lib.rs` — late-pass `LintContext` visitor that fires on `MethodCall`
  expressions whose resolved DefId matches one of `BANNED_METHOD_PATHS`
  (`Command::spawn`/`output`/`status` on both `std::process` and
  `tokio::process`). Matching is exact — any other method on those types
  (`wait_with_output`, `kill`, `try_wait`, …) is intentionally not banned.
- `allowlist.txt` — newline-separated tail-suffix matches for source files
  that pre-date the lint or legitimately need to call the banned APIs (the
  blessed helpers themselves; in-source `#[cfg(test)]` modules). New
  production code should NEVER be added here — route the spawn through
  `crates/zccache-daemon/src/process.rs` helpers instead.

See the parent directory's `README.md` for the user-facing rationale and
the table of replacements.
