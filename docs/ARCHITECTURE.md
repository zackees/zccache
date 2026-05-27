# zccache Architecture

This document is the index for zccache's architecture specification. Each subsystem is documented in a separate file under `docs/architecture/`.

## Documents

| Document | Lines | What it covers |
|---|---|---|
| [architecture/overview.md](architecture/overview.md) | ~280 | System diagram, all 9 component descriptions and key interfaces |
| [architecture/data-flow.md](architecture/data-flow.md) | ~120 | Cache hit, cache miss, and passthrough step-by-step traces |
| [architecture/ipc.md](architecture/ipc.md) | ~60 | Transport abstraction, socket discovery, connection lifecycle, errors |
| [architecture/metadata-cache.md](architecture/metadata-cache.md) | ~130 | In-memory cache data model, confidence levels, watcher integration |
| [architecture/artifact-store.md](architecture/artifact-store.md) | ~130 | Disk layout, redb index schema, LRU eviction, corruption detection |
| [architecture/rust-artifact-plan.md](architecture/rust-artifact-plan.md) | ~120 | Rust plan ownership, thin/full semantics, restore hardening, backends, diagnostics, CLI contract |
| [architecture/target-cache.md](architecture/target-cache.md) | ~70 | Legacy action target snapshot ownership, outputs, and rust-plan boundary |
| [architecture/runtime.md](architecture/runtime.md) | ~130 | Concurrency model, correctness guarantees, failure modes, crash recovery |
| [architecture/portability.md](architecture/portability.md) | ~110 | Platform differences, path handling, file identity, future extensions |

## Quick Reference

- **High-level design** → [overview.md](architecture/overview.md)
- **"How does a cache hit work?"** → [data-flow.md](architecture/data-flow.md)
- **CLI↔daemon communication** → [ipc.md](architecture/ipc.md)
- **File change detection** → [metadata-cache.md](architecture/metadata-cache.md)
- **Disk cache & eviction** → [artifact-store.md](architecture/artifact-store.md)
- **soldr target artifact contract** → [rust-artifact-plan.md](architecture/rust-artifact-plan.md)
- **Legacy action target snapshots** → [target-cache.md](architecture/target-cache.md)
- **Thread safety & crash safety** → [runtime.md](architecture/runtime.md)
- **Where zccache writes on disk (`ZCCACHE_CACHE_DIR` contract)** → [runtime.md § Cache root invariants](architecture/runtime.md#cache-root-invariants)
- **Windows/macOS/Linux differences** → [portability.md](architecture/portability.md)
- **Compile journal fields & `miss_reason` enum** → [journal-schema.md](journal-schema.md)

See also: [DESIGN_DECISIONS.md](DESIGN_DECISIONS.md) for rationale behind key choices, [ROADMAP.md](ROADMAP.md) for implementation phases.
