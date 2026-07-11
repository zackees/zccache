# PERF.md — testing zccache performance

zccache has one performance workflow today: **`.github/workflows/perf-rust-cluster.yml`** (the Rust perf cluster). It exercises zccache against Rust workloads by **building `soldr` and `zccache` from source on every run** (plain `cargo build --release` + `Swatinem/rust-cache` + `mold` linker — no soldr/zccache wrapping cargo, that would be circular), then wires them together via the new sticky `soldr update-zccache <dir>` API so the freshly-built zccache is the one under test. A future **`perf-cpp-cluster.yml`** will mirror this shape for C/C++ workloads (clang/gcc + zccache).

For the per-scenario design rationale (what each cell proves), see [`perf/README.md`](perf/README.md).

> **Embedded integration update:** soldr now embeds zccache. The cluster checks the zccache commit under test out inside soldr's vendored submodule before building soldr; the historical runtime pin command is no longer used.

## Setup

No secrets required. Both repos are public; `actions/checkout` reads them with the default `GITHUB_TOKEN`.

If either source tree at `main` HEAD fails to build, the `build-binaries` job fails loudly with the cargo error message — that is the fail-loud signal that the upstream is broken.

### Why build from source instead of consuming pre-built artifacts?

Two reasons:
- **`soldr` and `zccache` are the binaries under test.** Building them with themselves (wrapping `cargo` with `soldr` or `zccache`) would either bootstrap them off prior cached versions of themselves, or measure the bootstrap rather than the cache. Plain `cargo build --release` sidesteps both pitfalls.
- **`main` HEAD is the substance of the test.** Whatever soldr or zccache do today on `main` is what the perf cluster measures today. Artifact consumption would lag by however stale the upstream's most recent dispatched build happens to be (soldr's `release-auto.yml` only fires on releases; zccache's `build.yml` is `workflow_dispatch`-only).

### What about speed?

`Swatinem/rust-cache` caches cargo intermediates per-repo per-platform; warm rebuilds are typically a few minutes. `mold` linker is installed on Linux to shorten the final-link step. The first run on a new platform is slower because both caches are empty.

## How it triggers

The workflow fires on:

1. **`workflow_dispatch`** — the "Run workflow" button in the Actions UI. Dispatch inputs are used verbatim and the branch name is ignored.
2. **`push`** to `main`, `perf/**`, or `evaluate/**`. The branch name is parsed into an effective `(platforms, fixtures, scenarios)` scope (see below). The dispatch inputs are not consulted.
3. **Manual `gh workflow run` CLI** — same as the dispatch button.

The full matrix is always loaded; cells that fall outside the resolved scope skip themselves at the gate step. **`main` defaults to the `medium` fixture only** to keep per-push cycle time short; push to `perf/all` (or `perf/all-sqlite-*`) when you want `sqlite-link` measured too — its bundled `libsqlite3` adds ~3-5 min of `cc-rs` build per cell.

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
| Scenario | `restore` | `restore-no-clean-warm` |
| any axis | `all` | wildcard — run every value on this axis |

The full scope table below is exhaustive for the original three scenarios; for the newer `restore` token, every pattern in the table has a parallel `…-restore` form (e.g. `perf/linux-medium-restore` runs the single cell linux × medium × restore-no-clean-warm).

Short tokens keep the branch name unambiguous (the real names contain hyphens that would collide with the axis separator).

### Full scope table

48 hierarchical patterns plus two opt-in-full aliases. Anything not in this table (e.g., a developer iteration branch like `perf/cluster-hierarchical-skip`) falls back to the **default scope (`medium` fixture, all scenarios)** and emits a `::notice::` so the run is still useful. Use `perf/all` if you want the full fixture sweep included.

#### Aliases — full ride

| Branch | Scope |
|---|---|
| `main` | every platform × `medium` fixture × every scenario (use `perf/all` for full sweep) |
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

> Today, only `linux` has a matrix row in `build-binaries` / `bench`. `win` and `mac` branches resolve correctly via setup but their cells gate out until cross-platform runner rows land. The branch names stay stable.

## Picking a branch for the work you're doing

- **Iterating on cache hit-rate fixes that only affect sqlite builds** → `perf/linux-sqlite-cold` (fastest signal: one cell, the hard gate scenario).
- **Tuning archive fidelity** → `perf/all-all-cold` (sweep cold-tar-untar-warm across everything; fixture variation matters).
- **Worktree path-remap change** → `perf/linux-all-worktree` (every fixture on linux, worktree scenario only).
- **Just experimenting / unsure** → `perf/all` for the full sweep, or push to `main` for the default scope (medium fixture, all scenarios) — fastest cycle that still covers all three scenarios.
- **Personal feature branch like `perf/wip/foo`** → falls through to the default scope (medium, all scenarios) with a `::notice::`. Fine for one-off runs; rename to a canonical pattern when you know what axis you're working on.

