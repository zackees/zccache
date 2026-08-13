# ban_std_pathbuf

This lint bans `std::path::PathBuf` in workspace code and directs developers to
`zccache_core::path::NormalizedPath` instead. `crates/zccache-platform/src/**`
is structurally exempt: that crate is the dependency leaf for raw host
mechanics, cannot depend on `zccache-core`, and deliberately exposes primitive
path results for callers to normalize at the product boundary.

The repository still has legacy `PathBuf` call sites, so the lint carries a
file-level allowlist for those modules. New files are denied by default. Remove
files from `src/allowlist.txt` as migrations land.
