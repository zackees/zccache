# PERF.md — testing zccache performance

zccache's scenario-level performance gate runs locally in Linux Docker through
[`ci/perf_local.py`](ci/perf_local.py). GitHub Actions does not run the large
wall-clock fixture matrix. Hosted runners remain responsible for deterministic
unit/integration checks and platform filesystem correctness, while the local
Docker gate supplies repeatable timing and retained evidence on known hardware.

For what each scenario proves, see [`perf/README.md`](perf/README.md).

## Authoritative release gate

Run the complete matrix from a clean committed checkout:

```powershell
uv run --no-project ci/perf_local.py --matrix
```

For threshold audits, repeat every cell and retain the per-cell distribution
under `.perf-local/results/<fixture>/<scenario>/repeat-summary.json`:

```powershell
uv run --no-project ci/perf_local.py --matrix --repeat 5
```

Each summary records the sample count, minimum, median, p95, median absolute
deviation (MAD), and maximum for cold and warm timings. Every sample still
passes the normal infrastructure and threshold gates; a repeated run never
turns a failed sample into a passing aggregate.

Threshold provenance is reproducible from git history:

```powershell
uv run --no-project python ci/perf_history.py --output ci/perf_threshold_history.json
```

The inventory covers the deleted hosted workflow, `PERF.md`, the local harness,
and the manifest, preserving the commit, scope, and threshold-related diff
lines. Before changing a budget, compare manifests and attach sample IDs,
issue, and rationale:

```powershell
uv run --no-project python ci/perf_history.py --old ci/perf_thresholds.json --new path/to/proposed-thresholds.json --evidence path/to/ratchet-evidence.json
```

Floor decreases and ceiling increases fail without that evidence object;
correctness gates remain independent of timing changes.

The matrix is Linux × two fixtures × four scenarios:

| Fixture | Purpose |
|---|---|
| `medium` | Representative Rust workspace |
| `sqlite-link` | Mixed Rust/C compilation through bundled SQLite; this is compiler coverage, not a mutable-database test |

| Scenario | Contract |
|---|---|
| `cold-tar-untar-warm` | Archived cache restores into a useful warm cache without duplicating the cache tree |
| `worktree-share` | Sibling worktrees reuse artifacts without unbounded cache growth |
| `touch-no-change` | Metadata-only source changes do not destroy reuse |
| `restore-no-clean-warm` | Restoring the cache without cleaning `target/` leaves the next build effectively warm and miss-free |

All eight cells must pass. Results and raw evidence are retained under:

```text
.perf-local/results/<fixture>/<scenario>/
```

Each directory contains `result.json`, cache reports, abort evidence,
shutdown records, logs, and RSS samples. A later fixture or scenario cannot
overwrite earlier evidence.

## Why local Docker is the gate

- The Docker VM and job limit are known. Hosted-runner contention does not
  silently redefine the floor.
- Named Linux volumes preserve compiler fingerprints and make warm rebuilds
  fast even when Docker Desktop runs on Windows.
- soldr and zccache are built from the exact requested source revisions, so an
  unmerged cross-repository fix can be tested before either PR lands.
- Long builds are allowed to finish. Progress diagnostics are emitted after
  60-second intervals, but there is no five-minute wall-clock abort.
- Every sample remains on disk for adversarial inspection instead of expiring
  as an Actions artifact.

The first soldr build may take 5–15 minutes. Persistent Docker volumes make
later source iterations much faster. The default `--jobs 2` fits an 8 GiB
Docker Desktop VM; raise it only when the VM has enough memory.

## Gate semantics

Timing is evaluated only after infrastructure validity passes. Every result
must declare typed values for:

- `infrastructure_valid`
- `invalid_reasons`
- `soldr_abort_count`
- `soldr_timeout_count`
- `soldr_no_cache_retry_count`
- `soldr_abort_evidence`

Every declared evidence file must exist beside `result.json`. Any soldr abort,
timeout, automatic no-cache retry, malformed record, missing field, or missing
evidence file invalidates the sample before timing is considered.