## Gate semantics

Every scenario gates on **`speedup >= min_speedup` AND (optionally) `warm_ms
<= max_warm_ms_<scen>`** — both must hold for PASS. The warm-ms ceiling is
opt-in per scenario; scenarios with no ceiling gate on speedup alone.

Linux thresholds today:

| Scenario | min_speedup | max_warm_ms | Mode |
|---|---:|---:|---|
| `cold-tar-untar-warm` | `4.5x` | — | **hard** (fails workflow) |
| `restore-no-clean-warm` | `4.5x` | `1500ms` | **hard** (fails workflow) |
| `worktree-share` | `4.5x` | — | soft (`::warning::`) |
| `touch-no-change` | `4.5x` | — | soft (`::warning::`) |

Why both gates instead of speedup-only: some scenarios have cold-side compile
time that dominates the speedup ratio (e.g. `restore-no-clean-warm`: cargo
populates `target/` from scratch on cold, so a real 20× warm regression — warm
going from 75 ms back to 1500 ms — still reports a ~40x speedup and passes a
speedup-only gate cleanly). The warm-ms ceiling catches user-visible regressions
that hide behind a high ratio. Other scenarios (`cold-tar-untar-warm`) want
speedup as the signal of cache contribution and don't currently set a warm-ms
ceiling.

Thresholds live on the `evaluate` matrix row (`min_speedup`,
`max_warm_ms_<scen>`). Each platform row carries its own values; the workflow
has only `linux` today, so the numbers above are linux-tuned. mac-arm / win
rows will need their own thresholds because per-OS variance on the noise floor
is significant. To add a warm-ms ceiling to a scenario that doesn't have one,
set the corresponding `max_warm_ms_<scen>` field in the matrix include to a
non-empty value (e.g. `"2000"`); the gate code picks it up via
`max_warm_ms_for`.

## Reading the run

Every cell appends to `$GITHUB_STEP_SUMMARY`. From the run page:

1. **Scope** table at the top (`setup` job) — confirms the resolved `(platforms, fixtures, scenarios)` and the source (`branch:<ref>`, `alias:main`, `dispatch`, `unknown-perf:<ref>`, etc.).
2. **bench** cells emit a per-fixture table with `cold/A ms | warm/B ms | speedup | hits/misses | hit rate | peak daemon RSS`.
3. **Evaluate** cell emits a single per-platform table covering every (fixture, scenario) it could find, with `cold | warm | speedup | threshold | mode | result`.
4. Failed runs annotate the failing rows with `::error::` lines (visible in the "Annotations" sidebar).

Raw `result.json`, `*-shutdown.json`, and `rss-*.csv` are uploaded as `perf-results-<platform>-<fixture>` artifacts (14-day retention).

## Iterating on a perf problem — local-first, GHA last

When you find (or suspect) a perf regression, the bias is **reproduce and fix locally first**. GHA is the gate; the local loop is the iteration loop. One GHA cycle is 5–17 minutes; one local cycle is seconds to a couple of minutes. Burning GHA cycles on hypotheses you haven't tried locally is the slowest possible workflow.

The flow:

