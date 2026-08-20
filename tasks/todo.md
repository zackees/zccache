# #1365 centralize host-platform mechanics behind zccache-platform

Parent: https://github.com/zackees/zccache/issues/1365 — 6 phase sub-issues (#1366-#1369 exist; two more to be created for phases 5/6). Parent auto-closes when all sub-issues close. Starting branch: `refactor/platform/phase2-fs`.

- [x] Read #1365 + sub-issues #1366-#1369; baseline branch recorded.
- [x] Install Rust 1.95.0 toolchain locally.
- [ ] Phase 0: exploration inventories (MSRV pins, platform-heavy code, amalgamation/dylint/CI).
- [x] Phase 1 (#1366): PR #1370 merged.
- [x] Phase 2 (#1367): PR #1371 merged; corrective PR #1373 merged green.
- [~] Phases 3/4/5 (#1368/#1369/#1378): integrated on PR #1375; checkpoint PRs #1377 and #1379 merged into its branch.
- [~] Phase 6 (#1380): zero-baseline implementation complete locally; validation and PR gates pending.
- [ ] Finish: git status clean; local repo back on `refactor/platform/phase2-fs` rebased to final main.

Each phase: branch from main → TDD RED→GREEN → soldr fmt/clippy/check → ./test → PR → wait for GHA green (watch) → merge → rebase next phase.

## Phase 2 (#1367) plan — platform::fs

Slices per sub-issue: (1) RED facade/characterization tests, (2) identity/link/change-marker
from persist/hardlink.rs + fs_caps.rs, (3) permissions from config/paths.rs + win_acl.rs +
staged_lock.rs + rust_plan/local.rs, (4) replace/clone/positioned-io from kv.rs +
persist/{artifact_io,staged_paths,staged_store}.rs, (5) path normalization from
zccache-core/src/path.rs, (6) wire consumers (core, artifact, daemon-core) + ratchet baseline.

Characterization tests FIRST: two hardlinks == FileIdentity; copy !=; nlink increments;
volume identity stable; Windows 128-bit FileId preserved; unix ChangeMarker = None.
Baseline rows to delete: all rows for core/artifact/daemon-core fs files (per-crate counts in
commit bde74dfb). Files must stay <1000 LOC (kv.rs 1061 → split).

## Phase 3 (#1368) plan — platform::ipc

RED (2026-08-13): `soldr --no-cache cargo test -p zccache-platform` failed with eight
`cannot find ipc in platform_imp` errors after the neutral contract and characterization tests
were added before any concrete backend. The same command is GREEN with 21 passing tests after
adding the Linux, macOS, and Windows concrete transports. Final focused GREEN:
26 platform tests, 52 IPC tests, and the download-protocol endpoint test pass on Windows;
platform + IPC all-target checks pass for installed macOS and Linux cross targets. Coverage
includes simultaneous 1 MiB duplex traffic, endpoint retirement/rebind, ordinary-file
preservation on Unix, Unix 0700/0600 permissions, and the live Windows pipe DACL.

Facade: endpoint/stream/listener/connect/peer. Move zccache-ipc/src/transport/{mod,unix,windows,
pipe_security}.rs native mechanics; keep framing/protocol/broker/timeouts in zccache-ipc.
Endpoint strings byte-for-byte stable; owner-only socket/pipe security preserved; no extra
roundtrip; download_client + daemon_mgmt consume neutral APIs. Baseline rows: all IPC +
download-protocol rows deleted (390 -> 336). Workspace check and clippy with `-D warnings`
pass; `./test` passes. `./test --integration` reaches an unrelated existing contract mismatch:
`stop_kills_locked_process_when_ipc_is_unreachable` expects a fabricated PowerShell PID to be
killed, while the #1161/#132 identity-bound stop intentionally refuses non-zccache processes.

## Phase 4 (#1369) plan — platform::process

RED (2026-08-13): `soldr --no-cache cargo test -p zccache-platform` fails with
13 missing process-facade APIs after adding the capability leaves and owned-child
characterization tests before any concrete backend.

Facade: command/spawn/priority/inspect/terminate/stdio/jobserver/exit. Move daemon/{process,
child_watchdog,jobserver,trampoline}.rs native code + core/crash.rs signal labels +
ipc PID helpers + cli-core deploy.rs spawn bits + bin/zccache.rs stack wrapper. Keep
CompilePriority/watchdog budgets/jobserver accounting in daemon. Coordinate #1360 (owner-death
behavior preserved). Files <1000 LOC (process.rs 1365, child_watchdog.rs 1202 → split).

GREEN checkpoint (2026-08-13): inspect/terminate callers, watchdog CPU accounting,
priority application, Windows kill-on-close Job Object ownership, and the POSIX jobserver
primitive now use the neutral process facade. Product policy and diagnostics remain in their
callers. Platform tests pass 32/32, focused daemon jobserver tests pass 2/2, and targeted
production clippy passes with warnings denied. The exact-occurrence baseline fell from 336 to
127 after also moving stdio redirection, crash-context interpretation, command window setup,
CLI stack selection, and pre-spawn owner-death selection; its cache version is 0.1.7. Docker
Desktop's Linux engine pipe was unavailable, so the
Linux Dylint runtime gate remains required before publication; local baseline wiring validation
passes.

The two touched oversized production files were split without moving product policy:
`process.rs` is 721 lines and `child_watchdog.rs` is 696 lines; their existing test modules
now live in purpose-named sibling `_tests.rs` files.

## Phase 5 (#1378) plan — platform::executable + platform::host

executable: deploy.rs suffixes/PATH/PATHEXT/image lookup/unlock_exe. host: native_cpu.rs,
defender.rs elevation + Defender primitives, path/env OS facts, home/runtime dirs, resource probes.

RED (2026-08-13): focused platform compilation failed with nine missing concrete
`executable`/`host` backend references after adding neutral characterization tests first.
GREEN checkpoint: all three private host trees now implement executable naming/search/stem,
shared-library candidates, running-image replacement, and host OS/arch/home/runtime/user/
elevation/resource facts. Windows Defender command mechanics moved into the concrete Windows
host tree while core retains product errors and UX.
Platform/core/depgraph tests and CLI-core all-feature/all-target checks pass on Windows. The
exact-occurrence baseline fell from 127 to 97 after migrating Defender, native CPU inputs,
libclang discovery, and executable replacement.

## Phase 6 (#1380) plan — zero baseline

RED: the final audit found 97 grandfathered production occurrences across artifact, compiler,
core, daemon, depgraph, and IPC.

GREEN implementation: all 97 rows were migrated, `baseline.txt` and its runtime ratchet were
deleted, and the lint now rejects every production occurrence directly. Host cfg/native paths
scan clean outside the selector/concrete trees. Production `libc`/`windows-sys` dependencies are
confined to zccache-platform; the only other libc declaration is Unix integration-test-only.

Pending: Linux Dylint, cross-target/target-semantics checks, publish self-containment, full tests,
clippy, pre-push review, and green PR CI before merge and parent closure.

# soldr#2188 short compiler staging root

- [x] Read the issue, staged-output architecture, portability contract, and performance gate.
- [x] RED: prove an embedded host can place private compiler staging outside a deep cache root.
- [x] GREEN: add an explicit embedded staging root while preserving cache-local staging by default.
- [x] Document the ownership and cleanup contract.
- [ ] Run focused tests, formatting, clippy, and the sanctioned performance matrix.
- [ ] Review, push, merge, and validate the upstream change before updating Soldr.

# #1215 staged materialization lock contention

Issue: https://github.com/zackees/zccache/issues/1215 — `perf(persist): bound staged materialization lock contention before GC hardening`

- [x] Read the issue, PERF.md, prior #1215 PRs, staged-store ownership code, and retained evidence.
- [x] RED: add a deterministic regression/performance test proving concurrent staged deliveries share one process-local OS read lease while preserving maintenance exclusion.
- [x] GREEN: retain one shared staged-store file lock per active daemon/root, releasing it only after the final materialization lease.
- [ ] Verify the complete staged delivery matrix, formatter, clippy/check, integration tests, and the sanctioned repeat-5 Docker matrix (or document a concrete infrastructure blocker).
- [ ] Self-review the final diff for correctness, lock lifetime, cross-process GC behavior, and scope.
- [ ] Commit, push, open and merge the #1215 PR; synchronize local `main`, delete this branch, and verify a clean checkout.

## Review

- Focused RED signal: `concurrent_staged_materialization_leases_share_the_store_lock` failed with eight OS shared-lock acquisitions before the state-aware lease implementation, then passed after it.
- Production behavior: all five daemon hit delivery entry points now acquire a `SharedState`-owned staged lease; direct test helpers retain an independent acquisition path only under `cfg(test)`.
- Pending full validation and review.

# soldr#2031 running-process boundary

- [x] Inventory raw daemon process creation and existing Dylint exemptions.
- [x] Route sync and Tokio daemon children through running-process.
- [x] Replace file allowlists with production/test scope detection and UI coverage.
- [x] Make the workspace Dylint job a pull-request gate.
- [x] Pin the running-process revision that provides the required APIs.
- [x] Run focused Dylint, daemon checks, fmt, and clippy.
- [ ] Open, drive, merge, and validate the coordinated upstream PR.

# soldr#1783 safe nested Dylint caching

- [x] RED: model only the explicit nested Dylint compiler shape as Rust.
- [x] Hash driver, inner compiler, loaded lint-library contents, and output-affecting environment.
- [x] Fail open with a diagnostic for missing, malformed, or unhashable lint-library state.
- [x] Cover warm hits, invalidations, diagnostics, output/exit status, and ordinary Rust regressions.
- [x] Remove process-global dated-nightly overrides and guard against their return.
- [ ] Exercise the supported Soldr front door in upstream CI.
- [ ] After Soldr lands its shim, follow up upstream to run Dylint CI and the real benchmark through `soldr cargo dylint`.
- [x] Document Cargo incremental reuse versus cross-target/worktree cache reuse.
- [ ] Run focused tests, Linux Docker integration, lint/docs, and sanctioned performance evidence.
- [ ] Open, drive, merge, and validate the upstream PR before updating Soldr.

# Soldr embedded durability Lavra follow-up

- [x] Split oversized embedded/staged-store source files without changing public API.
- [x] Correct embedded cancellation and report-shape documentation.
- [x] Bound the index-writer lost-wakeup regression test.
- [x] Repair Dylint environment tests and cover alias/retry recovery.
- [x] Complete staged persistence module and marker documentation.
- [x] Track depfile role independently from suffix, rewrite bytes atomically, and propagate failures.
- [x] Cover arbitrary-extension and non-UTF-8 depfiles plus rewrite failures.
- [x] Run focused/full Rust, Python, docs, lint, and cross-target verification.

# #1149 secret-safe compile journal environment

Issue: https://github.com/zackees/zccache/issues/1149

- [x] Define one deny-first diagnostic allowlist for embedded and IPC journals.
- [x] Enforce filtering in context capture, entry construction, and serialization.
- [x] Add a versioned cross-repo fixture and representative secret/value tests.
- [x] Document the security and replay compatibility contract.
- [x] Run focused/full tests, fmt, clippy, rustdoc, and review.
- [ ] Open the PR, pass CI, merge, and validate main.

# #1148 bounded embedded/standalone disk retention

Issue: https://github.com/zackees/zccache/issues/1148

- [x] Confirm standalone/embedded lifecycle parity gap and exact-root ownership.
- [x] Add shared policy/scanner seams with RED tests for budgets, ages, pressure, hardlinks, and sibling sentinels.
- [x] Wire the shared pass into standalone and embedded startup/periodic lifecycles.
- [x] Expose host-callable embedded maintenance and structured reports.
- [x] Persist/catch up the daily age-maintenance deadline inside the configured root.
- [x] Update architecture/runtime documentation.
- [x] Run focused/full tests, fmt, clippy, Windows compile, and review.
- [x] Open the PR, pass CI, merge, and validate main.

# #1063 thin-v3 ownership-aware durable export

Parent: zackees/soldr#1609 · Policy: `thin-v3-lifetime-partition-v1`

- [ ] Add cache-schema v3 ownership policy/mode and per-artifact owner metadata.
- [ ] Make identity/cache keys and bundle compatibility reject cross-mode payloads.
- [ ] Filter cook-owned outputs from `cook-partitioned-v1` durable exports; fail closed on unknown ownership.
- [ ] Expose verified owner-qualified materialization and machine-readable diagnostics/metrics.
- [ ] Add adversarial save/restore coverage, then validate the public CLI and embedded soldr consumer.

# #968 wedge/timeout burn-down — near complete

Meta: https://github.com/zackees/zccache/issues/968

# #1136 PathBuf Dylint backlog

Issue: https://github.com/zackees/zccache/issues/1136

- [x] Classify and migrate persistent filesystem identities to `NormalizedPath`.
- [x] Preserve bundle-relative paths without treating them as filesystem identities.
- [x] Add regression coverage for normalized identity and relative bundle behavior.
- [x] Run targeted daemon/test-support checks.
- [ ] Validate the Linux Dylint CI job after merge.
- [ ] Ship, merge, and validate the resolving PR; close #1136.

## Merged to main ✅
- #967 (PR #969) client-disconnect cancellation
- #962 (PR #970) orphan-pipe post-exit watchdog (Mode A)
- #971 (PR #976) in_flight_exec lost-wakeup + bounded waiter + exec-spawn watchdog
- #972 (PR #978) `-vV` identity probe timeout
- #973 (PR #977) embedded flush disk-save bounds
- #891 (PR #980) CPU/output progress watchdog (Mode B) — all-platform CPU sampling
- #890 (PR #981) async/process bridge design doc (runtime.md)

## In CI (merge when green) 🔁
- #974 (PR #979) watcher consumer wakes on shutdown
- #893 (PR #983) child pid in watchdog diagnostics
- #892 (PR #984) pipe-saturation / concurrent-drain regression test

## Remaining 📋
- #894 concurrency-not-reduced test (child_watchdog tests) — DO AFTER #892 merges
  (same tests-module region → conflict otherwise). N concurrent watchdog waits on
  ~1s sleepers → assert total < serial (not serialized).
- Then close #889 (all children #890/#891/#892/#893/#894 done) + close #968.

## Extra issues filed (user requests)
- #975 internal multi-crate split for parallel compiles (single published crate)
- soldr#1465 soldr build wedge (embedded zccache cache) — silent death; ZCCACHE_DISABLE=1 recovery

## Key techniques learned (memory)
- ZCCACHE_DISABLE=1 to bypass wedge-prone build cache locally
- soldr cargo check --target <triple> to validate cfg(linux)/cfg(macos) arms locally
- Always `cargo fmt --all` (auto-fix) + real exit check (no pipe mask) before commit
- Kill orphan cargo.exe holding target/debug/.cargo-lock if builds "block"

## Design rules honored
- Progress/CPU-based watchdogs, never dumb wall-clock (links run minutes)
- Every timeout/watchdog fire: loud warn! + durable lifecycle event (forensics)
- Constants at top of file

# #975 internal crate split

Source: https://github.com/zackees/zccache/issues/975#issuecomment-4920394110

Contract:
- Split the current `crates/zccache` monocrate into internal workspace crates so git-rev/vendored source consumers compile subsystems in parallel.
- Keep `zccache` as the public facade preserving existing public module paths, feature names, and bin targets.
- Keep crates.io publication as one public crate named `zccache`; later wave must add publish-time amalgamation plus a CI guard that no internal crate is accidentally published.

Current wave:
- Wave 1 foundation carve: `core`, `hash`, `audit`, and `gha`.
- Subagents may edit disjoint module/crate files only and must not run linting, building, testing, formatting, or any executable command.
- Main agent owns shared workspace manifests, facade wiring, and all verification/fixups with bounded `soldr` commands.

Wave 1 status:
- Added unpublished internal crates `zccache-core`, `zccache-hash`, `zccache-audit`, and `zccache-gha`.
- `zccache` facade now re-exports those crates as `zccache::core`, `zccache::hash`, `zccache::audit`, and feature-gated `zccache::gha`.
- Verified on 2026-07-09: focused `cargo check`, facade all-target/all-feature check, workspace all-target/all-feature check, `cargo fmt --all` via `soldr --no-cache`, workspace clippy all-target/all-feature, focused new-crate tests, and `./test`.

Next wave:
- Carve `symbols`, `download`, `fscache`, `compiler`, `artifact`, and `compile_trace` into new internal crates.
- Repurpose the existing `crates/zccache-fingerprint` Python-extension crate into the internal fingerprint engine crate with Python bindings gated behind its existing `python` feature.

Wave 2 status:
- Added unpublished internal crates `zccache-artifact`, `zccache-compile-trace`, `zccache-compiler`, `zccache-download`, `zccache-fscache`, and `zccache-symbols`.
- Repurposed existing `zccache-fingerprint` into the internal fingerprint engine crate while preserving the `python` extension feature.
- `zccache` facade re-exports these crates on the old public paths and forwards `download`, `symbols`, `cli`, and `gha` features as needed.
- Verified on 2026-07-09: Wave 2 crate check, facade all-target/all-feature check, workspace all-target/all-feature check, `cargo fmt --all` via `soldr --no-cache`, workspace clippy all-target/all-feature, focused internal-crate tests, and `./test`.

Next wave:
- Carve `depgraph` and `download_protocol` into internal crates.
- Repurpose existing `crates/zccache-watcher` Python-extension crate into the internal watcher engine crate with Python bindings gated behind its existing `python` feature.

Wave 3 status:
- Added unpublished internal crates `zccache-depgraph` and `zccache-download-protocol`.
- Repurposed existing `zccache-watcher` into the internal watcher engine crate while preserving the `python` extension feature.
- `zccache` facade re-exports `depgraph`, feature-gated `download_protocol`, and `watcher` on the old public paths.
- Verified on 2026-07-09: Wave 3 crate check, facade all-target/all-feature check, workspace all-target/all-feature check, `cargo fmt --all` via `soldr --no-cache`, workspace clippy all-target/all-feature, focused Wave 3 tests, and `./test`.

Next wave:
- Carve `protocol` into `zccache-protocol`.
- Then carve `ipc` into `zccache-ipc` once `protocol` is available.

Wave 4 status:
- Added unpublished internal crate `zccache-protocol`.
- Moved the daemon protocol module and protobuf build into `zccache-protocol`.
- `zccache` facade re-exports `zccache_protocol` on the old `zccache::protocol` path.
- Verified on 2026-07-09: protocol crate all-target/all-feature check, facade all-target/all-feature check, workspace all-target/all-feature check, `cargo fmt --all` via `soldr --no-cache`, and focused protocol tests.

Next wave:
- Carve `ipc` into `zccache-ipc` now that `zccache-protocol` owns protocol types.

Wave 5 status:
- Added unpublished internal crate `zccache-ipc`.
- Moved the IPC transport, broker, manifest, and process helpers into `zccache-ipc`.
- `zccache` facade re-exports `zccache_ipc` on the old `zccache::ipc` path.
- Verified on 2026-07-09: IPC crate all-target/all-feature check, facade all-target/all-feature check, workspace all-target/all-feature check, `cargo fmt --all` via `soldr --no-cache`, workspace clippy all-target/all-feature, focused IPC tests, and `./test`.

Next wave:
- Add publish-time single-crate amalgamation and CI guards so only the public `zccache` crate can be published.
- Do release/soldr validation and hardening after the split PR lands.

Wave 6 status:
- Added release-time `zccache` crate amalgamation for crates.io packaging while keeping the checked-in workspace split for git/path consumers.
- Marked internal and PyPI extension crates `publish = false`; crates.io publish order is now only the public `zccache` crate.
- Release publish now performs a real `cargo package` verification on the transformed crate before upload.
- Verified on 2026-07-09: focused release Python tests, full `ci/tests`, `bash ./test`, workspace clippy all-target/all-feature, and transformed `zccache` package verification with `RUSTFLAGS=-D warnings`.
# #1131 remaining cache-isolation bugs

- [x] #912: merge and validate output-directory side-effect isolation (#1132).
- [x] #1028: keep distinct mutable dependency-graph variants per worktree
  while retaining root-normalized artifact identity.
- [x] Add A/B/A, concurrent registration, bounded-variant, and snapshot
  round-trip coverage; validate the Windows worktree integration without
  `RUST_MIN_STACK`.

# #1039 capability-driven COW materialization

Issue: https://github.com/zackees/zccache/issues/1039

- [ ] Capture RED characterization for poisoning, capability selection, registry, reflink independence, readonly cleanup, and matrix reporting.
- [ ] Add per-volume-pair capability probing with cached verdicts and kill switches.
- [ ] Add 128-bit-safe file identity and hardlink materialization registry/ceiling fallback.
- [ ] Add reflink-first materialization with mtime preservation and hardlink/copy fallback.
- [ ] Enforce readonly cache blobs, mediated detach, and verify/heal behavior.
- [ ] Add parameterized filesystem fixtures with loud matrix summaries.
- [ ] Add perf regression gate and user/architecture/feature-matrix docs.
- [ ] Validate on Windows 10 and Linux Docker, then run repo lint/test/review gates.
- [ ] Push one PR with RED evidence, wait for GHA/review, fix, squash merge, verify issue closure.

# #1056 immutable staged-output burn-down

Parent: https://github.com/zackees/zccache/issues/1056
Current child: https://github.com/zackees/zccache/issues/1071

- [ ] Replace lossy staged planner `Option` results with enabled/unsupported/error outcomes.
- [ ] Attribute every compile/archive/link/exact-exec rejection to one bounded stable reason.
- [ ] Add deterministic publication, salvage, and materialization fault hooks and adversarial tests.
- [ ] Validate #1071 on Windows and Linux Docker, merge its PR(s), then close the child.
- [ ] Re-audit every remaining parent producer, filesystem, mutable-output, and perf exit gate.
- [ ] Implement and merge all remaining parent slices before closing #1056.

# Fix-forward after #1049 and soldr embedded-zccache integration

- [x] Capture RED for the obsolete `soldr update-zccache` perf bootstrap.
- [x] Build soldr with the current zccache commit checked out in its vendored submodule.
- [x] Key the staged soldr binary by both the soldr and zccache commits.
- [x] Remove the obsolete runtime pin step.
- [x] Format the #1049 Rust changes and update the performance documentation.
- [x] Run focused tests, formatting, clippy, and review gates.
- [ ] Merge upstream, bump soldr's submodule, validate, and merge downstream.
# #1117 soldr-embedded mixed-language baseline

Issue: https://github.com/zackees/zccache/issues/1117

- [x] Add RED contracts for four languages, five lifecycle phases, provenance,
  strict infrastructure validity, wrapped native compilers, and artifact fidelity.
- [x] Add a compiler-complete embedded runner derived from the pinned #1116 image.
- [x] Add a deterministic Cargo fixture covering Rust, C, C++, and Emscripten outputs.
- [x] Add `perf_local.py --embedded-matrix` with repeat/resume campaign evidence.
- [x] Retain wall/CPU/TTFB/output, RSS, cache/artifact, phase, command, and identity data.
- [x] Run focused tests, Linux Docker diagnostics, and five valid samples per cell.
- [ ] Publish floor dossiers, merge the PR, and close #1117.

# DashMap read-snapshot investigation

- [x] Classify DashMaps by lookup/write semantics and select viable candidates.
- [x] Inspect existing performance evidence for lock or lookup overhead.
- [x] Bound the possible win from retained warm-path phase profiles.
- [x] Evaluate merge cost, stale-read correctness, memory, and operational complexity.
- [x] Report whether the design is worth implementing and where.
- [x] Shorten artifact lookup guards before considering a two-map redesign.
- [x] Share lazy payload and access state across owned cache-entry clones.

# Source-cache session immutability and GC investigation

- [x] Define source immutability boundaries for one compile and a build session.
- [x] Trace watcher, lookup, hashing, and metadata-cache mutations.
- [x] Audit periodic memory GC and explicit clear during active compiles.
- [x] Check regression tests and history for concurrent-change contracts.
- [x] Report whether an immutable per-session snapshot is safe.

# Traffic-aware source metadata GC investigation

- [x] Find existing compile/activity and memory-pressure signals.
- [x] Determine how to split candidate discovery from conditional deletion.
- [x] Design idle deferral with bounded starvation and memory-pressure escalation.
- [x] Identify candidate freshness, hot-entry, and journal cleanup requirements.
- [x] Report the recommended implementation shape.

# Traffic-aware source metadata GC implementation

- [x] Track complete compile-cache requests with an RAII activity guard.
- [x] Split metadata eviction into read-only candidate collection and conditional deletion.
- [x] Preserve entries whose verification timestamp changes between the two passes.
- [x] Defer blocking deletion for an idle grace period, then use nonblocking batches.
- [x] Force a blocking pass after bounded persistent traffic.
- [x] Distinguish lock-busy candidates from timestamp-refreshed candidates so
  forced completion retries only the former.
- [x] Settle deferred journal cleanup when traffic becomes idle or escalation fires.
- [x] Keep disk-maintenance scans read-only and take the publication writer
  only for the revalidated destructive commit.
- [x] Count compile, link, generic-exec, and exec-probe metadata consumers.
- [x] Cover blocking and nonblocking timestamp races with focused tests.
- [x] Cover gentle-to-forced handoff, held-shard retry, and maintenance/Clear
  artifact-lease ordering with deterministic tests.

# Dylint sibling env-dep identity follow-up

- [x] Reproduce the real-driver sibling miss by making rustc record
  `DYLINT_LIBS` as an env dependency while library paths differ by worktree.
- [x] Fold path-valued `DYLINT_LIBS` env deps through the existing synthetic
  Dylint content identity for lookup, freshness, and publication.
- [x] Validate the focused integration, depgraph suite, formatting, clippy,
  and review gate in Linux Docker.
- [ ] Merge the upstream PR, bump soldr's vendored commit, rerun the hosted
  real Dylint/watchdog acceptance, and close the meta issue.

# #1400 stable CLI-owned cache paths

- [x] Reproduce the current-main cargo-registry path mismatch from Actions on all three OSes.
- [x] File #1400 and add it to the #1205 burn-down tracker.
- [x] Add RED tests proving daemon namespaces do not move CLI-owned shared cache paths.
- [x] Keep daemon-owned state namespaced while restoring cargo-registry and symbols to the stable cache root.
- [x] Run focused tests, formatter, checks, clippy, and the locally available workflow contract.
- [x] Review, commit, push, merge the PR, close #1400, and verify current main.

## Review

- Current-main evidence: `cargo-registry save` writes below
  `daemon-state/<dev-binary-hash>/cargo-registry`, while `cache-root` reports
  the stable effective root expected by the public CLI and action contract.
- RED: the real CLI test failed with the archive under
  `daemon-state/dev-binary-hash/cargo-registry`; the production path helpers
  now keep both Cargo-registry and symbol payloads under the stable root.
- GREEN: the focused core path tests, the real cargo-registry CLI regression,
  the symbols caller contract, `zccache-core`'s full suite, the full
  `cli_cache_root` integration test, formatting, `zccache-core` clippy, and
  the feature-complete `zccache` check pass. The local workflow harness reaches
  its Docker preflight, but Docker Desktop is unavailable; the hosted
  three-OS action job remains the authoritative workflow validation.
- Review follow-up: an isolated symbols caller subprocess sets a non-empty
  daemon namespace and requires the shared cache directory to be exactly
  `<default_cache_dir>/symbols`, rejecting a stale
  `daemon-state/<namespace>` intermediate path.
- PR #1401 merged as `d11f4bde`; all three hosted cargo-registry jobs passed
  and #1400 closed automatically.

# #1404-#1409 / #1412 current-main CI stability

- [x] Reproduce and fix the namespace-mismatched daemon lockfile budget test (#1404).
- [x] Characterize the detached exec/Clear handoff timeout and keep its lock-order proof deterministic (#1405).
- [x] Make the warm-hit unit performance sentinel robust to hosted Windows runner noise (#1406).
- [x] Run build-harness journal cleanup after failed tests so the strict audit owns only runtime logs (#1407).
- [x] Allow explicit scheduler tolerance around pending-write timeout wakeups (#1408).
- [x] Remove obsolete legacy-toolchain allowlist entries while keeping the checker exact (#1409).
- [x] Make wrapper failure-boundary lifecycle fixtures namespace-aware (#1412).
- [x] Add RED contract coverage for each deterministic regression.
- [x] Run focused tests, formatting, checks, clippy, and the repository review gate.
- [x] Push, merge, close #1404-#1409 and #1412, and verify current-main CI.

## Review

- #1404 hosted RED: the development daemon wrote its namespaced lockfile within
  the nine-second cap, while the parent test polled the legacy unnamespaced path.
- #1405 hosted RED: the queued Clear future timed out after the publisher
  completed its single-guard handoff under the Linux x86 daemon-core suite.
- #1406 hosted RED: one Windows x86 sample took 2.1811039s against a 2s absolute
  budget; the dedicated COW performance guard passed in the same run.
- #1407 hosted RED: a preceding workspace failure skipped the success-only
  cleanup, then the unconditional audit reported 188 embedded v1.12.15
  build-harness journal records as integration-runtime violations.
- #1408 local full-suite RED: a 5ms pending-write timeout resumed at
  100.2205ms and failed an upper assertion of 100ms by 220.5us.
- #1409 local RED: the exact legacy-toolchain checker listed three files that
  no longer contain the legacy marker and failed on its stale allowlist.
- #1412 hosted RED: the wrapper contract tests read the legacy lifecycle log
  while development children wrote namespaced events; the refusal exit code
  was correct, but the test saw no refusal or spawn events.
- GREEN: all focused tests pass; the full daemon-core suite passes with 754
  active tests and 25 ignored; both lockfile-budget tests pass; the workflow
  and toolchain Python contracts pass; all three ignored wrapper-boundary
  tests pass; feature-complete check, clippy with warnings denied, formatting,
  and diff whitespace checks pass.
- Hosted GREEN: PR #1410 passed the full matrix, including MSRV, Dylint,
  wrapper failure boundaries, full workspace integration, strict artifact
  audit, and all platform jobs; it merged as `502827e3` and closed all seven
  linked issues.

# #1411 resumable standalone performance fixture

Issue: https://github.com/zackees/zccache/issues/1411

- [x] Add RED coverage proving resume never rebuilds or replaces the recorded fixture.
- [x] Keep every retained executable campaign-local and verify it read-only.
- [x] Preserve the normal build-and-record path for fresh campaigns.
- [x] Run focused tests, lint/format checks, and the review gate.
- [ ] Push, merge, close #1411, and resume the #1116 campaign.

## Review

- RED: resume called `_build_benchmark` before comparing the recorded fixture
  digest, replacing the shared artifact before it could detect drift.
- GREEN: 35 focused standalone/embedded tests pass; shell syntax, Python
  syntax, E/F lint, and diff checks are clean.
- Docker: a fresh interrupted campaign built and verified both local fixture
  executables; `--resume` ran verify without a build and preserved both hashes
  exactly before correctly refusing timing on unrelated active host compilers.
- Review follow-ups: retain both runtime executables per campaign, hash both,
  reject missing or changed artifacts before sampling, and mount both the
  fixture and its parent results alias read-only during verification.
- clud-review: clean (one reviewer).

# #1414 isolate session-reaping lifecycle events

- [x] Add a RED regression proving a state-owned reap event follows the daemon's explicit cache root, not the process-global environment.
- [x] Route the event through the explicit-root lifecycle writer and preserve its payload/cardinality contract.
- [x] Run the focused regression repeatedly, the daemon-core suite, formatting, and clippy/checks.
- [x] Run the repository review gate.
- [ ] Push, merge, close #1414, and verify the Linux x86 gate.

## Review

- RED: with the old process-global writer, the isolated daemon log retained
  zero rows after a one-session reap (`left: 0`, `right: 1`).
- GREEN: the focused test passes against the daemon-owned root; the full
  daemon-core suite passes (754 active, 25 ignored), as do formatting,
  warnings-denied clippy for every daemon-core target, and whitespace checks.
- Post-merge GREEN: the focused regression passes against current main.
- clud-review: clean (one reviewer).

# #1418 cache packed Linux debug sidecars

Issue: https://github.com/zackees/zccache/issues/1418

- [x] Add RED coverage for staged expected outputs and legacy collected files.
- [x] Model `<primary>.dwp` for explicit Linux targets without affecting other targets.
- [ ] Prove miss/hit persistence and hosted cache-first release behavior.
- [x] Run focused tests, formatting, Clippy/checks, and the review gate.
- [ ] Push, merge, close #1418, and remove the #864 repair once hosted evidence is green.

## Review

- Hosted RED: release dry run 32355952002 restored both shipped Linux binaries
  without their packed debug sidecars and entered the wrapper-free repair.
- Local GREEN: three target/output-set regressions pass; the Linux-only real
  daemon miss/delete/hit test builds on Windows and will execute on hosted
  Linux. All-target Clippy passes with warnings denied.
- Review follow-up: packed sidecars now require effective link emission and
  enabled debug info, and repeated `split-debuginfo` values use command order.
- clud-review: clean after two finding/fix passes (one reviewer).

# #1419 preserve ordered codegen option semantics

Issue: https://github.com/zackees/zccache/issues/1419

- [x] Add RED parser and context-key coverage for reversed repeated options.
- [x] Preserve ordered codegen/linker arguments and normalize `-g`/`-O` aliases.
- [x] Bump the Rust context-key domain to prevent reuse of pre-fix artifacts.
- [x] Run focused tests, formatting, Clippy/checks, and the review gate.
- [ ] Push, merge, and close #1419.

## Review

- RED: reversed last-one-wins values sorted to the same parsed representation;
  the Dylint lane repeated that sort, and linker arguments also lost order.
- RED: `-g` and `-O` stayed in separately sorted unknown flags, so their order
  relative to explicit `-C` values was invisible to both keys and DWP modeling.
- GREEN: ordered parser, effective-value, alias-precedence, linker-order,
  Dylint-key, and context-key regressions pass; the key domain is now v3.
- GREEN: all 448 depgraph tests, six focused DWP tests, the ignored daemon
  integration build, formatting, diff checks, and warnings-denied Clippy pass.
- clud-review: clean after ordered-key follow-up fixes (one reviewer).

# #1361 separate Dylint artifact and verdict identity

Issue: https://github.com/zackees/zccache/issues/1361

- [x] Add RED coverage proving plain artifacts cannot satisfy a Dylint verdict.
- [x] Remove the Dylint input hash from artifact and metadata-compat identity.
- [x] Add a Dylint-keyed verdict layer that gates diagnostics and exit status.
- [ ] Prove build/Dylint artifact sharing without skipped lint execution.
- [x] Run focused tests, formatting, Clippy/checks, and the review gate.
- [ ] Push, merge, and close #1361.

## Review

- RED: the Dylint input hash changed the rustc artifact context key, preventing
  plain and linted invocations from sharing byte-identical outputs.
- GREEN: rustc artifact identity excludes the Dylint hash; an embedded verdict
  map keys stdout, stderr, and exit status by plain/Dylint identity and gates
  materialization before any output is replayed.
- Legacy index snapshots migrate with an empty verdict map, while malformed
  current snapshots cannot fall back to the positional legacy decoder.
- Missing verdicts are soft misses that preserve the shared artifact and its
  depgraph entry; error verdict publication preserves existing output metadata
  and follows the same ordered index-writer WAL as success publication.
- Review follow-up: cached error verdicts now return before output payload
  materialization, rehydrate staged-path markers in their diagnostic streams,
  and replay byte-exact status/diagnostics without writing a sibling success
  artifact to the destination.
- Review follow-up: access checkpoints use timestamp-only WAL touches that
  merge into the newest row, and cold commits merge durable output metadata
  plus sibling verdicts before publication.
- Repo hygiene: request validation, Dylint/time-macro setup, include discovery,
  and invocation parsing moved into `pipeline/request_prep.rs`; both pipeline
  modules are below the 1,000-line source ceiling.
- GREEN: 104 active artifact tests (1 ignored), 449 depgraph tests, and 766
  active daemon-core tests (25 ignored), plus focused merge/gating/loader
  regressions, formatting, diff checks, and warnings-denied Clippy.
- Pending: Unix ignored integration proof.
- clud-review: clean after cached-error rehydration and panic-free split
  follow-ups (one reviewer).
