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
