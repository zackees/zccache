# kernal-api Migration Baseline

This is the phase-0 contract for [zccache#1518](https://github.com/zackees/zccache/issues/1518). It is deliberately an inventory and characterization gate: it does not add `kernal-api`, change a dependency, or move native code. The capability implementation program is [kernal-api#5](https://github.com/zackees/kernal-api/issues/5); its package-boundary measurements are [kernal-api#3](https://github.com/zackees/kernal-api/issues/3).

```text
zccache -> kernal-api -> running-process -> Tokio/native OS
```

`running-process` remains the native substrate. `kernal-api` owns facade types and safe semantics; zccache retains product policy and payload protocol `0x7A63`.

## Inventory and omission check

The authoritative inventory is [kernal-api-migration.toml](kernal-api-migration.toml). It lists every public `zccache-platform` operation and direct production backend declaration. Every item has exactly one disposition:

- **reuse** — consume an existing kernel contract;
- **extend** — add a facade-owned semantic contract;
- **move** — move generic native implementation to the kernel; or
- **retain** — leave product policy in its owning zccache crate, with a reason and no native implementation.

Run `uv run --no-project python ci/check_kernal_api_baseline.py` after changing the facade, inventory, or covered manifests. It derives public symbols from the facade and relevant dependency declarations from manifests. It fails if an operation or declaration has no mapping, a mapping goes stale, a disposition is invalid, a retained item lacks ownership/reason, or a characterization path disappears.

The completed zccache-platform migration ([#1365](https://github.com/zackees/zccache/issues/1365) through [#1380](https://github.com/zackees/zccache/issues/1380)) is predecessor evidence, not a second migration to perform. A later slice deletes each replaced native implementation immediately rather than maintaining two paths.

## Characterization contract

The inventory names focused evidence for embedded lifecycle; process lifecycle; IPC and broker semantics; frozen wire bytes; and filesystem safety. A later slice must add a focused RED reproducer before changing an uncovered contract, then turn that signal GREEN. This baseline authorizes no wire change: envelope version/length, protobuf frame bytes, protocol `0x7A63`, endpoint selection, and round-trip count remain frozen.

## Measurement protocol

Record raw phase-0 evidence under `docs/evidence/kernal-api-migration/phase-0/<host>/<timestamp>/`; do not commit machine-specific raw output. Commit a Markdown summary beside the raw location with toolchain, host, revision, commands, feature sets, and result filenames.

For every comparable later slice, collect the same feature sets and command order:

1. Clean and representative incremental build timing reports through the repository-required `soldr` entrypoint.
2. Duplicate dependency-tree output and reverse-feature inspection, identifying owners of async, process, allocator, crash, console, hash, and network facilities.
3. Package-from-archive evidence after a published kernel release is pinned.
4. The [sanctioned PERF matrix](../../PERF.md) only when runtime behavior changes; retain its `.perf-local/results/` evidence rather than treating a wall-clock unit test as a performance baseline.

Phase 0 records the starting graph and timings. It does not claim a build-time improvement; compare that evidence only after the phase-8 release result from kernal-api#5.

## Ownership boundary

zccache keeps cache layout, materialization/mtime and hardlink policy, retry/deployment budgets, compiler target interpretation, endpoint naming policy, service names, payload schemas, and user diagnostics. The kernel may own mechanisms such as process containment, local transport, mapped files, locks, identity, hashing, clocks, cancellation, and native filesystem operations.
