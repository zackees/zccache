# Crates Architecture

21 crates split into two product surfaces: the **compile cache** and a separate **download cache**, plus utility binaries (`zccache-fp`, `zccache-stamp`) and one CI lib (`zccache-ci`).

> [!NOTE]
> **Binary layout (post-consolidation, #997–#999).** The compile-cache binaries all live as `[[bin]]` targets in the single `crates/zccache` crate — `zccache` (CLI), `zccache-daemon` (the standalone daemon; its `main` is the library `daemon::entry`), and `zccache-fp`. As of #998 the `zccache` binary is a **multi-call binary**: it dispatches on `argv[0]` and runs the daemon when invoked as `zccache-daemon`, so the CLI **self-deploys** the daemon by copying itself to `~/.zccache/v<VERSION>/zccache-daemon` (#999) rather than shipping a separate daemon executable. `crates/zccache-cli` is **not** the CLI — it is the PyO3 `cdylib` hosting `zccache._native`. See [docs/architecture/runtime.md § Standalone daemon identity, deployment & lifecycle](../docs/architecture/runtime.md#standalone-daemon-identity-deployment--lifecycle).

## Dependency Graph

```
APPLICATION BINARIES
────────────────────
zccache-cli (bin "zccache")  ──┐
  deps: artifact, compiler, core, hash, ipc, protocol,
        download, download-client, gha, symbols      │
                                                      │
zccache-daemon (bin)  ─────────┤
  deps: artifact, compiler, core, hash, ipc, protocol,
        fscache, watcher, depgraph, fingerprint,      │
        test-support (dev only)                       │
                                                      │
zccache-download-daemon (bin)  ┤  deps: core, ipc, download, download-protocol
zccache-download-cli (bin "zccache-download")  ┤  deps: download, download-client
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
DOWNLOAD-CACHE SUBSYSTEM LIBS                         │
─────────────────────────────                         │
zccache-download ──── core
zccache-download-protocol ─── download, core
zccache-download-client  ──── download, download-protocol,
                              download-daemon, core, ipc
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

### Compile-cache subsystem libs
- **zccache-protocol** — `Request`/`Response` enums, `ArtifactData`, length-prefixed bincode framing; bump `PROTOCOL_VERSION` on any wire-format change
- **zccache-ipc** — Platform IPC endpoint discovery (`default_endpoint()`: Unix sockets vs named pipes)
- **zccache-fscache** — `MetadataCache` (DashMap-backed) with `Confidence` levels and time-based decay
- **zccache-artifact** — Content-addressed disk store with 2-level hex sharding, redb index for LRU eviction; also Rust-plan bundle save/restore
- **zccache-watcher** — `FileWatcher` trait over notify crate; dedicated OS thread, events via tokio channel
- **zccache-compiler** — `CompilerFamily` detection, `ParsedInvocation` for cacheability checks (clang/gcc/msvc/rustc/clang-cl), plus `parse_linker`, `parse_archiver`, `parse_msvc`, `parse_rustfmt`, `response_file`, `strict_paths`, `arduino` submodules
- **zccache-depgraph** — Persistent dependency graph for cache invalidation; snapshot save/load, dep walker
- **zccache-fingerprint** — File fingerprinting engine + `zccache-fp` CLI for inspecting/marking fingerprints

### Compile-cache application binaries
- **zccache-daemon** — Tokio async runtime, IPC server, orchestrates all compile-cache subsystems
- **zccache-cli** — `zccache` binary: subcommands (start/stop/status/clear/analyze/warm/session/snapshot/cargo-registry/gha/rust-plan/fp/symbols), compiler wrapper mode, daemon lifecycle, GHA + Rust-plan save/restore

### Download-cache (separate daemon for fetching cached artifact archives)
- **zccache-download** — Core download engine and types
- **zccache-download-protocol** — IPC protocol for download daemon
- **zccache-download-client** — Rust client API for the download daemon
- **zccache-download-daemon** — Per-user `zccache-download-daemon` binary
- **zccache-download-cli** — `zccache-download` CLI binary

### Other
- **zccache-symbols** — Release-build marker, symbol cache, and symbol-archive fetcher; ships `zccache-stamp` CI helper
- **zccache-gha** — GitHub Actions Cache API client (used by both daemons for shared caching)
- **zccache-ci** — Stop-hook helper (process/thread dumping) run after every Claude Code Stop event
- **zccache-test-support** — Shared test utilities (dev-dependency only)

## Key Design Patterns

**Correctness model (layered invalidation):** Watcher events set confidence to Medium, never High. `lookup_since()` has a fast path (one stat, zero hash) that checks `(mtime, size)` against the cached entry even when the journal says "no changes"; `metadata.lookup()` is the full stat-verify + hash fallback. Content hashing is ground truth. A wrong cache hit is catastrophic; an extra stat is cheap.

**IPC:** Unix domain sockets on Linux/macOS, named pipes on Windows, behind a transport trait. Messages are length-prefixed bincode. Daemon is lazily started by CLI if not running.

**File identity:** Tracked as (path, file_id) where file_id = inode on Unix, nFileIndex on Windows. Catches file replacement even when mtime is unchanged.

**Cache keys:** blake3 hash of: compiler identity + sorted args + sorted env vars + source content hash + dependency hashes. Domain separation tag "zccache-cache-key-v1".

**Concurrency:** Tokio tasks for IPC, DashMap for metadata cache (sharded lock-free reads), redb MVCC for artifact index, file watcher on dedicated OS thread.

## File-size discipline

No source file > 1,000 LOC. Enforced by `ci/hooks/loc_guard.py` (warns >1K, blocks >1.5K). When a file approaches the cap, convert it to a directory module: `foo.rs` → `foo/mod.rs` + per-domain files alongside, with tests in a `tests/` subdirectory. Re-export `pub` items from `mod.rs` so the public path is unchanged. Precedents: PRs #355–#363 split server.rs, cli/main.rs, perf_bench_test.rs, compiler/lib.rs, server/{tests,mod}.rs, compile_journal.rs, and depgraph/snapshot.rs.
