# ban_tmp_literal/src

- `lib.rs` — the late lint pass: flags `"/tmp"` / `"/tmp/..."` string
  literals, checks the allowlist by path tail, and carries the ignored UI
  test scaffolding shared with `ban_unrooted_tempdir`.
- `allowlist.txt` — legacy exemptions, grouped and justified inline. New
  entries require an inline comment explaining why the site cannot use
  `zccache_core::config::tmp_dir()` or a neutral fixture path.
