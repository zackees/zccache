# Dylint Libraries

Custom Rust lints used by this workspace.

- `ban_std_pathbuf`: bans new uses of `std::path::PathBuf` outside the explicit legacy allowlist.
- `ban_unrooted_tempdir`: bans tempdir/temp-file creation under `$TMPDIR` instead of `zccache_core::config::default_cache_dir()`.
- `ban_raw_subprocess_in_daemon`: bans raw subprocess spawns in daemon code paths.
- `ban_tmp_literal`: bans hardcoded `/tmp` path string literals — they only exist on POSIX (#828).

All four libraries use the published Dylint 6.0.1 crates and the pinned
`nightly-2026-05-26` toolchain. The supported `cargo-dylint` driver is used
directly; no custom driver checkout or library alias repair is required.

Run from the repository root:

```bash
cargo dylint --all --workspace
```
