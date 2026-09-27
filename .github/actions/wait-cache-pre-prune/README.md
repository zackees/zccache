# Wait for cache pre-prune

This composite action is used by cache-writing workflows on pushes to `main`.
It waits for the `cache-pre-prune.yml` run for the exact pushed commit to
complete successfully and verifies that `main` still points to that commit
before allowing cache writes. On pull requests and non-push events, its steps
are skipped so those runs remain restore-only.

The action requires only `actions: read` and `contents: read`; it does not
change cache state.
