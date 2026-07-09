# ban_tmp_literal

Dylint library that bans hardcoded `/tmp` path string literals (`"/tmp"` or
`"/tmp/..."`) in workspace Rust code. Tracked by issue
[#828](https://github.com/zackees/zccache/issues/828).

## Why

zccache ships on Linux, macOS, and Windows. A literal `/tmp/...` path only
exists on POSIX: Windows callers either silently write to `C:\tmp\` (if it
exists, polluting the filesystem) or fail. CI on macOS/Linux passes; Windows
runners or dev boxes break. The fix is mechanical, but discovery requires a
scan — and future regressions require this lint.

## What to use instead

- **Runtime scratch state**: `zccache_core::config::tmp_dir()` — rooted under
  the ground-truth cache dir, visible to `zccache clear`, same volume as the
  destination (preserves the atomic-rename invariant). See
  `ban_unrooted_tempdir` for the companion rule that keeps scratch state out
  of `$TMPDIR` entirely.
- **Tests that need a real directory**: `tempfile::tempdir_in(...)` rooted
  under the cache dir per `ban_unrooted_tempdir`'s guidance.
- **Fixture path strings that never touch the filesystem** (map keys, parser
  inputs): any platform-neutral fake path that does not imply a real POSIX
  location, e.g. `/fixture/foo.c`.

## Allowlist

Legacy files are exempted via `src/allowlist.txt` (path-tail matching, same
mechanism as `ban_unrooted_tempdir`). The entries fall into three groups,
documented inline in that file:

1. **Deliberate `cfg(unix)` socket endpoints** — `/tmp/zccache-{user}/...`
   is the versioned daemon-discovery convention when `XDG_RUNTIME_DIR` is
   unset. Changing these would break endpoint compatibility between zccache
   versions; they are intentionally permanent exemptions.
2. **Wire-format compat pins** — fixture literals that feed pinned bincode
   fingerprints; changing the bytes breaks golden values by design.
3. **fs-inert legacy test fixtures** — map keys and parser inputs that never
   touch the filesystem. Migrate to neutral fake paths as you touch each
   file; do not add new entries in this group.

## Scope note

The workspace dylint runner (`cargo dylint --all --workspace`, via
`python3 -m ci.lint --dylint-only`) checks production targets; `#[cfg(test)]`
modules and integration tests are compiled only under `--all-targets`, which
the runner does not currently pass (a pre-existing property shared by all
dylints in this repo). The allowlist nevertheless carries the test-side legacy
files so the lint stays clean if the runner ever gains `--all-targets`.

## Running

From the repository root:

```bash
cargo dylint --lib ban_tmp_literal --workspace
```

The UI test is `#[ignore]`d until `.stderr` snapshots are captured, matching
`ban_unrooted_tempdir`; the lint is exercised end-to-end by the workspace
dylint run in CI.
