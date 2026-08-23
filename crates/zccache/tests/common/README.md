# Shared integration-test helpers

Small utilities used by more than one integration test in
`crates/zccache/tests/`. Included with `mod common;`, so unused items need
`#[allow(dead_code)]` — a test binary that does not call a helper would
otherwise fail the workspace's `-D warnings`.

- `create_file` — write a file, creating parent directories.
- `rel_paths` — relative paths from `ScannedFile`s, for assertions.
- `wait_for_mtime_change` — sleeps past filesystem mtime granularity. Windows
  NTFS does not always advance mtime on rapid successive writes, so tests that
  depend on an mtime change must wait rather than assume.
