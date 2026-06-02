# Documentation Guide

Architecture docs are split by subsystem. Read only what's relevant to your current work.

## Which doc to read

| Working on crate | Read these |
|---|---|
| `zccache-cli` | [overview.md](architecture/overview.md) (2.1), [data-flow.md](architecture/data-flow.md) |
| `zccache-daemon` | [overview.md](architecture/overview.md) (2.2), [runtime.md](architecture/runtime.md) |
| `zccache-ipc` | [overview.md](architecture/overview.md) (2.3), [ipc.md](architecture/ipc.md) |
| `zccache-protocol` | [overview.md](architecture/overview.md) (2.4), [ipc.md](architecture/ipc.md) |
| `zccache-fscache` | [metadata-cache.md](architecture/metadata-cache.md) |
| `zccache-watcher` | [metadata-cache.md](architecture/metadata-cache.md) (watcher section) |
| `zccache-artifact` | [artifact-store.md](architecture/artifact-store.md) |
| `zccache-hash` | [overview.md](architecture/overview.md) (2.8) |
| `zccache-compiler` | [overview.md](architecture/overview.md) (2.9), [data-flow.md](architecture/data-flow.md) |
| `zccache-core` | [overview.md](architecture/overview.md) |
| `zccache-symbols` | Crate README — 128-byte release footer, `<dump>.symref` sidecars, `zccache-stamp` CI helper |
| Perf measurement workflows | [/PERF.md](../PERF.md) — `perf-rust-cluster.yml`, scenarios, gate semantics |
| Crash dumper (CLI + daemon) | [runtime.md](architecture/runtime.md) (Crash Dumper) — `zccache_core::crash::install` covers both binaries |
| Generic tool exec (`zccache exec`, issue #272) | [runtime.md § Generic tool exec](architecture/runtime.md#generic-tool-exec-zccache-exec) — `handle_exec.rs` + `cli/commands/exec.rs` |
| Platform-specific issues | [portability.md](architecture/portability.md) |
| Compile journal record shape | [journal-schema.md](journal-schema.md) |
| `ZCCACHE_CACHE_DIR` contract + `zccache cache-root` | [runtime.md § Cache root invariants](architecture/runtime.md#cache-root-invariants) |

## Where to document new features

Feature documentation belongs in the **subsystem doc that owns the feature**, not in a top-level monolith. This preserves progressive disclosure — the agent only loads what's relevant.

**Rules:**
1. **Write docs in the subsystem file.** A new daemon feature goes in [runtime.md](architecture/runtime.md), a new IPC message in [ipc.md](architecture/ipc.md), etc. Use the "Which doc to read" table above to find the right home.
2. **Add a breadcrumb in the parent index.** After adding content to a subsystem doc, add a one-line entry in [ARCHITECTURE.md](ARCHITECTURE.md) or the "Quick Reference" section so the agent can discover it from the hierarchy. The breadcrumb is a pointer, not a summary — keep it under 15 words.
3. **Cross-cutting features get a row in the table above.** If a feature spans multiple crates, add a row to "Which doc to read" pointing at the relevant docs so the agent knows where to look.
4. **Never duplicate content across docs.** If two docs need the same info, one doc owns it and the other links to it with a breadcrumb.

**Goal:** An agent starting from `CLAUDE.md` → `docs/CLAUDE.md` → subsystem doc can find any feature in at most 3 hops. Each hop adds detail; no hop requires reading unrelated content.

## Other docs

- **[DESIGN_DECISIONS.md](DESIGN_DECISIONS.md)** — 25 ADR-style decisions with rationale
- **[ROADMAP.md](ROADMAP.md)** — 7 implementation phases
- **[ARCHITECTURE.md](ARCHITECTURE.md)** — Index of all architecture documents
