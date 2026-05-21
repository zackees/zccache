# Fixtures

Each fixture is a self-contained Rust project that simulates one
shape of CI workload. Workers materialise a fixture by extracting its
`.tar.gz` into a per-cell working directory; no Rust toolchain is
needed to unpack one.

## Current fixtures

### `medium`

Single binary crate depending on serde + derive, tokio
(rt-multi-thread + macros), clap + derive, reqwest (rustls + json),
tracing, anyhow, thiserror, chrono, and uuid. Resolves to ~200
transitive crates; targets the 30–60 s cold-build envelope on
`ubuntu-24.04` so warm-vs-cold deltas are large enough to see but the
matrix still finishes in a reasonable wall time.

`src/main.rs` references every direct dependency from `main` so the
compiler instantiates each crate and dead-code elimination cannot
prune them away.

### `sqlite-link`

A Rust binary that links libsqlite3 statically via rusqlite's
`bundled` feature. The dep graph stays small (rusqlite + serde +
anyhow, ~30 crates), but the build pipeline includes a non-trivial
C compilation stage (cc-rs / clang) for libsqlite3 itself — exactly
the fingerprint surface that `medium` does not exercise. If
soldr's caching regresses on build-script outputs or native-linker
artifacts, this row goes red before `medium` does.

Requires a C compiler on the worker. CI workers `apt-get install gcc`
on demand; local builds rely on whatever cc-rs already finds.

## Regenerating tarballs

```bash
bash perf/fixtures/regen.sh medium
```

`regen.sh` is deterministic: same source tree + same Cargo.lock means
identical bytes (sorted entries, owner/group stripped, mtime pinned
to epoch). Running it on an unchanged source tree produces no
`git diff`.

To bump transitive versions:

```bash
(cd perf/fixtures/medium && soldr cargo generate-lockfile)
bash perf/fixtures/regen.sh medium
git add perf/fixtures/medium/Cargo.lock perf/fixtures/medium.tar.gz
```
