# Dylint Libraries

Custom Rust lints used by this workspace.

- `ban_std_pathbuf`: bans new uses of `std::path::PathBuf` outside the explicit legacy allowlist.
- `ban_unrooted_tempdir`: bans tempdir/temp-file creation under `$TMPDIR` instead of `zccache_core::config::default_cache_dir()`.
- `ban_raw_subprocess_in_daemon`: bans raw subprocess spawns in daemon code paths.
- `ban_tmp_literal`: bans hardcoded `/tmp` path string literals — they only exist on POSIX (#828).

Note:

- `ban_std_pathbuf` currently pins `dylint_linting` / `dylint_testing` to an upstream `trailofbits/dylint` commit instead of the `5.0.0` crates.io release. That release predates the March 18, 2026 `rustc_session` API change and fails against the newer toolchains this repo uses. Remove the git pin once a newer crates.io release includes that compatibility fix.

Run from the repository root:

```bash
cargo dylint --all --workspace
```
