# PERF.md — testing zccache performance

zccache has one performance workflow today: **`.github/workflows/perf-rust-cluster.yml`** (the Rust perf cluster). It exercises zccache against Rust workloads using pre-built `soldr` and `zccache` binaries pulled from each repo's build pipeline artifacts (`zccache/build.yml` for the zccache trio, `soldr/release-auto.yml` for the soldr binary), wired together via the new sticky `soldr update-zccache <dir>` API so the cluster can install a freshly-built zccache into a freshly-built soldr toolchain home. A future **`perf-cpp-cluster.yml`** will mirror this shape for C/C++ workloads (clang/gcc + zccache).

For the per-scenario design rationale (what each cell proves), see [`perf/README.md`](perf/README.md).

## Setup

This workflow downloads pre-built binaries from the `zackees/soldr` and `zackees/zccache` build pipelines. Cross-repo artifact reads require a fine-grained Personal Access Token.

1. Create a fine-grained PAT at <https://github.com/settings/personal-access-tokens/new> with `Actions: Read` scope on **both** `zackees/soldr` and `zackees/zccache`.
2. Add the token as repo secret `CROSS_REPO_PAT` in this repo's Settings → Secrets and variables → Actions.

If `CROSS_REPO_PAT` is missing or expired, the `fetch-binaries` job fails loudly with a clear error message — no silent skipping.

Also required: both repos must have at least one **successful** main-branch run of their respective build pipeline (`zackees/zccache:.github/workflows/build.yml` and `zackees/soldr:.github/workflows/release-auto.yml`). If either is missing or broken, the workflow fails loudly. Trigger them via the Actions tab → `Run workflow` on `main` if needed. Note that soldr's `release-auto.yml` only fires on version bumps in `Cargo.toml` or tag pushes — a stale main may need a manual dispatch before the perf cluster has artifacts to consume.

### Why two repos for the binaries?

We deliberately do not build soldr or zccache inside the perf job. Building soldr requires soldr (toolchain bootstrap), and building zccache benefits from a working soldr + zccache (compiler caching). Doing both inside the perf job would either circularly depend on the artifacts under test or measure the bootstrap rather than the cache. Pulling artifacts from each repo's `build.yml` cleanly separates "produce a binary" from "measure that binary."

## How it triggers

The workflow fires on:

1. **`workflow_dispatch`** — the "Run workflow" button in the Actions UI. Dispatch inputs are used verbatim and the branch name is ignored.
2. **`push`** to `main`, `perf/**`, or `evaluate/**`. The branch name is parsed into an effective `(platforms, fixtures, scenarios)` scope (see below). The dispatch inputs are not consulted.
3. **Manual `gh workflow run` CLI** — same as the dispatch button.

The full matrix is always loaded; cells that fall outside the resolved scope skip themselves at the gate step. `main` always runs the full sweep.

## Branch-name convention

Branch syntax: **`perf/<plat>-<fix>-<scen>`** with one short token per axis. `all` is the wildcard at any axis.

### Token mapping

| Axis | Branch token | Real value |
|---|---|---|
| Platform | `linux` | `linux` |
| Platform | `win` | `win` |
| Platform | `mac` | `mac-arm` |
| Fixture | `medium` | `medium` |
| Fixture | `sqlite` | `sqlite-link` |
| Scenario | `cold` | `cold-tar-untar-warm` |
| Scenario | `worktree` | `worktree-share` |
| Scenario | `touch` | `touch-no-change` |
| any axis | `all` | wildcard — run every value on this axis |

Short tokens keep the branch name unambiguous (the real names contain hyphens that would collide with the axis separator).

### Full scope table

48 hierarchical patterns plus two full-sweep aliases. Anything not in this table (e.g., a developer iteration branch like `perf/cluster-hierarchical-skip`) falls back to a full sweep and emits a `::notice::` so the run is still useful.

#### Aliases — full ride

| Branch | Scope |
|---|---|
| `main` | every platform × fixture × scenario |
| `perf/all` | same as `perf/all-all-all` |

#### Platform = `all` (12)

| Branch | Scope |
|---|---|
| `perf/all-all-all` | full sweep |
| `perf/all-all-cold` | every platform, every fixture, **cold** only |
| `perf/all-all-worktree` | every platform, every fixture, **worktree-share** only |
| `perf/all-all-touch` | every platform, every fixture, **touch-no-change** only |
| `perf/all-medium-all` | every platform, **medium** fixture, every scenario |
| `perf/all-medium-cold` | every platform, **medium**, cold only |
| `perf/all-medium-worktree` | every platform, **medium**, worktree only |
| `perf/all-medium-touch` | every platform, **medium**, touch only |
| `perf/all-sqlite-all` | every platform, **sqlite-link**, every scenario |
| `perf/all-sqlite-cold` | every platform, **sqlite-link**, cold only |
| `perf/all-sqlite-worktree` | every platform, **sqlite-link**, worktree only |
| `perf/all-sqlite-touch` | every platform, **sqlite-link**, touch only |

#### Platform = `linux` (12)

