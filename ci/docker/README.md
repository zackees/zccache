# Local Perf Harness Docker Images

Three Docker images that together let you reproduce the `perf-rust-cluster.yml`
GHA scenarios on the host machine without burning a GHA cycle. Orchestrated by
[`../perf_local.py`](../perf_local.py).

## Why three images instead of one big multi-stage build

A single multi-stage `Dockerfile` would re-COPY the entire source tree on every
iteration, busting Docker's layer cache the moment a single `.rs` file changed.
That's the same wall-time as a GHA cycle, defeating the point.

By splitting into a **builder pair** (one per Rust project) plus a separate
**runner** image, sources are mounted as volumes — the cargo target/ dir lives
in a host-side volume, so `cargo build` uses incremental compilation across
runs. First run is full (5-8 min), subsequent runs are seconds when only a few
crates changed.

## The three images

| Image tag                          | Built from                  | Role                                                                                                |
|------------------------------------|-----------------------------|-----------------------------------------------------------------------------------------------------|
| `zccache-perf-soldr-builder`       | `soldr-builder.Dockerfile`  | rust:alpine + musl-dev. Volume-mount soldr source at `/src`, persistent `/target`, drop static `soldr` binary at `/out/soldr`. |
| `zccache-perf-zccache-builder`     | `zccache-builder.Dockerfile`| rust:bookworm development image used by the local test, lint, and shell subcommands. |
| `zccache-perf-runner`              | `runner.Dockerfile`         | Runs scenarios with soldr embedding this checkout's committed zccache HEAD. |

## Volume conventions

All volumes are managed by the orchestrator. The layout under `<repo>/.perf-local/`:

```
.perf-local/
├── soldr-src/                  # shallow clone of soldr@main (refreshed by orchestrator)
├── target/
│   ├── soldr/                  # persistent cargo target/ for soldr (musl)
│   └── zccache/                # persistent cargo target/ for zccache (gnu)
├── cargo-home/                 # persistent CARGO_HOME (registry index + crate sources)
│   ├── soldr/                  # so no-op rebuilds don't re-fetch ~175 MiB per run
│   └── zccache/
├── binaries/
│   └── soldr/soldr             # static soldr binary with zccache embedded
└── results/
    └── <scenario>/             # result.json + cache reports per run
```

The `cargo-home/` volumes are required for fast incremental rebuilds: the
Dockerfiles set `ENV CARGO_HOME=/cargo-home` so cargo's fingerprint check
loads registry sources from there. Without a host-side mount, every run
starts with an empty registry and pays a re-download + re-fingerprint
cost (~20-25s for soldr, ~30s for zccache). Persisting them drops no-op
rebuilds to under 15s wall time on Docker-for-Windows.

Everything under `.perf-local/` is `.gitignore`d.

## Image rebuild policy

Builder images are rebuilt only when their Dockerfile changes — the orchestrator
checks `docker images -q <tag>` and skips build if a layer exists. To force a
rebuild (e.g. after pinning a new toolchain), pass `--rebuild-images` to
`perf_local.py`.

Source changes do NOT trigger image rebuilds — they trigger a cargo recompile
inside the running container, which reuses the persistent `target/` volume.
