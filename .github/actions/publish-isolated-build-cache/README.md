# `publish-isolated-build-cache` composite action

A job that compiles under an isolated `SOLDR_CACHE_DIR` (setup-soldr's
`seed-isolated-build-cache`) seeds that store from the build-cache setup-soldr
restored, but setup-soldr's post step saves only its own store. Without this
action the isolated store, which holds everything the job compiled, is thrown
away at job end, and the next run's compiles all miss (#1867).

This action replaces setup-soldr's build-cache store with the isolated one, so
the post step saves it under the job's key. Session logs are copied too, so
setup-soldr's save gate and its `final zccache session stats` line see the
isolated session; the build-cache save profile trims them from the archive.

## Inputs

- `isolated-soldr-cache-dir` (required): the isolated `SOLDR_CACHE_DIR`. Stop
  its daemon first (`soldr cache shutdown`), so its index is flushed.
- `build-cache-path` (required): setup-soldr's `build-cache-path` output. When
  it is empty (build-cache off), or the isolated store is missing, the action
  does nothing.

Give the calling job its own `cache-key-suffix`: a key shared with other jobs
is saved by whichever finishes first. Run it with `if: always()` after the
step that stops the isolated daemon.

Tests: `ci/tests/test_isolated_build_cache.py`.
