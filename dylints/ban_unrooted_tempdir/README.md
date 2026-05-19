# ban_unrooted_tempdir

This lint bans calls that create scratch directories or files under the OS
temp dir (`$TMPDIR` / `%TEMP%`) and steers production code toward paths
rooted under `zccache_core::config::default_cache_dir()` (i.e.
`~/.zccache/`, honoring `ZCCACHE_CACHE_DIR`).

The user-facing motivation: every byte zccache writes should live under one
ground-truth directory the user can inspect, override, or clean. Scratch
dirs scattered across `$TMPDIR` are invisible to a `zccache clear` and
survive process death on Windows for hours.

The banned APIs are:

| Banned call                | Replacement                                                                                  |
| -------------------------- | -------------------------------------------------------------------------------------------- |
| `std::env::temp_dir()`     | `zccache_core::config::default_cache_dir()` (or a named subdir like `symbols_cache_dir()`)   |
| `tempfile::tempdir()`      | `tempfile::tempdir_in(zccache_core::config::tmp_dir())`                                      |
| `tempfile::TempDir::new()` | `tempfile::TempDir::new_in(zccache_core::config::tmp_dir())`                                 |
| `tempfile::NamedTempFile::new()` | `tempfile::NamedTempFile::new_in(<same dir as the rename target>)` for atomic writes   |

The `*_in(...)` variants take an explicit path and are always allowed.

The repository has legacy call sites that pre-date this lint; those files
are listed in `src/allowlist.txt`. New files are denied by default. Remove
files from the allowlist as migrations land.
