# ban_std_pathbuf — sources

- `lib.rs` — late-pass visitor that fires on `std::path::PathBuf` type
  references and `PathBuf::*` associated-function references.
- `allowlist.txt` — newline-separated tail-suffix matches for source files
  that pre-date the lint and still use raw `PathBuf`. Remove entries as
  call sites migrate to `zccache_core::path::NormalizedPath`.

See the parent directory's `README.md` for the user-facing rationale.