1. **Reproduce locally.** Pick the narrowest scenario that surfaces the problem and run it on the same fixture the GHA job hit. See [Local dry-runs](#local-dry-runs) below for the one-liner. Capture the JSON `{"scenario":...}` line — that's your baseline.
2. **Form one hypothesis.** Don't change two things at once or you'll lose attribution.
3. **Edit + re-run the local scenario.** Compare cold_ms, warm_ms, ratio (warm/cold), hits, misses against your baseline. Iterate locally until the JSON line says you fixed it.
4. **Only now push.** Open a `perf/<plat>-<fix>-<scen>` branch matched to the narrowest cell that exercises your fix (see [Picking a branch](#picking-a-branch-for-the-work-youre-doing)). Watch the GHA run to confirm the local result reproduces on the cluster's hardware.
5. **Iterate on GHA only when you must.** If the bug only reproduces under GHA's environment (older glibc, different filesystem, specific runner image), you've earned the right to push uncertain hypotheses. Note in the commit message that local repro failed and why — future-you will want that context.
6. **Lock the fix in with a perf unit test.** See [Preventing regressions](#preventing-regressions--add-a-perf-unit-test). Without a test, the bug comes back the next time someone refactors the affected path.

The cluster is the regression-blocking measurement, but it is a bad iteration loop. Use it accordingly.

## Preventing regressions — add a perf unit test

Every perf bug you fix should leave a test behind that fails when the regression returns. Tests are the spec: if the perf characteristic isn't tested, it doesn't exist. The perf cluster catches scenario-level cold/warm collapses, but a tight unit test pins down the specific function or path that was slow and makes the future fix obvious.

Two venues, picked by what you're protecting:

**1. Compile-time speed floors (C / C++ / Rust vs bare + vs sccache).** Extend [`crates/zccache-daemon/tests/perf_bench_test.rs`](crates/zccache-daemon/tests/perf_bench_test.rs) with a new `#[test] #[ignore]` benchmark, then add a threshold row in [`ci/perf_guard.py`](ci/perf_guard.py) so [`.github/workflows/perf-guard.yml`](.github/workflows/perf-guard.yml) gates on it on every push to main. The existing rows are your template (`perf_c_zccache_vs_bare`, `perf_warm_cache_zccache_vs_sccache`, etc.).

**2. Function- or path-level budget assertions.** For a regression scoped to a single function (e.g. "session-end took 200 ms when it used to take 5 ms"), add a `#[test]` in the relevant crate that asserts a `Duration` budget:

```rust
#[test]
fn session_end_under_50ms() {
    let t = Instant::now();
    daemon.session_end(session_id).unwrap();
    let elapsed = t.elapsed();
    assert!(elapsed < Duration::from_millis(50), "session_end took {:?}", elapsed);
}
```

Pick the budget at ~3× the post-fix measurement so machine variance doesn't make the test flaky. Mark `#[ignore]` if the test is too slow for the normal suite — `./test --full` picks up ignored tests and CI runs that variant.

**Naming convention:** prefix the test with `perf_` so it's discoverable and so a future cleanup can grep them all. Reference the issue number in a comment (e.g. `// regression test for #320`) — future-you will want to find why the budget was picked.

**Avoid `criterion` / `divan`** for these tests. The point is a single hard assertion that flips a CI red, not a statistical comparison — those tools are for diagnosis (PERF.md → "Iterating on a perf problem"), not regression gates.

## Local dry-runs

### Recommended: Docker harness (`ci/perf_local.py`)

Three-image Docker harness that reproduces one perf-cluster cell on the host
machine without burning a GHA cycle:

```bash
uv run python ci/perf_local.py                      # cold-tar-untar-warm × medium (default)
uv run python ci/perf_local.py --scenario worktree-share
uv run python ci/perf_local.py --fixture sqlite-link
uv run python ci/perf_local.py --rebuild-images     # force docker build of all 3 images
uv run python ci/perf_local.py --skip-soldr-build   # fast iteration after zccache-only change
```

Architecture:

- **`zccache-perf-soldr-builder`** — rust:alpine + musl-dev. Builds the
  static `soldr` binary at `.perf-local/binaries/soldr/soldr`.
- **`zccache-perf-zccache-builder`** — rust:bookworm. Builds the
  `zccache` trio at `.perf-local/binaries/zccache/`.
- **`zccache-perf-runner`** — rust:bookworm + bash/tar/zstd/jq. Mounts
  both binary dirs + the zccache source for `perf/scenarios/`, runs the
  scenario, writes `result.json` + cache reports under
  `.perf-local/results/<scenario>/`.

Source code is **volume-mounted** into the builder containers, and
`target/` lives in a persistent host-side volume — so cargo incremental
recompiles only the crates that changed. First run is full (~5-8 min);
subsequent runs after editing one crate finish in seconds. Image
rebuilds are only needed when a `ci/docker/*.Dockerfile` changes
(force with `--rebuild-images`).

The orchestrator prints the same rich Evaluate-style summary table the
GHA workflow emits, so a local result is directly comparable to a cluster
result row-for-row.

See [`ci/docker/README.md`](ci/docker/README.md) for the full mount layout.

### Bare-bash dry-run (no Docker, Linux only)

For when Docker isn't available and you're on a Linux host that already has
the perf script's deps installed (bash, tar, zstd, jq, plus a soldr +
zccache pair on PATH):

```bash
# Set up the fixture, then run one scenario (writes result.json to stdout)
bash perf/lib/extract.sh medium /tmp/perf-medium && bash perf/scenarios/cold-tar-untar-warm/run.sh /tmp/perf-medium/medium
```

Swap `medium` → `sqlite-link` for the other fixture, and `cold-tar-untar-warm`
→ `worktree-share` / `touch-no-change` for the other two scenarios. The
scripts are POSIX bash and do not require any GHA-only env vars;
`measure::append_summary_md` is a no-op when `$GITHUB_STEP_SUMMARY` is unset.

To diff between runs, re-pipe the result.json into a file and
`jq -r 'to_entries | map("\(.key)=\(.value)") | join(" ")'` it — keys appear
in a stable order, so visual diff works.

## Related

- **Issue [#320](https://github.com/zackees/zccache/issues/320)** — the cold_skip regression that motivated this workflow.
- **[soldr's PERF.md](https://github.com/zackees/soldr/blob/main/PERF.md)** — the upstream pattern this workflow is adapted from.