| Branch | Scope |
|---|---|
| `perf/linux-all-all` | **linux** only, every fixture × scenario |
| `perf/linux-all-cold` | linux, every fixture, cold only |
| `perf/linux-all-worktree` | linux, every fixture, worktree only |
| `perf/linux-all-touch` | linux, every fixture, touch only |
| `perf/linux-medium-all` | linux + medium, every scenario |
| `perf/linux-medium-cold` | **single cell**: linux × medium × cold |
| `perf/linux-medium-worktree` | **single cell**: linux × medium × worktree |
| `perf/linux-medium-touch` | **single cell**: linux × medium × touch |
| `perf/linux-sqlite-all` | linux + sqlite-link, every scenario |
| `perf/linux-sqlite-cold` | **single cell**: linux × sqlite-link × cold |
| `perf/linux-sqlite-worktree` | **single cell**: linux × sqlite-link × worktree |
| `perf/linux-sqlite-touch` | **single cell**: linux × sqlite-link × touch |

#### Platform = `win` (12)

| Branch | Scope |
|---|---|
| `perf/win-all-all` | **win** only, every fixture × scenario |
| `perf/win-all-cold` | win, every fixture, cold only |
| `perf/win-all-worktree` | win, every fixture, worktree only |
| `perf/win-all-touch` | win, every fixture, touch only |
| `perf/win-medium-all` | win + medium, every scenario |
| `perf/win-medium-cold` | **single cell**: win × medium × cold |
| `perf/win-medium-worktree` | **single cell**: win × medium × worktree |
| `perf/win-medium-touch` | **single cell**: win × medium × touch |
| `perf/win-sqlite-all` | win + sqlite-link, every scenario |
| `perf/win-sqlite-cold` | **single cell**: win × sqlite-link × cold |
| `perf/win-sqlite-worktree` | **single cell**: win × sqlite-link × worktree |
| `perf/win-sqlite-touch` | **single cell**: win × sqlite-link × touch |

#### Platform = `mac` (mac-arm) (12)

| Branch | Scope |
|---|---|
| `perf/mac-all-all` | **mac-arm** only, every fixture × scenario |
| `perf/mac-all-cold` | mac, every fixture, cold only |
| `perf/mac-all-worktree` | mac, every fixture, worktree only |
| `perf/mac-all-touch` | mac, every fixture, touch only |
| `perf/mac-medium-all` | mac + medium, every scenario |
| `perf/mac-medium-cold` | **single cell**: mac × medium × cold |
| `perf/mac-medium-worktree` | **single cell**: mac × medium × worktree |
| `perf/mac-medium-touch` | **single cell**: mac × medium × touch |
| `perf/mac-sqlite-all` | mac + sqlite-link, every scenario |
| `perf/mac-sqlite-cold` | **single cell**: mac × sqlite-link × cold |
| `perf/mac-sqlite-worktree` | **single cell**: mac × sqlite-link × worktree |
| `perf/mac-sqlite-touch` | **single cell**: mac × sqlite-link × touch |

> Today, only `linux` has a matrix row in `fetch-binaries` / `bench`. `win` and `mac` branches resolve correctly via setup but their cells gate out until cross-platform runner rows land. The branch names stay stable.

## Picking a branch for the work you're doing

- **Iterating on cache hit-rate fixes that only affect sqlite builds** → `perf/linux-sqlite-cold` (fastest signal: one cell, the hard gate scenario).
- **Tuning archive fidelity** → `perf/all-all-cold` (sweep cold-tar-untar-warm across everything; fixture variation matters).
- **Worktree path-remap change** → `perf/linux-all-worktree` (every fixture on linux, worktree scenario only).
- **Just experimenting / unsure** → `perf/all` or `main` — full sweep; the workflow handles the volume.
- **Personal feature branch like `perf/wip/foo`** → falls through to full sweep with an `::notice::`. Fine for one-off runs; rename to a canonical pattern when you know what axis you're working on.

## Gate semantics

- **`cold-tar-untar-warm` < 3x** (cold/warm ratio in the Evaluate step) → **fails the workflow**. Hard gate.
- **`worktree-share` < 3x** → emits `::warning::`, doesn't fail. Soft gate today; promotes to hard once the baseline stabilizes.
- **`touch-no-change` < 3x** → same as worktree-share, soft today.

Threshold lives on the `evaluate` matrix row (`min_speedup: "3.0"`).

## Reading the run

Every cell appends to `$GITHUB_STEP_SUMMARY`. From the run page:

1. **Scope** table at the top (`setup` job) — confirms the resolved `(platforms, fixtures, scenarios)` and the source (`branch:<ref>`, `alias:main`, `dispatch`, `unknown-perf:<ref>`, etc.).
2. **bench** cells emit a per-fixture table with `cold/A ms | warm/B ms | speedup | hits/misses | hit rate | peak daemon RSS`.
3. **Evaluate** cell emits a single per-platform table covering every (fixture, scenario) it could find, with `cold | warm | speedup | threshold | mode | result`.
4. Failed runs annotate the failing rows with `::error::` lines (visible in the "Annotations" sidebar).

Raw `result.json`, `*-shutdown.json`, and `rss-*.csv` are uploaded as `perf-results-<platform>-<fixture>` artifacts (14-day retention).

## Local dry-runs

You can run any single scenario locally without GHA:

```bash
# Set up the fixture, then run one scenario (writes result.json to stdout)
bash perf/lib/extract.sh medium /tmp/perf-medium && bash perf/scenarios/cold-tar-untar-warm/run.sh /tmp/perf-medium/medium
```

The scripts are POSIX bash and do not require any GHA-only env vars; `measure::append_summary_md` is a no-op when `$GITHUB_STEP_SUMMARY` is unset.

## Related

- **Issue [#320](https://github.com/zackees/zccache/issues/320)** — the cold_skip regression that motivated this workflow.
- **[soldr's PERF.md](https://github.com/zackees/soldr/blob/main/PERF.md)** — the upstream pattern this workflow is adapted from.
