# Crates Architecture

24 crates split into two product surfaces: the **compile cache** and a separate **download cache**, plus utility binaries (`zccache-fp`, `zccache-stamp`) and one CI lib (`zccache-ci`).

> [!NOTE]
> **Binary layout (#997–#1000).** Release archives and wheels ship `zccache` plus the intentionally separate `zccache-fp`. `zccache` is a **multi-call binary**: copies named `zccache-daemon` and `zccache-download-daemon` dispatch to their respective daemon entry points. Both daemons are self-deployed beneath `~/.zccache/v<VERSION>/`; their legacy `[[bin]]` targets remain buildable as transitional source/test shims but are not distribution artifacts. `crates/zccache-cli` is **not** the CLI — it is the PyO3 `cdylib` hosting `zccache._native`. See [docs/architecture/runtime.md § Standalone daemon identity, deployment & lifecycle](../docs/architecture/runtime.md#standalone-daemon-identity-deployment--lifecycle).

## Dependency Graph

```
APPLICATION BINARIES
────────────────────
zccache (bins "zccache", "zccache-download")  ──┐
  the shipped multi-call binary; CLI surface lives │
  in zccache-cli-core                              │
                                                      │
zccache-cli (PyO3 cdylib, NOT the CLI)  ┤  hosts zccache._native
                                                      │
zccache-cli-core ──────────────────────┤
  the CLI subcommands + wrapper mode, plus the       │
  download_client / download_daemon modules behind   │
  the download-client / download-daemon features     │
  deps: artifact, compiler, core, hash, ipc, protocol,
        daemon-core, download, download-protocol,     │
        gha, symbols                                  │
                                                      │
zccache-daemon-core ───────────────────┤
  deps: artifact, compiler, core, hash, ipc, protocol,
        fscache, watcher, depgraph, fingerprint,      │
        test-support (dev only)                       │
                                                      │
SIDECAR BINARIES                                      │
────────────────                                      │
zccache-fp (in zccache-fingerprint)  ┤  deps: core, hash
zccache-stamp (in zccache-symbols)   ┤  deps: core
                                                      │
COMPILE-CACHE SUBSYSTEM LIBS                          │
────────────────────────────                          │
zccache-artifact ───── hash ──── core
zccache-compiler ──── hash
zccache-fscache ───── core
zccache-watcher ───── fscache
zccache-depgraph ──── hash, core
zccache-fingerprint ── hash, core
zccache-protocol ──── core
zccache-ipc ──────── protocol, core
                                                      │
zccache-platform ── (dependency leaf: no zccache-* deps;
                      host mechanics behind neutral facades)
                                                      │
DOWNLOAD-CACHE SUBSYSTEM LIBS                         │
─────────────────────────────                         │
zccache-download ──── core
zccache-download-protocol ─── download, core
(the download client/daemon/CLI are NOT separate crates -- they are
 modules of zccache-cli-core and bins of zccache; see the note above)
                                                      │
SHARED FOUNDATIONS                                    │
──────────────────                                    │
zccache-core   (Error/Result, Config, NormalizedPath)
zccache-hash   (blake3 ContentHash, CacheKeyBuilder)
                                                      │
OTHER                                                 │
─────                                                 │
zccache-gha          (lib, no internal deps)
zccache-symbols      (lib + zccache-stamp bin)
zccache-ci           (lib, used by Stop hook — core, ipc)
zccache-test-support (dev-only test utilities)
```

## Crate Responsibilities

### Shared foundations
- **zccache-core** — Shared error types (`Error`/`Result`), `Config`, `NormalizedPath` for cross-platform path handling
- **zccache-hash** — `ContentHash` (blake3), `CacheKeyBuilder` with domain-separated deterministic hashing
- **zccache-platform** — Dependency-leaf crate for host-platform mechanics (#1365): one `cfg_select!` host selector in its `lib.rs` and five neutral facades (`process`, `fs`, `ipc`, `executable`, `host`). Every consuming crate aliases it as `crate::platform`; concrete `platform_{win,linux,macos}` trees are private. Never depends on a zccache crate and never contains product types. Amalgamated as a private `platform` module of the published `zccache` crate.

### Compile-cache subsystem libs
- **zccache-protocol** — `Request`/`Response` enums, `ArtifactData`, length-prefixed prost framing; bump `PROTOCOL_VERSION` on any wire-format change
- **zccache-ipc** — Platform IPC endpoint discovery (`default_endpoint()`: Unix sockets vs named pipes)
- **zccache-fscache** — `MetadataCache` (DashMap-backed) with `Confidence` levels and time-based decay
- **zccache-artifact** — Content-addressed disk store with 2-level hex sharding, in-memory index snapshotted to a bincode blob (`index.bin`) for LRU eviction; also Rust-plan bundle save/restore
- **zccache-watcher** — `FileWatcher` trait over notify crate; dedicated OS thread, events via tokio channel
- **zccache-compiler** — `CompilerFamily` detection, `ParsedInvocation` for cacheability checks (clang/gcc/msvc/rustc/clang-cl), plus `parse_linker`, `parse_archiver`, `parse_msvc`, `parse_rustfmt`, `response_file`, `strict_paths`, `arduino` submodules
- **zccache-depgraph** — Persistent dependency graph for cache invalidation; snapshot save/load, dep walker
- **zccache-fingerprint** — File fingerprinting engine + `zccache-fp` CLI for inspecting/marking fingerprints
- **zccache-watcher-py** — the PyO3 `cdylib` for `zccache.watcher._native`. A separate crate because cargo cannot make a crate-type conditional: a `cdylib` alongside `rlib` is built by every consumer that only wants the rlib, and under `+crt-static` that link fails (#1497). `[lib] name` keeps the output filename unchanged.
- **zccache-fingerprint-py** — the PyO3 `cdylib` for `zccache.fingerprint._native`; same split as `zccache-watcher-py`, same reason.

### Compile-cache application binaries
- **zccache** — the transitional absorber crate (#365) and the home of every shipped `[[bin]]`. `zccache` itself is the multi-call binary; `zccache-daemon`, `zccache-download-daemon`, and `zccache-ci` (the Stop-hook process/thread dumper) are **bin targets of this crate**, not crates of their own.
- **zccache-daemon-core** — Tokio async runtime, IPC server, orchestrates all compile-cache subsystems (#1018 crate split). Reached through the `zccache` multicall binary's daemon entry.
- **zccache-cli-core** — the CLI subsystem (#1022 Split A): subcommands (start/stop/status/clear/analyze/warm/session/snapshot/cargo-registry/gha/rust-plan/fp/symbols/fetch), compiler wrapper mode, daemon lifecycle, GHA + Rust-plan save/restore, plus the `download_client` / `download_daemon` modules.
- **zccache-cli** — **not** the CLI. PyO3 `rlib`+`cdylib` hosting `zccache._native` for the Python package.

### Download-cache (separate daemon for fetching cached artifact archives)
- **zccache-download** — Core download engine and types
- **zccache-download-protocol** — IPC protocol for download daemon
- **download client / daemon / CLI** — *not* separate crates. `download_client`
  and `download_daemon` are modules of `zccache-cli-core` behind the
  `download-client` / `download-daemon` features, and the `zccache-download`
  binary is a `[[bin]]` of the `zccache` crate.

### Other
- **zccache-symbols** — Release-build marker, symbol cache, and symbol-archive fetcher; ships `zccache-stamp` CI helper
- **zccache-gha** — GitHub Actions Cache API client (used by both daemons for shared caching)
- **zccache-audit** — Durable audit schema types for embedded zccache integrations
- **zccache-compile-trace** — Per-sub-phase JSONL trace inside the embedded compile path
- **zccache-test-support** — Shared test utilities (dev-dependency only)

## Key Design Patterns

**Correctness model (layered invalidation):** Watcher events set confidence to Medium, never High. `lookup_since()` has a fast path (one stat, zero hash) that checks `(mtime, size)` against the cached entry even when the journal says "no changes"; `metadata.lookup()` is the full stat-verify + hash fallback. Content hashing is ground truth. A wrong cache hit is catastrophic; an extra stat is cheap.

**IPC:** Unix domain sockets on Linux/macOS, named pipes on Windows, behind a transport trait. Messages are length-prefixed prost. Daemon is lazily started by CLI if not running.

**File identity:** Tracked as (path, file_id) where file_id = inode on Unix, nFileIndex on Windows. Catches file replacement even when mtime is unchanged.

**Cache keys:** blake3 hash of: compiler identity + sorted args + sorted env vars + source content hash + dependency hashes. Domain separation tag "zccache-cache-key-v1".

**Concurrency:** Tokio tasks for IPC, DashMap for metadata cache (sharded lock-free reads), DashMap for the artifact index (disk I/O only in the background WAL writer's `flush()`), file watcher on dedicated OS thread.

## File-size discipline

No source file > 1,000 LOC. Enforced by `ci/hooks/loc_guard.py` (warns >1K, blocks >1.5K). When a file approaches the cap, convert it to a directory module: `foo.rs` → `foo/mod.rs` + per-domain files alongside, with tests in a `tests/` subdirectory. Re-export `pub` items from `mod.rs` so the public path is unchanged. Precedents: PRs #355–#363 split server.rs, cli/main.rs, perf_bench_test.rs, compiler/lib.rs, server/{tests,mod}.rs, compile_journal.rs, and depgraph/snapshot.rs.
