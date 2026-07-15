# Local performance scenarios

These fixtures and shell scenarios are driven by the authoritative Linux
Docker harness in [`../ci/perf_local.py`](../ci/perf_local.py). Run the full
release gate with:

```powershell
uv run --no-project ci/perf_local.py --matrix
```

The harness builds soldr with the current committed zccache revision embedded,
runs each fixture/scenario pair in an isolated container, applies hard timing,
infrastructure, and staged-telemetry gates, and retains evidence under
`.perf-local/results/<fixture>/<scenario>/`.

## Fixtures

- `medium`: representative pure-Rust dependency graph.
- `sqlite-link`: Rust plus bundled `libsqlite3` through `cc-rs`. It validates
  mixed compiler coverage; it is not a mutable SQLite database test.
- `embedded-mixed`: dependency-free Rust, C, C++, and Emscripten source used
  by the explicit soldr lifecycle campaign.

Fixture archives are generated from the sibling source directories by
[`fixtures/regen.sh`](fixtures/regen.sh).

## Scenarios

| Scenario | What turns red |
|---|---|
| `build-then-check` | Cross-verb build/check reuse; diagnostic, not part of the eight-cell rollout gate |
| `cold-tar-untar-warm` | Cache archive fidelity or cache-tree duplication |
| `worktree-share` | Path remapping or sibling-worktree sharing |
| `touch-no-change` | Content-hash robustness after metadata-only changes |
| `restore-no-clean-warm` | Restore/no-op behavior or downstream cache misses |
| `embedded-lifecycle` | Explicit soldr daemon startup, cold, local-hit, sibling-hit, and no-op phases |

Every rollout scenario wraps both measured builds with
`measure::run_guarded_soldr_command`. There is no fixed child wall-clock
deadline. Structured soldr abort/retry evidence invalidates a sample before
timing is evaluated.

## Measurement

[`lib/common.sh`](lib/common.sh) owns shared measurement behavior:

- elapsed wall time;
- cache size and archive size;
- compiler and embedded-daemon RSS sampling;
- cache/session reports;
- staged hash/publication/materialization counters and timings;
- artifact-relative abort evidence;
- snapshot quiescing so SQLite state is not archived with a live owner.

Scenario scripts emit one `result.json`. The local Python evaluator rejects
missing or malformed infrastructure fields, non-positive timing, speedups or
warm times outside the documented budgets, missing staged publications,
salvage/critical failures, excessive copied bytes, missing materialization
tiers, and restore warm misses.

## Layout

```text
perf/
├── fixtures/
│   ├── medium/
│   ├── medium.tar.gz
│   ├── sqlite-link/
│   ├── sqlite-link.tar.gz
│   └── regen.sh
├── lib/
│   ├── common.sh
│   └── extract.sh
└── scenarios/
    ├── build-then-check/run.sh
    ├── cold-tar-untar-warm/run.sh
    ├── worktree-share/run.sh
    ├── touch-no-change/run.sh
    └── restore-no-clean-warm/run.sh
```

## Adding a fixture or scenario

1. Keep fixture source deterministic and network-independent after extraction.
2. Reuse the helpers in `lib/common.sh`; do not add a second timing or abort
   protocol.
3. Emit typed infrastructure fields and preserve evidence beside `result.json`.
4. Add pure harness tests under `ci/tests/` for parsing and failure behavior.
5. Add the cell to `ci/perf_local.py` only after a local baseline justifies its
   budgets.
6. Update [`../PERF.md`](../PERF.md) with the contract and threshold rationale.

Do not move the wall-clock matrix into GitHub Actions. Hosted jobs remain for
deterministic regression and platform-correctness tests.
