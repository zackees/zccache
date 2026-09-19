# ban_std_pathbuf

This lint bans `std::path::PathBuf` in workspace code and directs developers to
`zccache_core::path::NormalizedPath` instead. The narrowly approved product
`platform.rs` adapters are exempt only where a canonical kernal-api capability
requires a primitive path. Callers normalize product paths at their boundary.

The repository still has legacy `PathBuf` call sites, so the lint carries a
file-level allowlist for those modules. New files are denied by default. Remove
files from `src/allowlist.txt` as migrations land.