Each attempt also runs the Rust `zccache-ci audit-logs <cache-root> --context
perf` gate before its cache root is removed. The gate is policy-owned by
`zccache-audit`; on failure retain the root and report so the JSONL evidence is
available for inspection rather than silently deleting the repro.

The local Linux thresholds are:

| Gate | Budget |
|---|---:|
| Minimum speedup, every cell | `4.5x` |
| Restore warm time | `10,000ms` |
| Worktree warm time | `15,000ms` |
| Touch warm time | `10,000ms` |
| Cold staged hash + publish + materialize overhead | `15,000ms` |
| Warm bytes copied | `2 GiB` |
| Salvage attempts / critical staged failures | `0` |

`cold-tar-untar-warm` has no absolute warm-time ceiling; its speedup is the
signal. The other ceilings include roughly 2× headroom over the accepted
8 GiB, two-job Docker measurements from #1084.

Every cell must also report at least one successful cold staged publication.
Warm cache scenarios must report a reflink, hardlink-shared, or copy
materialization tier. `restore-no-clean-warm` instead requires exactly zero
warm cache misses. Missing staged telemetry is a hard failure.

## Narrow diagnostic runs

Run one cell while iterating:

```powershell
uv run --no-project ci/perf_local.py
uv run --no-project ci/perf_local.py --scenario worktree-share
uv run --no-project ci/perf_local.py --fixture sqlite-link
uv run --no-project ci/perf_local.py --scenario restore-no-clean-warm --fixture sqlite-link
```

Single-cell runs print a timing/report summary and are diagnostic. Before a
performance PR merges, run `--matrix` so infrastructure and staged telemetry
hard gates are applied to all eight cells.

For a dependent soldr change that has not merged:

```powershell
uv run --no-project ci/perf_local.py --matrix --soldr-ref fix/example-branch
```

The harness checks out that soldr ref, moves soldr's embedded zccache submodule
to the current committed zccache SHA, and builds the exact pair. A dirty
zccache checkout is rejected so measured source cannot differ from the commit
being reported.

Use `--rebuild-images` after changing a Dockerfile. Build/test/lint helpers use
the same warmed Linux volumes:

```powershell
uv run --no-project ci/perf_local.py fmt
uv run --no-project ci/perf_local.py clippy --workspace --all-targets
uv run --no-project ci/perf_local.py test
```

See [`ci/docker/README.md`](ci/docker/README.md) for image and volume layout.

## Standalone compiler campaign

The standalone campaign runs the registered C, C++, Emscripten, and Rust
`perf_bench_test` scenarios in one pinned Linux image. The image uses soldr to
prepare Rust into a cached Docker layer, then seeds soldr/Cargo/rustup state
into a named runtime volume. Source is mounted read-only, and timed samples
invoke the prebuilt benchmark binary directly.

Run one diagnostic sample while iterating:

```powershell
uv run --no-project python -m ci.perf_standalone --language c --test perf_c_zccache_vs_bare --attempts 1 --rebuild-image
```

Run the complete five-sample campaign from a clean committed checkout:

```powershell
uv run --no-project python -m ci.perf_standalone
```

Resume an interrupted campaign without mixing commit, image, fixture, host, or
attempt identities:

```powershell
uv run --no-project python -m ci.perf_standalone --resume
```

Evidence is retained under `.perf-standalone/results/`. Each campaign has a
JSON index and Markdown table linking raw logs, parsed rows, cache phase/byte
telemetry, command provenance, and resource usage for every test. Its prebuilt
benchmark and `zccache-ci` executables live in that campaign's `fixture/`
directory; resume mounts them read-only and verifies both recorded SHA-256
digests before running another sample.

## Soldr-embedded lifecycle campaign

The embedded campaign uses the exact locally built soldr binary containing the
current committed zccache revision. For Rust, C, C++, and Emscripten it records
daemon startup, an already-running cold build, a local cache hit, a sibling
worktree hit, and a target-intact no-op.

The runner derives from the standalone image, whose Docker build executes
`soldr toolchain prepare` with a 600-second command-output window. Runtime
containers seed that prepared soldr home into a named volume and run with the
network disabled, so a sample cannot bootstrap missing dependencies.

