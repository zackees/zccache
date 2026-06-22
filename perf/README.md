# soldr performance cluster

`workflow_dispatch`-triggered GitHub Actions workflow that measures
soldr's cache hit rate, daemon memory, and on-disk footprint across a
three-axis matrix:

- **Platform** — which OS the worker runs on. v2 ships `linux`; `win`
  and `mac-arm` / `mac-x86` slot in as matrix-row additions once the
  RSS sidecar in `lib/common.sh` grows cross-platform branches.
- **Fixture** — what is being cached. `medium` (pure-Rust dep graph)
  and `sqlite-link` (Rust + a bundled C library) today.
- **Scenario** — how the cache flows between invocations. Three
  scenarios today, each pinning a single failure mode.

Dispatch inputs accept comma-separated subsets per axis (or `all`)
so a one-cell debug run is a single picker tweak in the Actions UI.

## Why this exists

A "single speed demo" tells you that soldr is slow, never *why*.  Each
matrix cell in this cluster pins a single failure mode:

| Scenario              | What breaks when this cell turns red                |
| --------------------- | --------------------------------------------------- |
| `build-then-check`    | cross-verb build/check cache reuse (soldr#758)       |
| `cold-tar-untar-warm` | cache archive fidelity (tar/untar round-trip)        |
| `worktree-share`      | `ZCCACHE_PATH_REMAP=auto` injection (issue #352)     |
| `touch-no-change`     | mtime/content-hash robustness (soldr save/load #377) |

## How a worker measures

Each worker (one matrix cell) does the same four things and emits a
single JSON line plus a markdown row to `$GITHUB_STEP_SUMMARY`:

1. **Hit rate** comes from `soldr cache report --json` /
   `soldr session-end --json` — soldr already exposes per-session
   `hits`, `misses`, `compilations`, `hit_rate`, plus per-extension
   rollups when `zccache analyze` is available.
2. **Memory** comes from a bash sidecar that polls
   `ps -o pid,rss,vsz,comm` once per second into a CSV filtered to
   `zccache-daemon|rustc|cargo`. Peak and p95 RSS are computed
   post-hoc from the CSV.
3. **Disk footprint** is `du -sb $SOLDR_CACHE_DIR/cache/zccache` plus
   the size of any intermediate tarball.
4. **Wall time** is wrapped around each build step.

The raw CSV and JSON payloads are uploaded as
`perf-results-<platform>-<fixture>-<scenario>` artifacts so you can
re-analyse a run without re-firing the workflow.

## How the master build is cached

The `build-soldr` job uses two layered caches keyed by platform so
the second dispatch on the same soldr commit is essentially free:

1. **`actions/cache`** keyed by
   `soldr-bin-<platform>-<hashFiles('crates/**','Cargo.{toml,lock}')>`
   over `target/release/soldr` — same source, same platform, no
   compile.
2. **`Swatinem/rust-cache@v2`** under that, exercised only on a
   cache miss, so the rare rebuild is incremental.

Soldr itself is deliberately **not** used to build soldr in this
workflow. The perf cluster has to keep working when soldr is broken
or absent on a new platform, so the bootstrap path stays on bare
cargo + stock GHA caches.

## Layout

```
perf/
├── fixtures/
│   ├── medium/             # pure-Rust dep graph (~200 crates)
│   ├── medium.tar.gz
│   ├── sqlite-link/        # Rust + bundled C (libsqlite3 via cc-rs)
│   ├── sqlite-link.tar.gz
│   └── regen.sh            # rebuilds <name>.tar.gz from <name>/
├── lib/
│   ├── common.sh           # measure::* helpers (rss poller, du, summary)
│   └── extract.sh          # untar a fixture into $WORKDIR
├── scenarios/
│   ├── build-then-check/run.sh
│   ├── cold-tar-untar-warm/run.sh
│   ├── worktree-share/run.sh
│   └── touch-no-change/run.sh
└── README.md               # this file
```

## Adding a new fixture

1. `mkdir perf/fixtures/<name>` with a self-contained Rust project.
   The Cargo.toml MUST declare `[workspace]` so it does not get
   folded into the parent soldr workspace.
2. `(cd perf/fixtures/<name> && soldr cargo generate-lockfile)` to
   pin transitive versions.
3. `bash perf/fixtures/regen.sh <name>` to produce the tarball.
4. Commit both the source tree and the new tarball.

## Adding a new scenario

1. `mkdir perf/scenarios/<name>` with a `run.sh` that takes the
   fixture's working directory as its first positional argument and
   writes a single JSON line to stdout.
2. Add the scenario name to the matrix in
   `.github/workflows/perf-cluster.yml`.

## Running locally

```bash
# Extract the fixture into a scratch dir and run one scenario:
WORKDIR=$(mktemp -d)
bash perf/lib/extract.sh medium "${WORKDIR}"
bash perf/scenarios/touch-no-change/run.sh "${WORKDIR}/medium"
```

`SOLDR_DEBUG=1` keeps the raw RSS CSV around (otherwise it is deleted
at the end of the scenario).
