# Architecture Documents

Detailed design specification for zccache, split by subsystem.

| Document | Covers |
|---|---|
| [overview.md](overview.md) | System diagram, all 9 component descriptions |
| [data-flow.md](data-flow.md) | Cache hit/miss/passthrough step-by-step traces |
| [ipc.md](ipc.md) | Transport abstraction, socket discovery, connection lifecycle |
| [metadata-cache.md](metadata-cache.md) | In-memory cache, confidence model, file watcher integration |
| [artifact-store.md](artifact-store.md) | Disk layout, redb index, eviction, corruption detection |
| [rust-artifact-plan.md](rust-artifact-plan.md) | Rust plan ownership, thin/full semantics, backends, diagnostics, CLI contract |
| [embedded-service.md](embedded-service.md) | Embedded host-daemon API, audit continuity, soldr/fbuild integration contract |
| [vendored-hotfix-workflow.md](vendored-hotfix-workflow.md) | Branch → host-pin → validate → upstream → release loop for embedded-host bug fixes (zccache#909) |
| [runtime.md](runtime.md) | Concurrency, correctness model, failure modes, crash recovery |
| [portability.md](portability.md) | Platform differences, path handling, future extensions |
