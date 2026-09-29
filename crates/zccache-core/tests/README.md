# zccache-core integration tests

- `mtime_owner_workspace.rs` - #1771 guard: only `src/mtime.rs` (plus an
  explicit, reasoned allowlist) may set a file time anywhere in the workspace.
