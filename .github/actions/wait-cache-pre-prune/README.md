# Wait for cache pre-prune

This composite action is used by cache-writing workflows on pushes to `main`.
It waits for the `cache-pre-prune.yml` run for the exact pushed commit to
complete successfully and verifies that `main` still points to that commit,
then exports its decision as `ZCCACHE_CACHE_WRITES=true|false` to the rest of
the job. On pull requests and non-push events its steps are skipped, the
variable stays unset, and those GitHub runs remain restore-only.

It never fails the job. When the pre-prune run failed (for example its
forecast failed closed), `main` advanced past this commit, or the wait timed
out, it emits a warning and exports `ZCCACHE_CACHE_WRITES=false`: the job still
builds and tests, but uploads no remote caches. Normal setup-soldr writers
use global `save-cache: auto` with a `save-cache-remote` expression requiring
`env.ZCCACHE_CACHE_WRITES == 'true'` (`REMOTE_SAVE_CACHE_POLICY` in
`ci/check_cache_footprint.py`, enforced by that script). The
`cache-pre-prune.yml` run itself still goes red when its forecast refuses, so
a budget problem stays visible in one workflow instead of failing all of them.

The published setup-soldr action ignores remote permission locally while
keeping automatic saves warm. An explicit global `save-cache: false` remains
read-only everywhere; this action never overrides it.

The action requires only `actions: read` and `contents: read`; it does not
change cache state.