Run one correctness sample while iterating:

```powershell
uv run --no-project ci/perf_local.py --embedded-matrix --language rust
```

Run or resume the required five-sample matrix from a clean commit:

```powershell
uv run --no-project ci/perf_local.py --embedded-matrix --repeat 5
uv run --no-project ci/perf_local.py --embedded-matrix --repeat 5 --resume
```

Each sample rejects soldr aborts, timeouts, no-cache retries, daemon fallbacks,
missing cache semantics, wrapper bypass, artifact drift, output replay failure,
or incomplete provenance. Results and per-lifecycle median/MAD/min/max floor
dossiers are retained under `.perf-local/results/embedded-mixed/`.

## Cross-platform responsibility

Linux Docker is the sanctioned timing environment. Native Windows, macOS, and
special filesystems still gate correctness through focused tests:

- NTFS hardlinks must remain shared-inode only for allowlisted immutable
  outputs, with mutation fail-closed behavior.
- ReFS/btrfs/APFS reflinks must prove destination mutation cannot write through
  to the object store.
- Mutable and unknown outputs must use reflink or independent copy.
- FAT/exFAT, cross-volume, unsupported, and failure cases must fall back safely.

Do not infer platform performance from Linux timing, and do not weaken Linux
budgets to accommodate a hosted runner. If dedicated stable hardware is later
added for another platform, establish a separate local baseline and explicit
budgets.

## Regression tests

Scenario timing catches broad failures. A performance bug should also leave a
deterministic test at the narrowest useful layer:

- Use the existing ignored benchmark tests plus `ci/perf_guard.py` for
  compile-time comparisons that are stable enough for a cheap hosted check.
- Use a focused duration/counter assertion for a specific function or path.
- Prefix performance tests with `perf_` and reference the motivating issue.

Do not add a second ad-hoc benchmark framework. Extend this harness or the
existing focused regression tests.

### Choosing a deadline — classify it before you pick a number

Most wall-clock assertions that fail on CI were never measuring what they
claimed to. Before choosing a duration, decide which of three things the
deadline is. The right number differs, and so does the right fix when it goes
red (issue #1452 collected the instances behind this).

**1. The deadline *is* the assertion.** The property is latency, and the bound
is what separates pass from fail. `perf_explicit_argv_dispatches_in_process_under_250ms`
is the example: a process spawn costs ~10-50 ms, so the 250 ms bound is the
whole test. Keep these tight, give them roughly 3× headroom over the post-fix
local measurement, and **never widen one to quiet a flake** — the widened test
passes with the regression present and is then decoration. If such a test is
flaky, it needs a better instrument, not a bigger number.

**2. The deadline is a proxy for a structural property.** The real claim is
"no synchronous load here", "no subprocess spawned", "this ran in-process",
and time is standing in for it. These are the worst offenders, because the
proxy tracks machine speed rather than the property: a fast machine can satisfy
the bound *with* the regression, and a loaded one violates it without.
Assert the property directly where you can — a lifecycle event, a counter, a
source-window scan. Where you cannot, measure a **differential** against a
control so machine speed cancels. `daemon_spawn_lockfile_budget_test` is the
worked example of both halves.

**3. The deadline is only a hang detector.** Once a guard is released or a task
is unblocked, the work either completes promptly or is broken and never
completes. Nothing interesting lives between 5 s and 60 s, so a tight bound
buys no detection and costs CI cycles. Be generous, and name the constant so
the intent is legible — see `HANDOFF_HANG_DETECTOR` in
`handle_exec_tests.rs`.

Two rules that apply to all three:

- **Do not apply a blanket multiplier** when a deadline goes red. It hides
  genuine hangs, and for kind 1 it destroys the test outright.
- **Size against CI, not your machine.** A dev box is routinely several times
  faster than a loaded hosted runner; a unit suite measured at 57 s locally has
  been observed taking 523 s in CI. 3× headroom over a local measurement is a
  floor for kind 1 and not enough on its own for anything else.
