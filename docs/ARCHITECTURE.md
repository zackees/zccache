# zccache Architecture

This document is the index for zccache's architecture specification. Each subsystem is documented in a separate file under `docs/architecture/`.

## Documents

| Document | Lines | What it covers |
|---|---|---|
| [architecture/overview.md](architecture/overview.md) | ~280 | System diagram, all 9 component descriptions and key interfaces |
| [architecture/data-flow.md](architecture/data-flow.md) | ~160 | Cache hit, cache miss, passthrough traces + rustc key-scope rules (env-deps, incremental, crate types) |
| [architecture/ipc.md](architecture/ipc.md) | ~60 | Transport abstraction, socket discovery, connection lifecycle, errors |
| [architecture/metadata-cache.md](architecture/metadata-cache.md) | ~130 | In-memory cache data model, confidence levels, watcher integration |
| [architecture/artifact-store.md](architecture/artifact-store.md) | ~130 | Disk layout, redb index schema, LRU eviction, corruption detection |
| [architecture/rust-artifact-plan.md](architecture/rust-artifact-plan.md) | ~120 | Rust plan ownership, thin/full semantics, restore hardening, backends, diagnostics, CLI contract |
| [architecture/embedded-service.md](architecture/embedded-service.md) | ~290 | Embedded service MVP boundary, audit continuity, soldr/fbuild integration design |
| [architecture/target-cache.md](architecture/target-cache.md) | ~70 | Legacy action target snapshot ownership, outputs, and rust-plan boundary |
| [architecture/runtime.md](architecture/runtime.md) | ~130 | Concurrency model, correctness guarantees, failure modes, crash recovery |
| [architecture/portability.md](architecture/portability.md) | ~110 | Platform differences, path handling, file identity, future extensions |

## Quick Reference

- **High-level design** → [overview.md](architecture/overview.md)
- **"How does a cache hit work?"** → [data-flow.md](architecture/data-flow.md)
- **CLI↔daemon communication** → [ipc.md](architecture/ipc.md)
- **File change detection** → [metadata-cache.md](architecture/metadata-cache.md)
- **Disk cache & eviction** → [artifact-store.md](architecture/artifact-store.md)
- **Transactional directory outputs** → [artifact-store.md](architecture/artifact-store.md#immutable-staged-output-rollout)
- **Reflink / hardlink COW safety** → [artifact-store.md](architecture/artifact-store.md#capability-driven-cow-materialization)
- **soldr target artifact contract** → [rust-artifact-plan.md](architecture/rust-artifact-plan.md)
- **Embedded soldr/fbuild service integration** → [embedded-service.md](architecture/embedded-service.md)
- **Legacy action target snapshots** → [target-cache.md](architecture/target-cache.md)
- **Thread safety & crash safety** → [runtime.md](architecture/runtime.md)
- **Async/process bridge — watchdogs, cancellation & timeouts (deadlock hardening)** → [runtime.md § Async / process bridge](architecture/runtime.md#async--process-bridge-watchdogs-cancellation--timeouts)
- **Where zccache writes on disk (`ZCCACHE_CACHE_DIR` contract)** → [runtime.md § Cache root invariants](architecture/runtime.md#cache-root-invariants)
- **Host no-spawn guard (`ZCCACHE_NO_SPAWN`, embedding hosts)** → [runtime.md § Host no-spawn guard](architecture/runtime.md#host-no-spawn-guard-zccache_no_spawn)
- **Standalone daemon identity, deployment & lifecycle (argv[0] single binary, version-rooted deploy, versioned endpoints)** → [runtime.md § Standalone daemon identity, deployment & lifecycle](architecture/runtime.md#standalone-daemon-identity-deployment--lifecycle)
- **Windows/macOS/Linux differences** → [portability.md](architecture/portability.md)
- **Compile journal fields & `miss_reason` enum** → [journal-schema.md](journal-schema.md)

See also: [DESIGN_DECISIONS.md](DESIGN_DECISIONS.md) for rationale behind key choices, [ROADMAP.md](ROADMAP.md) for implementation phases.
For embedded mode, start with [architecture/embedded-service.md](architecture/embedded-service.md), especially the MVP status section for the landed documentation contract versus open soldr/fbuild integration work.
